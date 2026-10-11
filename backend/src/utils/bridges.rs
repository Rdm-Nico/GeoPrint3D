//! Bridges: decks, piers, towers and removable print supports.
//!
//! A lean model, chosen after a heavier first attempt printed mostly flush
//! slabs that did not read as bridges. A deck is a strip extruded along its
//! OSM way, anchored to the RENDERED terrain at its abutments and interpolated
//! in between, so the clearance over a river comes from the banks (as in the
//! reference prints of Bern). A bridge is emitted only when it is recognisable
//! in print: long enough, wide enough, and with a visible gap under the deck.
//!
//! Scales follow the buildings: XY is true scale (widened to a printable
//! minimum); the deck height lives in terrain units because it must meet the
//! banks; vertical sizes of its own — deck thickness, towers — use the
//! building exaggeration E, towers through the same soft-knee compression.
//!
//! Supports: mapped piers / pylons / bridge towers (`bridge:support`,
//! `man_made=tower` + `tower:type=bridge`) are clustered into printable
//! blocks — the role comes from the tags only. Where none are mapped, default
//! piers are spaced along the open spans (as OSM2World's BridgeModule does).
//! Spans longer than FDM can bridge get breakaway columns (a "pre-supported"
//! model) in their own triangle range, so the file prints without any manual
//! support placement in the slicer.
//!
//! Phase A (`BridgePlan::build`, real metres) runs before the terrain mesh;
//! phase B (`BridgePlan::solve`, mesh units) samples the rendered surface.

use crate::models::{BridgeFeatures, BridgeKind, Building, ProjectedPoint, SupportRole, TerrainMesh};
use crate::services::height_inference::soft_knee;
use crate::utils::hydro::node_key;
use crate::utils::mesh::{solid_is_closed, TerrainSurface};
use crate::utils::projection::CoordinateProjector;
use anyhow::Result;
use std::collections::{BTreeMap, HashMap};

// ── Printed sizes (mm) ────────────────────────────────────────────────────────
const MIN_DECK_WIDTH_MM: f64 = 0.8; // two 0.4 mm extrusion lines
const MIN_DECK_THICK_MM: f64 = 0.8;
const DECK_THICK_M: f64 = 2.0; // real structural depth, × E
const GAP_MM: f64 = 1.0; // smallest gap under a deck that prints and reads
const EMBED_MM: f64 = 0.2; // how far solids sink into what they stand on
const END_EXTENSION_MM: f64 = 0.3; // deck ends run into the bank
const STATION_STEP_MM: f64 = 0.5;
// Recognisability.
const MIN_BRIDGE_LEN_MM: f64 = 4.0;
const MIN_REAL_WIDTH_MM: f64 = 0.3; // narrower decks need > ~2.7× widening…
const LONG_BRIDGE_MM: f64 = 10.0; // …unless the bridge is this long
const MIN_OPEN_RUN_MM: f64 = 2.0;
// Lift over water where the banks are too low for a visible gap.
const LIFT_CLEARANCE_M: f64 = 8.0; // real clearance, × E
const LIFT_SLOPE: f64 = 0.3; // printed rise per printed run (~17°)
// Piers and towers.
const PIER_SPACING_MM: f64 = 6.0;
const PIER_LEN_MM: f64 = 1.0;
const PIER_WIDTH_FRAC: f64 = 0.8;
const MIN_PIER_MM: f64 = 0.8;
const MIN_TOWER_MM: f64 = 1.2;
const TOWER_ABOVE_DECK_MM: f64 = 1.0; // a tower always shows above its deck
const UNTAGGED_TOWER_ABOVE_DECK_MM: f64 = 2.0;
const OVERLAP_MM: f64 = 0.05; // piers reach into the deck
// Breakaway columns ("pre-supported" model).
const MAX_UNSUPPORTED_MM: f64 = 10.0; // longest span FDM bridges reliably
const BREAKAWAY_SPACING_MM: f64 = 6.0;
const BREAKAWAY_SIZE_MM: f64 = 1.2;
const BREAKAWAY_NECK_MM: f64 = 0.8; // two lines: thinner bases get dropped
const BREAKAWAY_FLARE_MM: f64 = 0.6;
/// Air gap between a breakaway column and the deck: one 0.2 mm layer, like a
/// slicer's support Z distance. Assumes 0.2 mm layers.
pub const BREAKAWAY_TOP_GAP_MM: f64 = 0.2;
// Grouping.
const POOL_ABUTMENT_MM: f64 = 1.5; // parallel decks share their abutment height
// Supports merge into one block only when they (nearly) touch: the slabs,
// legs and crossbeams of one tower do, separate piers of a viaduct do not.
const SUPPORT_CLUSTER_M: f64 = 3.0;
const SUPPORT_REACH_MM: f64 = 1.0;
const SUPPORT_REACH_M: f64 = 10.0;
const MAX_BEND_DEG: f64 = 45.0; // sharper bends split the strip (no miter spikes)

type Pt = (f64, f64);
type Key = (u64, u64);

// ── Small geometry helpers ────────────────────────────────────────────────────

fn sub(a: Pt, b: Pt) -> Pt {
    (a.0 - b.0, a.1 - b.1)
}
fn add(a: Pt, b: Pt) -> Pt {
    (a.0 + b.0, a.1 + b.1)
}
fn mul(a: Pt, k: f64) -> Pt {
    (a.0 * k, a.1 * k)
}
fn dot(a: Pt, b: Pt) -> f64 {
    a.0 * b.0 + a.1 * b.1
}
fn len(a: Pt) -> f64 {
    dot(a, a).sqrt()
}
fn unit(a: Pt) -> Pt {
    let l = len(a);
    if l > 1e-12 {
        (a.0 / l, a.1 / l)
    } else {
        (1.0, 0.0)
    }
}
/// Left normal of a direction.
fn perp(a: Pt) -> Pt {
    (-a.1, a.0)
}

fn point_in_ring(p: Pt, ring: &[Pt]) -> bool {
    let mut inside = false;
    let n = ring.len();
    for i in 0..n {
        let (a, b) = (ring[i], ring[(i + 1) % n]);
        if (a.1 > p.1) != (b.1 > p.1) {
            let x = a.0 + (p.1 - a.1) / (b.1 - a.1) * (b.0 - a.0);
            if p.0 < x {
                inside = !inside;
            }
        }
    }
    inside
}

/// Distance from p to a polyline and the arc length of the closest point.
fn project_on_polyline(p: Pt, pts: &[Pt]) -> (f64, f64) {
    let (mut best_d, mut best_s, mut acc) = (f64::INFINITY, 0.0, 0.0);
    for w in pts.windows(2) {
        let d = sub(w[1], w[0]);
        let l2 = dot(d, d);
        let t = if l2 > 0.0 { (dot(sub(p, w[0]), d) / l2).clamp(0.0, 1.0) } else { 0.0 };
        let q = add(w[0], mul(d, t));
        let dist = len(sub(p, q));
        if dist < best_d {
            best_d = dist;
            best_s = acc + t * l2.sqrt();
        }
        acc += l2.sqrt();
    }
    (best_d, best_s)
}

fn polyline_len(pts: &[Pt]) -> f64 {
    pts.windows(2).map(|w| len(sub(w[1], w[0]))).sum()
}

/// Liang–Barsky clip of a polyline to an axis-aligned rectangle
/// [x0, y0, x1, y1]. Returns the inside pieces and, per piece, whether its
/// start / end was created by the cut (not an original vertex).
fn clip_polyline(pts: &[Pt], r: [f64; 4]) -> Vec<(Vec<Pt>, bool, bool)> {
    let mut out = Vec::new();
    let mut cur: Vec<Pt> = Vec::new();
    let mut cur_cut_start = false;
    for (i, w) in pts.windows(2).enumerate() {
        let (a, b) = (w[0], w[1]);
        let d = sub(b, a);
        let (mut t0, mut t1) = (0.0f64, 1.0f64);
        let mut visible = true;
        for (p, q) in [(-d.0, a.0 - r[0]), (d.0, r[2] - a.0), (-d.1, a.1 - r[1]), (d.1, r[3] - a.1)] {
            if p.abs() < 1e-15 {
                if q < 0.0 {
                    visible = false;
                    break;
                }
            } else {
                let t = q / p;
                if p < 0.0 {
                    t0 = t0.max(t);
                } else {
                    t1 = t1.min(t);
                }
            }
        }
        if !visible || t0 > t1 {
            if cur.len() >= 2 {
                out.push((std::mem::take(&mut cur), cur_cut_start, true));
            }
            cur.clear();
            continue;
        }
        if cur.is_empty() {
            cur.push(add(a, mul(d, t0)));
            cur_cut_start = t0 > 0.0 || i > 0;
        }
        cur.push(add(a, mul(d, t1)));
        if t1 < 1.0 {
            out.push((std::mem::take(&mut cur), cur_cut_start, true));
        }
    }
    if cur.len() >= 2 {
        out.push((cur, cur_cut_start, false));
    }
    out
}

fn find(parent: &mut [usize], mut x: usize) -> usize {
    while parent[x] != x {
        parent[x] = parent[parent[x]];
        x = parent[x];
    }
    x
}
fn union(parent: &mut [usize], a: usize, b: usize) {
    let (ra, rb) = (find(parent, a), find(parent, b));
    if ra != rb {
        parent[ra] = rb;
    }
}

// ── Stacked bases go back to the building path ────────────────────────────────

/// A building-tagged support whose footprint carries stacked `building:part`
/// elements (Tower Bridge's towers stand on `bridge:support=pier` bases) goes
/// back to the building list, so the parts keep the base they adopt today.
/// It stays in the support list marked `as_building`: its bridge still counts
/// as mapped (no invented piers) and it still bounds the spans.
/// Returns how many supports were handed back.
pub fn hand_back_stacked_bases(bridges: &mut BridgeFeatures, buildings: &mut Vec<Building>) -> usize {
    let centroids: Vec<Pt> = buildings
        .iter()
        .filter(|b| b.is_part && b.min_height >= 0.5 && !b.footprint.is_empty())
        .map(|b| {
            let n = b.footprint.len() as f64;
            (b.footprint.iter().map(|p| p.0).sum::<f64>() / n, b.footprint.iter().map(|p| p.1).sum::<f64>() / n)
        })
        .collect();
    if centroids.is_empty() {
        return 0;
    }
    let mut handed = 0;
    for s in bridges.supports.iter_mut() {
        if s.building.is_some() && centroids.iter().any(|&c| point_in_ring(c, &s.outer)) {
            buildings.push(s.building.take().unwrap());
            s.as_building = true;
            handed += 1;
        }
    }
    handed
}

// ── Phase A: planimetry in real metres ────────────────────────────────────────

/// One deck strip: a maximal chain of bridge ways (merged through nodes where
/// exactly two compatible ways meet), clipped to the terrain.
struct Piece {
    way_ids: Vec<u64>,
    kind: BridgeKind,
    layer: i8,
    width_m: f64,
    width_source: &'static str,
    name: Option<String>,
    structure: Option<String>,
    pts: Vec<Pt>,
    /// Graph node of each end.
    ends: [usize; 2],
    /// Stations every STATION_STEP (original vertices kept): (point, arc length).
    stations: Vec<(Pt, f64)>,
    length: f64,
    group: usize,
    /// Spine of the bundle of adjacent parallel decks this piece belongs to
    /// (carriageways, separately mapped sidewalks). Only spines get piers
    /// and breakaway columns, sized to the whole bundle.
    spine: usize,
    /// A narrow deck lying against a wider one: not widened to the printable
    /// minimum (the union with its neighbour already prints).
    attached: bool,
}

impl Piece {
    /// Deck width as printed (mesh units = metres in XY).
    fn w_eff(&self, min_w: f64) -> f64 {
        if self.attached {
            self.width_m
        } else {
            self.width_m.max(min_w)
        }
    }
}

struct Group {
    pieces: Vec<usize>,
    outline_id: Option<u64>,
    name: Option<String>,
    length_m: f64,
    /// A mapped support lies on one of its decks: no default piers.
    mapped: bool,
    skip: Option<&'static str>,
}

/// A cluster of mapped supports (all the building:part slabs of one tower).
struct Cluster {
    ids: Vec<u64>,
    pts: Vec<Pt>,
    role: SupportRole,
    height_m: Option<f64>,
    deck_relative: bool,
    /// Every member was handed back to the building path.
    as_building: bool,
    piece: Option<usize>,
}

pub struct BridgePlan {
    pieces: Vec<Piece>,
    groups: Vec<Group>,
    clusters: Vec<Cluster>,
    node_cut: Vec<bool>,
    node_pos: Vec<Pt>,
    /// Largest axis-aligned rectangle inside the terrain hull: every bridge
    /// vertex stays in it, so the print scale cannot change.
    rect: [f64; 4],
    /// Lowest x / y of the terrain grid: print coordinates are
    /// (x − origin) · mm_per_m after normalisation.
    origin: Pt,
    cell: f64,
    mm_per_m: f64,
    ways_outside: usize,
}

/// Input of phase A after the lon/lat merge.
struct Chain {
    way_ids: Vec<u64>,
    attrs: usize, // index of the way providing kind/width/name
    pts: Vec<Pt>, // lon/lat
}

impl BridgePlan {
    /// Build the planimetry from raw OSM bridge features. `points` is the
    /// projected (grid_dim)² DEM grid (row 0 south, column 0 west).
    pub fn build(
        features: &BridgeFeatures,
        projector: &CoordinateProjector,
        points: &[ProjectedPoint],
        grid_dim: usize,
        mm_per_m: f64,
    ) -> Result<Self> {
        anyhow::ensure!(grid_dim >= 2 && points.len() == grid_dim * grid_dim, "bridges need the regular DEM grid");
        let mm = |v: f64| v / mm_per_m; // printed mm → metres
        let at = |i: usize, j: usize| &points[i * grid_dim + j];
        let n = grid_dim - 1;
        let rect = [
            (0..grid_dim).map(|i| at(i, 0).x).fold(f64::MIN, f64::max),
            (0..grid_dim).map(|j| at(0, j).y).fold(f64::MIN, f64::max),
            (0..grid_dim).map(|i| at(i, n).x).fold(f64::MAX, f64::min),
            (0..grid_dim).map(|j| at(n, j).y).fold(f64::MAX, f64::min),
        ];
        let cell = {
            let d = |a: &ProjectedPoint, b: &ProjectedPoint| ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt();
            d(&points[0], &points[1]).max(d(&points[0], &points[grid_dim]))
        };
        let project = |p: Pt| -> Result<Pt> {
            let q = projector.project(p.0, p.1, 0.0)?;
            Ok((q.x, q.y))
        };

        // ── Chains in lon/lat (exact shared OSM nodes) ───────────────────────
        let chains = merge_ways(features);

        // ── Project, clip, nodes ─────────────────────────────────────────────
        let mut node_of: HashMap<Key, usize> = HashMap::new();
        let mut node_cut: Vec<bool> = Vec::new();
        let mut node_pos: Vec<Pt> = Vec::new();
        let mut new_node = |key: Option<Key>, pos: Pt, node_cut: &mut Vec<bool>, node_pos: &mut Vec<Pt>| -> usize {
            if let Some(k) = key {
                if let Some(&id) = node_of.get(&k) {
                    return id;
                }
                node_of.insert(k, node_cut.len());
            }
            node_cut.push(key.is_none());
            node_pos.push(pos);
            node_cut.len() - 1
        };
        let mut pieces: Vec<Piece> = Vec::new();
        let mut ways_outside = 0usize;
        for ch in &chains {
            let w = &features.ways[ch.attrs];
            let projected = ch.pts.iter().map(|&p| project(p)).collect::<Result<Vec<Pt>>>()?;
            let parts = clip_polyline(&projected, rect);
            if parts.is_empty() {
                ways_outside += ch.way_ids.len();
            }
            for (pts, cut_start, cut_end) in parts {
                let length = polyline_len(&pts);
                if length < 1e-3 {
                    continue;
                }
                let k0 = (!cut_start).then(|| node_key(ch.pts[0]));
                let k1 = (!cut_end).then(|| node_key(*ch.pts.last().unwrap()));
                let a = new_node(k0, pts[0], &mut node_cut, &mut node_pos);
                let b = new_node(k1, *pts.last().unwrap(), &mut node_cut, &mut node_pos);
                pieces.push(Piece {
                    way_ids: ch.way_ids.clone(),
                    kind: w.kind,
                    layer: w.layer,
                    width_m: w.width_m as f64,
                    width_source: w.width_source,
                    name: w.name.clone(),
                    structure: w.structure.clone(),
                    stations: Vec::new(),
                    length,
                    pts,
                    ends: [a, b],
                    group: 0,
                    spine: 0,
                    attached: false,
                });
            }
        }

        // ── Outlines: grouping, width hint, outline-only bridges ─────────────
        let mut parent: Vec<usize> = (0..pieces.len()).collect();
        let mut piece_outline: Vec<Option<usize>> = vec![None; pieces.len()];
        let mut outlines_m: Vec<(u64, Option<String>, Vec<Pt>)> = Vec::new();
        for o in &features.outlines {
            let ring = o.outer.iter().map(|&p| project(p)).collect::<Result<Vec<Pt>>>()?;
            if ring.len() >= 3 {
                outlines_m.push((o.id, o.name.clone(), ring));
            }
        }
        for (oi, (id, name, ring)) in outlines_m.iter().enumerate() {
            // A deck belongs to the outline when any part of it runs inside
            // (a long way can cross a small approach outline without a vertex
            // in it); the width hint needs its midpoint inside.
            let rb = ring.iter().fold([f64::MAX, f64::MAX, f64::MIN, f64::MIN], |b, p| {
                [b[0].min(p.0), b[1].min(p.1), b[2].max(p.0), b[3].max(p.1)]
            });
            let touches = |pc: &Piece| {
                stations(&pc.pts, 5.0).iter().any(|&(q, _)| {
                    q.0 >= rb[0] && q.0 <= rb[2] && q.1 >= rb[1] && q.1 <= rb[3] && point_in_ring(q, ring)
                })
            };
            let inside: Vec<usize> = (0..pieces.len()).filter(|&i| touches(&pieces[i])).collect();
            let centred: Vec<usize> = inside
                .iter()
                .copied()
                .filter(|&i| point_in_ring(point_at(&pieces[i].pts, pieces[i].length / 2.0), ring))
                .collect();
            let (axis_a, axis_b, area) = pca_axis(ring);
            let axis_len = len(sub(axis_b, axis_a)).max(1e-3);
            let outline_width = area / axis_len;
            match inside.len() {
                0 => {
                    // Outline-only bridge: its principal axis becomes the deck.
                    for (pts, cut_start, cut_end) in clip_polyline(&[axis_a, axis_b], rect) {
                        let length = polyline_len(&pts);
                        if length < 1e-3 {
                            continue;
                        }
                        let a = new_node(None, pts[0], &mut node_cut, &mut node_pos);
                        let b = new_node(None, pts[1], &mut node_cut, &mut node_pos);
                        // Fresh nodes are created as "cut"; the axis ends are
                        // real abutments unless the hull cut them.
                        node_cut[a] = cut_start;
                        node_cut[b] = cut_end;
                        parent.push(pieces.len());
                        piece_outline.push(Some(oi));
                        pieces.push(Piece {
                            way_ids: Vec::new(),
                            kind: BridgeKind::Road,
                            layer: features.outlines.iter().find(|o| o.id == *id).map_or(1, |o| o.layer),
                            width_m: outline_width.max(1.0),
                            width_source: "outline",
                            name: name.clone(),
                            structure: None,
                            stations: Vec::new(),
                            length,
                            pts,
                            ends: [a, b],
                            group: 0,
                            spine: 0,
                            attached: false,
                        });
                    }
                }
                _ => {
                    if inside.len() == 1 && centred.len() == 1 && outline_width > pieces[inside[0]].width_m {
                        pieces[inside[0]].width_m = outline_width;
                        pieces[inside[0]].width_source = "outline";
                    }
                    for &i in &inside {
                        piece_outline[i] = Some(oi);
                        union(&mut parent, inside[0], i);
                    }
                }
            }
        }

        // ── Groups: decks sharing a node, or inside one outline ──────────────
        let mut by_node: HashMap<usize, usize> = HashMap::new();
        for (i, p) in pieces.iter().enumerate() {
            for &e in &p.ends {
                if node_cut[e] {
                    continue;
                }
                match by_node.get(&e) {
                    Some(&j) => union(&mut parent, i, j),
                    None => {
                        by_node.insert(e, i);
                    }
                }
            }
        }
        let mut root_group: BTreeMap<usize, usize> = BTreeMap::new();
        let mut groups: Vec<Group> = Vec::new();
        for i in 0..pieces.len() {
            let r = find(&mut parent, i);
            let g = *root_group.entry(r).or_insert_with(|| {
                groups.push(Group {
                    pieces: Vec::new(),
                    outline_id: None,
                    name: None,
                    length_m: 0.0,
                    mapped: false,
                    skip: None,
                });
                groups.len() - 1
            });
            pieces[i].group = g;
            groups[g].pieces.push(i);
            if let Some(oi) = piece_outline[i] {
                groups[g].outline_id = Some(outlines_m[oi].0);
                if groups[g].name.is_none() {
                    groups[g].name = outlines_m[oi].1.clone();
                }
            }
            if groups[g].name.is_none() {
                groups[g].name = pieces[i].name.clone();
            }
        }

        // ── Stations ─────────────────────────────────────────────────────────
        let step = mm(STATION_STEP_MM);
        for p in pieces.iter_mut() {
            p.stations = stations(&p.pts, step);
        }

        // ── Bundles of adjacent parallel decks ───────────────────────────────
        // Two decks of one group are adjacent when ≥ 80% of the shorter one
        // lies within the sum of their half widths (+ a small margin) of the
        // longer one: carriageways and separately mapped sidewalks.
        let min_w = mm(MIN_DECK_WIDTH_MM);
        let margin = mm(0.3).max(2.0);
        for g in groups.iter() {
            let mut idx = g.pieces.clone();
            idx.sort_by(|&a, &b| {
                let k = |i: usize| pieces[i].length * pieces[i].width_m;
                k(b).partial_cmp(&k(a)).unwrap_or(std::cmp::Ordering::Equal)
            });
            let mut parent: Vec<usize> = (0..idx.len()).collect();
            for x in 0..idx.len() {
                for y in x + 1..idx.len() {
                    let (a, b) = (&pieces[idx[x]], &pieces[idx[y]]);
                    let (short, long) = if a.length <= b.length { (a, b) } else { (b, a) };
                    let reach = short.width_m / 2.0 + long.width_m.max(min_w) / 2.0 + margin;
                    let near = short.stations.iter().filter(|&&(q, _)| project_on_polyline(q, &long.pts).0 <= reach).count();
                    if near as f64 >= 0.8 * short.stations.len() as f64 {
                        union(&mut parent, x, y);
                    }
                }
            }
            // Spine = first (longest × widest) piece of each bundle.
            let mut spine_of: HashMap<usize, usize> = HashMap::new();
            for x in 0..idx.len() {
                let r = find(&mut parent, x);
                let spine = *spine_of.entry(r).or_insert(idx[x]);
                pieces[idx[x]].spine = spine;
                pieces[idx[x]].attached = spine != idx[x] && pieces[idx[x]].width_m < min_w;
            }
        }

        // ── Recognisability filters 1–2 (length, width) ──────────────────────
        for g in groups.iter_mut() {
            let mut length = g.pieces.iter().map(|&i| pieces[i].length).fold(0.0, f64::max);
            let ends: Vec<Pt> = g
                .pieces
                .iter()
                .flat_map(|&i| [pieces[i].pts[0], *pieces[i].pts.last().unwrap()])
                .collect();
            for a in &ends {
                for b in &ends {
                    length = length.max(len(sub(*a, *b)));
                }
            }
            g.length_m = length;
            let width = g.pieces.iter().map(|&i| pieces[i].width_m).fold(0.0, f64::max);
            let len_mm = length * mm_per_m;
            if len_mm < MIN_BRIDGE_LEN_MM {
                g.skip = Some("too_short");
            } else if width * mm_per_m < MIN_REAL_WIDTH_MM && len_mm < LONG_BRIDGE_MM {
                g.skip = Some("too_thin");
            }
        }

        // ── Mapped supports: cluster, then attach to the nearest deck ────────
        let mut clusters = cluster_supports(features, &project, SUPPORT_CLUSTER_M)?;
        let reach = mm(SUPPORT_REACH_MM).max(SUPPORT_REACH_M);
        for c in clusters.iter_mut() {
            let n = c.pts.len().max(1) as f64;
            let centroid = (c.pts.iter().map(|p| p.0).sum::<f64>() / n, c.pts.iter().map(|p| p.1).sum::<f64>() / n);
            let mut best: Option<(f64, usize)> = None;
            for (i, p) in pieces.iter().enumerate() {
                let (d, _) = project_on_polyline(centroid, &p.pts);
                if d <= p.w_eff(min_w) / 2.0 + reach && best.map_or(true, |(bd, _)| d < bd) {
                    best = Some((d, i));
                }
            }
            if let Some((_, i)) = best {
                c.piece = Some(i);
                groups[pieces[i].group].mapped = true;
            }
        }

        let origin = (
            points.iter().map(|p| p.x).fold(f64::INFINITY, f64::min),
            points.iter().map(|p| p.y).fold(f64::INFINITY, f64::min),
        );
        Ok(Self { pieces, groups, clusters, node_cut, node_pos, rect, origin, cell, mm_per_m, ways_outside })
    }
}

/// Merge bridge ways into chains: ways are first split at interior vertices
/// shared with another bridge way (OSM ways run through junctions), then
/// joined back through nodes where exactly two compatible pieces meet (same
/// kind, layer and width). Undirected: road ways have no meaningful direction.
fn merge_ways(features: &BridgeFeatures) -> Vec<Chain> {
    let ways = &features.ways;
    let mut used_by: HashMap<Key, usize> = HashMap::new();
    for w in ways {
        let mut keys: Vec<Key> = w.line.iter().map(|&p| node_key(p)).collect();
        keys.sort_unstable();
        keys.dedup();
        for k in keys {
            *used_by.entry(k).or_insert(0) += 1;
        }
    }
    // Segments: (way index, points).
    let mut segs: Vec<(usize, Vec<Pt>)> = Vec::new();
    for (wi, w) in ways.iter().enumerate() {
        if w.line.len() < 2 {
            continue;
        }
        let mut cur = vec![w.line[0]];
        for (k, &p) in w.line.iter().enumerate().skip(1) {
            cur.push(p);
            if k + 1 < w.line.len() && used_by[&node_key(p)] >= 2 {
                segs.push((wi, std::mem::replace(&mut cur, vec![p])));
            }
        }
        segs.push((wi, cur));
    }
    let end_key = |s: usize, e: usize| -> Key {
        let pts = &segs[s].1;
        node_key(if e == 0 { pts[0] } else { pts[pts.len() - 1] })
    };
    let mut at: HashMap<Key, Vec<(usize, usize)>> = HashMap::new();
    for s in 0..segs.len() {
        for e in 0..2 {
            at.entry(end_key(s, e)).or_default().push((s, e));
        }
    }
    let compatible = |a: usize, b: usize| {
        let (wa, wb) = (&ways[segs[a].0], &ways[segs[b].0]);
        wa.kind == wb.kind && wa.layer == wb.layer && (wa.width_m - wb.width_m).abs() < 1e-3
    };
    let next = |s: usize, e: usize| -> Option<(usize, usize)> {
        let v = &at[&end_key(s, e)];
        if v.len() != 2 {
            return None;
        }
        let other = if v[0] == (s, e) { v[1] } else { v[0] };
        (other.0 != s && compatible(s, other.0)).then_some(other)
    };

    let mut visited = vec![false; segs.len()];
    let mut chains = Vec::new();
    for s in 0..segs.len() {
        if visited[s] {
            continue;
        }
        visited[s] = true;
        let mut pts = segs[s].1.clone();
        let mut way_ids = vec![ways[segs[s].0].id];
        let (mut cur, mut cur_end) = (s, 1);
        while let Some((o, oe)) = next(cur, cur_end) {
            if visited[o] {
                break;
            }
            visited[o] = true;
            let mut p = segs[o].1.clone();
            if oe == 1 {
                p.reverse();
            }
            pts.extend(p.into_iter().skip(1));
            way_ids.push(ways[segs[o].0].id);
            cur = o;
            cur_end = 1 - oe;
        }
        let (mut cur, mut cur_end) = (s, 0);
        while let Some((o, oe)) = next(cur, cur_end) {
            if visited[o] {
                break;
            }
            visited[o] = true;
            let mut p = segs[o].1.clone();
            if oe == 0 {
                p.reverse();
            }
            p.pop();
            p.extend(pts);
            pts = p;
            way_ids.insert(0, ways[segs[o].0].id);
            cur = o;
            cur_end = 1 - oe;
        }
        way_ids.dedup();
        chains.push(Chain { way_ids, attrs: segs[s].0, pts });
    }
    chains
}

/// Principal axis of a ring: its two ends (extent of the vertices along the
/// axis, through the centroid) and the ring's area.
fn pca_axis(ring: &[Pt]) -> (Pt, Pt, f64) {
    let n = ring.len() as f64;
    let c = (ring.iter().map(|p| p.0).sum::<f64>() / n, ring.iter().map(|p| p.1).sum::<f64>() / n);
    let (mut sxx, mut syy, mut sxy) = (0.0, 0.0, 0.0);
    for p in ring {
        let d = sub(*p, c);
        sxx += d.0 * d.0;
        syy += d.1 * d.1;
        sxy += d.0 * d.1;
    }
    let angle = 0.5 * (2.0 * sxy).atan2(sxx - syy);
    let u = (angle.cos(), angle.sin());
    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    for p in ring {
        let t = dot(sub(*p, c), u);
        lo = lo.min(t);
        hi = hi.max(t);
    }
    let mut area = 0.0;
    for i in 0..ring.len() {
        let (a, b) = (ring[i], ring[(i + 1) % ring.len()]);
        area += a.0 * b.1 - b.0 * a.1;
    }
    (add(c, mul(u, lo)), add(c, mul(u, hi)), area.abs() / 2.0)
}

/// Point at arc length `s` along a polyline.
fn point_at(pts: &[Pt], s: f64) -> Pt {
    let mut acc = 0.0;
    for w in pts.windows(2) {
        let l = len(sub(w[1], w[0]));
        if acc + l >= s && l > 0.0 {
            return add(w[0], mul(sub(w[1], w[0]), (s - acc) / l));
        }
        acc += l;
    }
    *pts.last().unwrap()
}

/// Stations along a polyline: every original vertex, plus points so no gap
/// exceeds `step`. Returns (point, arc length).
fn stations(pts: &[Pt], step: f64) -> Vec<(Pt, f64)> {
    let mut out = vec![(pts[0], 0.0)];
    let mut acc = 0.0;
    for w in pts.windows(2) {
        let l = len(sub(w[1], w[0]));
        if l <= 1e-9 {
            continue;
        }
        let k = (l / step).ceil().max(1.0) as usize;
        for j in 1..=k {
            let t = j as f64 / k as f64;
            out.push((add(w[0], mul(sub(w[1], w[0]), t)), acc + l * t));
        }
        acc += l;
    }
    out
}

/// Cluster mapped supports whose bboxes are within `reach` of each other:
/// the dozens of building:part slabs of one tower become one block.
fn cluster_supports(
    features: &BridgeFeatures,
    project: &dyn Fn(Pt) -> Result<Pt>,
    reach: f64,
) -> Result<Vec<Cluster>> {
    let rings: Vec<Vec<Pt>> = features
        .supports
        .iter()
        .map(|s| s.outer.iter().map(|&p| project(p)).collect::<Result<Vec<Pt>>>())
        .collect::<Result<_>>()?;
    let bbs: Vec<[f64; 4]> = rings
        .iter()
        .map(|r| {
            r.iter().fold([f64::MAX, f64::MAX, f64::MIN, f64::MIN], |b, p| {
                [b[0].min(p.0), b[1].min(p.1), b[2].max(p.0), b[3].max(p.1)]
            })
        })
        .collect();
    let n = rings.len();
    let mut parent: Vec<usize> = (0..n).collect();
    for i in 0..n {
        for j in i + 1..n {
            let (a, b) = (bbs[i], bbs[j]);
            let dx = (b[0] - a[2]).max(a[0] - b[2]).max(0.0);
            let dy = (b[1] - a[3]).max(a[1] - b[3]).max(0.0);
            if dx.hypot(dy) <= reach {
                union(&mut parent, i, j);
            }
        }
    }
    let mut by_root: BTreeMap<usize, Cluster> = BTreeMap::new();
    for i in 0..n {
        let s = &features.supports[i];
        let r = find(&mut parent, i);
        let c = by_root.entry(r).or_insert_with(|| Cluster {
            ids: Vec::new(),
            pts: Vec::new(),
            role: SupportRole::Pier,
            height_m: None,
            deck_relative: false,
            as_building: true,
            piece: None,
        });
        c.ids.push(s.id);
        c.pts.extend(rings[i].iter().copied());
        if s.role == SupportRole::Tower {
            c.role = SupportRole::Tower;
        }
        if let Some(h) = s.height_m {
            c.height_m = Some(c.height_m.map_or(h as f64, |m: f64| m.max(h as f64)));
        }
        if s.min_height_m.is_some_and(|m| m < 0.0) {
            c.deck_relative = true;
        }
        c.as_building &= s.as_building;
    }
    Ok(by_root.into_values().collect())
}

// ── Phase B: heights, openings, supports (mesh units) ─────────────────────────

/// Scene parameters for phase B.
pub struct SolveParams {
    /// Building exaggeration E (mesh units per real metre, vertical).
    pub building_scale: f64,
    /// Knee of the scene's soft-knee height compression.
    pub knee: f32,
    /// Lowest Z of the terrain mesh: no bridge solid goes below it, or the
    /// base plate would drop.
    pub terrain_min_z: f64,
    /// Emit breakaway columns under spans longer than FDM can bridge.
    pub print_supports: bool,
}

/// A closed solid ready to be emitted.
enum Solid {
    /// Deck strip: per station (left xy, right xy, top z, bottom z).
    Strip(Vec<(Pt, Pt, f64, f64)>),
    /// Stack of horizontal quads (CCW from above), bottom to top.
    Prism(Vec<([Pt; 4], f64)>),
}

/// Result of phase B: the solids to emit and the report.
pub struct BridgeBuild {
    structures: Vec<Solid>,
    columns: Vec<Solid>,
    pub report: serde_json::Value,
    pub emitted: usize,
    pub summary: String,
}

/// Per-station profile of one deck piece.
struct Profile {
    top: Vec<f64>,
    bottom: Vec<f64>,
    ground_min: Vec<f64>,
    open: Vec<bool>,
    gap: Vec<Option<f64>>,
}

impl BridgePlan {
    pub fn summary(&self) -> String {
        format!("{} deck piece(s) in {} group(s), {} support cluster(s)", self.pieces.len(), self.groups.len(), self.clusters.len())
    }

    /// Heights, openings, piers, towers and breakaway columns, sampled on the
    /// rendered terrain surface.
    pub fn solve(&self, surface: &TerrainSurface, p: &SolveParams) -> BridgeBuild {
        let mmu = |v: f64| v / self.mm_per_m; // printed mm → mesh units (XY and Z)
        let thick = mmu(MIN_DECK_THICK_MM).max(DECK_THICK_M * p.building_scale);
        let gap_min = mmu(GAP_MM);
        // Clearance a deck is lifted to over water: a real navigation
        // clearance at the building exaggeration, never below GAP.
        let lift_clearance = gap_min.max(LIFT_CLEARANCE_M * p.building_scale);
        let embed = mmu(EMBED_MM);
        let floor = |z: f64| z.max(p.terrain_min_z);
        let min_w = mmu(MIN_DECK_WIDTH_MM);
        let w_eff = |pc: &Piece| pc.w_eff(min_w);
        let clamp = |q: Pt| (q.0.clamp(self.rect[0], self.rect[2]), q.1.clamp(self.rect[1], self.rect[3]));

        // ── Abutments: min of land samples beyond each free end ──────────────
        let mut degree = vec![0usize; self.node_cut.len()];
        for pc in &self.pieces {
            for &e in &pc.ends {
                degree[e] += 1;
            }
        }
        let mut fixed: Vec<Option<f64>> = vec![None; self.node_cut.len()];
        for pc in &self.pieces {
            for (k, &e) in pc.ends.iter().enumerate() {
                if self.node_cut[e] || degree[e] != 1 {
                    continue;
                }
                let (end, inner) = if k == 0 { (pc.pts[0], pc.pts[1]) } else { (pc.pts[pc.pts.len() - 1], pc.pts[pc.pts.len() - 2]) };
                fixed[e] = self.abutment(surface, end, unit(sub(end, inner)), w_eff(pc) / 2.0);
            }
        }
        // Parallel decks (carriageways, separately mapped sidewalks) share
        // their abutment height: no steps between strips side by side.
        let pool = mmu(POOL_ABUTMENT_MM);
        let ends: Vec<(usize, i8)> = self
            .pieces
            .iter()
            .flat_map(|pc| pc.ends.iter().map(move |&e| (e, pc.layer)))
            .filter(|&(e, _)| fixed[e].is_some())
            .collect();
        let pooled: Vec<(usize, f64)> = ends
            .iter()
            .map(|&(e, layer)| {
                let v = ends
                    .iter()
                    .filter(|&&(o, l)| l == layer && len(sub(self.node_pos[o], self.node_pos[e])) <= pool)
                    .filter_map(|&(o, _)| fixed[o])
                    .fold(f64::INFINITY, f64::min);
                (e, v)
            })
            .collect();
        for (e, v) in pooled {
            fixed[e] = Some(v);
        }

        let mut structures = Vec::new();
        let mut columns = Vec::new();
        let mut reports = Vec::new();
        let mut emitted = 0usize;
        let mut skips: BTreeMap<&'static str, usize> = BTreeMap::new();
        let (mut n_mapped, mut n_default, mut n_towers, mut n_columns, mut n_orphans) = (0, 0, 0, 0, 0);
        let mut needs_supports: Vec<String> = Vec::new();

        for (gi, g) in self.groups.iter().enumerate() {
            let main = *g
                .pieces
                .iter()
                .max_by(|&&a, &&b| {
                    let k = |i: usize| self.pieces[i].length * self.pieces[i].width_m;
                    k(a).partial_cmp(&k(b)).unwrap_or(std::cmp::Ordering::Equal)
                })
                .unwrap();
            let center = point_at(&self.pieces[main].pts, self.pieces[main].length / 2.0);
            let center_mm = [(center.0 - self.origin.0) * self.mm_per_m, (center.1 - self.origin.1) * self.mm_per_m];
            let mut report = serde_json::json!({
                "ways": g.pieces.iter().flat_map(|&i| self.pieces[i].way_ids.iter().copied()).collect::<Vec<_>>(),
                "outline": g.outline_id,
                "name": g.name,
                "kind": self.pieces[main].kind.name(),
                "layer": self.pieces[main].layer,
                "structure": g.pieces.iter().find_map(|&i| self.pieces[i].structure.clone()),
                "length_m": g.length_m,
                "length_mm": g.length_m * self.mm_per_m,
                "center_print_mm": center_mm,
                "decks": g.pieces.iter().map(|&i| {
                    let pc = &self.pieces[i];
                    serde_json::json!({
                        "length_m": pc.length,
                        "width_m": pc.width_m,
                        "width_source": pc.width_source,
                        "width_mm": w_eff(pc) * self.mm_per_m,
                        "attached": pc.attached,
                        "spine": pc.spine == i,
                    })
                }).collect::<Vec<_>>(),
            });
            let group_clusters: Vec<&Cluster> = self
                .clusters
                .iter()
                .filter(|c| c.piece.is_some_and(|i| self.pieces[i].group == gi))
                .collect();
            if let Some(reason) = g.skip {
                report["skip_reason"] = reason.into();
                *skips.entry(reason).or_insert(0) += 1;
                n_orphans += group_clusters.len();
                reports.push(report);
                continue;
            }

            // ── Node heights: harmonic on the group's graph ──────────────────
            let values = self.node_heights(g, &fixed);

            // ── Profiles ─────────────────────────────────────────────────────
            let mut profiles: Vec<Profile> = Vec::with_capacity(g.pieces.len());
            for &i in &g.pieces {
                let pc = &self.pieces[i];
                let (va, vb) = (values[pc.ends[0]], values[pc.ends[1]]);
                let caps = pc.ends.map(|e| fixed[e].and(values[e]));
                profiles.push(self.profile(surface, pc, va, vb, caps, lift_clearance, thick, gap_min, embed, &floor, w_eff(pc) / 2.0));
            }
            // Fallback for decks no abutment reaches (both ends cut by the
            // bbox): flat at the highest ground under the group + gap.
            if g.pieces.iter().any(|&i| self.pieces[i].ends.iter().any(|&e| values[e].is_none())) {
                let g_max = g
                    .pieces
                    .iter()
                    .flat_map(|&i| self.pieces[i].stations.iter().map(|s| s.0))
                    .filter_map(|q| surface.sample(q.0, q.1).map(|s| s.0))
                    .fold(p.terrain_min_z, f64::max);
                let flat = g_max + gap_min + thick;
                for (k, &i) in g.pieces.iter().enumerate() {
                    let pc = &self.pieces[i];
                    let va = values[pc.ends[0]].unwrap_or(flat);
                    let vb = values[pc.ends[1]].unwrap_or(flat);
                    let caps = pc.ends.map(|e| fixed[e].and(values[e]));
                    profiles[k] = self.profile(surface, pc, Some(va), Some(vb), caps, lift_clearance, thick, gap_min, embed, &floor, w_eff(pc) / 2.0);
                }
                report["abutments"] = "fallback (no abutment inside the bbox)".into();
            }

            // ── Filter 3: a visible opening ──────────────────────────────────
            let mut best_run = 0.0f64;
            let mut open_len = 0.0f64;
            let mut total_len = 0.0f64;
            let (mut gap_lo, mut gap_hi) = (f64::INFINITY, 0.0f64);
            for (k, &i) in g.pieces.iter().enumerate() {
                let pc = &self.pieces[i];
                for (a, b) in open_runs(&profiles[k].open) {
                    let l = pc.stations[b].1 - pc.stations[a].1;
                    best_run = best_run.max(l);
                    open_len += l;
                    for s in a..=b {
                        if let Some(gp) = profiles[k].gap[s] {
                            gap_lo = gap_lo.min(gp);
                            gap_hi = gap_hi.max(gp);
                        }
                    }
                }
                total_len += pc.length;
            }
            // Deck top vs highest ground under it, per piece (printed mm):
            // explains a `no_clearance` verdict (a DSM carrying the deck shows
            // as ground ≈ top all along).
            report["profile_mm"] = g
                .pieces
                .iter()
                .enumerate()
                .map(|(k, _)| {
                    let pr = &profiles[k];
                    let range = |v: &mut dyn Iterator<Item = f64>| {
                        let (lo, hi) = v.fold((f64::MAX, f64::MIN), |(l, h), z| (l.min(z), h.max(z)));
                        [lo * self.mm_per_m, hi * self.mm_per_m]
                    };
                    let ground: Vec<f64> = pr.top.iter().zip(&pr.gap).filter_map(|(t, gp)| gp.map(|gp| t - thick - gp)).collect();
                    serde_json::json!({
                        "top": range(&mut pr.top.iter().copied()),
                        "ground_max": range(&mut ground.iter().copied()),
                    })
                })
                .collect::<Vec<_>>()
                .into();
            report["abutment_mm"] = g
                .pieces
                .iter()
                .flat_map(|&i| self.pieces[i].ends.iter().map(|&e| values[e].map(|z| z * self.mm_per_m)))
                .collect::<Vec<_>>()
                .into();
            if best_run * self.mm_per_m < MIN_OPEN_RUN_MM {
                report["skip_reason"] = "no_clearance".into();
                report["best_open_run_mm"] = (best_run * self.mm_per_m).into();
                *skips.entry("no_clearance").or_insert(0) += 1;
                n_orphans += group_clusters.len();
                reports.push(report);
                continue;
            }
            report["open_fraction"] = (open_len / total_len.max(1e-9)).into();
            report["gap_mm"] = serde_json::json!({ "min": gap_lo * self.mm_per_m, "max": gap_hi * self.mm_per_m });

            // ── Decks ────────────────────────────────────────────────────────
            for (k, &i) in g.pieces.iter().enumerate() {
                let pc = &self.pieces[i];
                for strip in self.strips(surface, pc, &profiles[k], &values, &degree, w_eff(pc) / 2.0, embed, &floor) {
                    structures.push(Solid::Strip(strip.into_iter().map(|(l, r, t, b)| (clamp(l), clamp(r), t, b)).collect()));
                }
            }

            // ── Supports ─────────────────────────────────────────────────────
            // Mapped piers / towers on the deck they were attached to; then,
            // per bundle spine, default piers and breakaway columns sized to
            // the whole bundle (one pier under twin carriageways, not two).
            let mut piers_mapped = 0usize;
            let mut piers_as_buildings = 0usize;
            let mut piers_default = 0usize;
            let mut towers_json = Vec::new();
            let mut cols = 0usize;
            let (mut unsupported_before, mut unsupported_after) = (0.0f64, 0.0f64);
            let mut mapped_rects: Vec<[Pt; 4]> = Vec::new();
            // Towers of one bridge share a design: untagged ones (3 of
            // Millau's 7 pylons) take the median of the tagged ones.
            let tagged: Vec<(f64, bool)> = group_clusters
                .iter()
                .filter(|c| c.role == SupportRole::Tower)
                .filter_map(|c| c.height_m.map(|h| (h, c.deck_relative)))
                .collect();
            let shared_tower = (!tagged.is_empty()).then(|| {
                let mut t = tagged.clone();
                t.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
                t[t.len() / 2]
            });
            for (k, &i) in g.pieces.iter().enumerate() {
                let pc = &self.pieces[i];
                let prof = &profiles[k];
                for c in group_clusters.iter().filter(|c| c.piece == Some(i)) {
                    let min_size = if c.role == SupportRole::Tower { mmu(MIN_TOWER_MM) } else { mmu(MIN_PIER_MM) };
                    let Some((s, rect)) = self.support_rect(pc, c, min_size) else {
                        continue;
                    };
                    if c.as_building {
                        // Already extruded by the building path: only a stop.
                        mapped_rects.push(rect);
                        piers_as_buildings += 1;
                        continue;
                    }
                    let st = station_index(&pc.stations, s);
                    let ground = sample_min(surface, &rect).unwrap_or(prof.ground_min[st]);
                    let z0 = floor(ground - embed);
                    let (t_lo, t_hi) = along_extent(pc, &rect);
                    match c.role {
                        SupportRole::Pier => {
                            if !prof.open[st] {
                                continue; // the deck already reaches the ground here
                            }
                            let under = min_over(&pc.stations, &prof.bottom, t_lo, t_hi);
                            let z1 = under + mmu(OVERLAP_MM);
                            if z1 - z0 > mmu(0.2) {
                                structures.push(Solid::Prism(vec![(rect.map(clamp), z0), (rect.map(clamp), z1)]));
                                mapped_rects.push(rect);
                                piers_mapped += 1;
                            }
                        }
                        SupportRole::Tower => {
                            let deck_top = prof.top[st];
                            let rise = |h: f64| soft_knee(h as f32, p.knee) as f64 * p.building_scale;
                            let (height, deck_relative, source) = match (c.height_m, shared_tower) {
                                (Some(h), _) => (Some(h), c.deck_relative, "tag"),
                                (None, Some((h, rel))) => (Some(h), rel, "bridge"),
                                (None, None) => (None, false, "none"),
                            };
                            let mut top = match (height, deck_relative) {
                                (Some(h), true) => deck_top + rise(h),
                                (Some(h), false) => ground + rise(h),
                                (None, _) => deck_top + mmu(UNTAGGED_TOWER_ABOVE_DECK_MM),
                            };
                            top = top.max(deck_top + mmu(TOWER_ABOVE_DECK_MM));
                            structures.push(Solid::Prism(vec![(rect.map(clamp), z0), (rect.map(clamp), top)]));
                            mapped_rects.push(rect);
                            towers_json.push(serde_json::json!({
                                "ids": c.ids.len(),
                                "height_m": height,
                                "height_source": source,
                                "height_compressed_m": height.map(|h| soft_knee(h as f32, p.knee)),
                                "deck_relative": deck_relative,
                                "footprint_mm": [len(sub(rect[1], rect[0])) * self.mm_per_m, len(sub(rect[3], rect[0])) * self.mm_per_m],
                                "above_water_or_ground_mm": (top - ground) * self.mm_per_m,
                                "above_deck_mm": (top - deck_top) * self.mm_per_m,
                            }));
                        }
                    }
                }
            }
            let reach = mmu(SUPPORT_REACH_MM).max(SUPPORT_REACH_M);
            for (k, &i) in g.pieces.iter().enumerate() {
                let pc = &self.pieces[i];
                if pc.spine != i {
                    continue; // bundle members are carried by their spine's supports
                }
                let prof = &profiles[k];
                let members: Vec<usize> = g.pieces.iter().copied().filter(|&m| self.pieces[m].spine == i).collect();
                // Stops along the spine: every mapped pier / tower next to it.
                let mut occupied: Vec<(f64, f64)> = mapped_rects
                    .iter()
                    .filter(|r| {
                        let c = mul(add(add(r[0], r[1]), add(r[2], r[3])), 0.25);
                        project_on_polyline(c, &pc.pts).0 <= w_eff(pc) / 2.0 + reach
                    })
                    .map(|r| along_extent(pc, r))
                    .collect();
                // Default piers only where nothing is mapped (fidelity).
                if !g.mapped {
                    for (a, b) in open_runs(&prof.open) {
                        let (s0, s1) = (pc.stations[a].1, pc.stations[b].1);
                        let run = s1 - s0;
                        let n = ((run * self.mm_per_m) / PIER_SPACING_MM).ceil() as usize;
                        for j in 1..n {
                            let s = s0 + run * j as f64 / n as f64;
                            if s - s0 < mmu(1.0) || s1 - s < mmu(1.0) {
                                continue;
                            }
                            let along = mmu(PIER_LEN_MM) / 2.0;
                            let (lo, hi) = self.bundle_range(i, &members, s, min_w);
                            let half = ((hi - lo) / 2.0 * PIER_WIDTH_FRAC).max(mmu(MIN_PIER_MM) / 2.0);
                            let c = (lo + hi) / 2.0;
                            let rect = frame_rect(&pc.pts, s, -along, along, c - half, c + half);
                            let st = station_index(&pc.stations, s);
                            let ground = sample_min(surface, &rect).unwrap_or(prof.ground_min[st]);
                            let z0 = floor(ground - embed);
                            let under = min_over(&pc.stations, &prof.bottom, s - along, s + along);
                            let z1 = under + mmu(OVERLAP_MM);
                            if z1 - z0 > mmu(0.2) {
                                structures.push(Solid::Prism(vec![(rect.map(clamp), z0), (rect.map(clamp), z1)]));
                                occupied.push((s - along, s + along));
                                piers_default += 1;
                            }
                        }
                    }
                }
                // Breakaway columns in spans too long to bridge.
                occupied.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
                for (a, b) in open_runs(&prof.open) {
                    let (s0, s1) = (pc.stations[a].1, pc.stations[b].1);
                    let mut stops = vec![(s0, s0)];
                    stops.extend(occupied.iter().copied().filter(|o| o.1 > s0 && o.0 < s1));
                    stops.push((s1, s1));
                    for w in stops.windows(2) {
                        let (from, to) = (w[0].1, w[1].0);
                        let span = to - from;
                        unsupported_before = unsupported_before.max(span);
                        if span * self.mm_per_m <= MAX_UNSUPPORTED_MM || !p.print_supports {
                            unsupported_after = unsupported_after.max(span);
                            continue;
                        }
                        let n = ((span * self.mm_per_m) / BREAKAWAY_SPACING_MM).ceil() as usize;
                        unsupported_after = unsupported_after.max(span / n as f64);
                        for j in 1..n {
                            let s = from + span * j as f64 / n as f64;
                            let (lo, hi) = self.bundle_range(i, &members, s, min_w);
                            if let Some(col) = self.column(surface, pc, prof, s, lo, hi, embed, &floor) {
                                columns.push(Solid::Prism(col.into_iter().map(|(r, z)| (r.map(clamp), z)).collect()));
                                cols += 1;
                            }
                        }
                    }
                }
            }
            n_mapped += piers_mapped;
            n_default += piers_default;
            n_towers += towers_json.len();
            n_columns += cols;
            if unsupported_before * self.mm_per_m > MAX_UNSUPPORTED_MM {
                needs_supports.push(g.name.clone().unwrap_or_else(|| format!("way {}", report["ways"][0])));
            }
            report["piers_mapped"] = piers_mapped.into();
            report["supports_as_buildings"] = piers_as_buildings.into();
            report["piers_default"] = piers_default.into();
            report["towers"] = towers_json.into();
            report["breakaway_columns"] = cols.into();
            report["max_unsupported_mm"] = serde_json::json!({
                "before": unsupported_before * self.mm_per_m,
                "after": unsupported_after * self.mm_per_m,
            });
            emitted += 1;
            reports.push(report);
        }

        let orphan_clusters = self.clusters.iter().filter(|c| c.piece.is_none()).count();
        n_orphans += orphan_clusters;
        let summary = format!(
            "{} emitted ({} mapped pier(s), {} default pier(s), {} tower(s), {} breakaway column(s)), {} skipped {:?}, {} orphan support cluster(s)",
            emitted, n_mapped, n_default, n_towers, n_columns,
            skips.values().sum::<usize>(), skips, n_orphans
        );
        let report = serde_json::json!({
            "emitted": emitted,
            "skipped": skips,
            "ways_outside_bbox": self.ways_outside,
            "orphan_support_clusters": n_orphans,
            "deck_thickness_mm": thick * self.mm_per_m,
            "breakaway_top_gap_mm": BREAKAWAY_TOP_GAP_MM,
            "breakaway_note": "columns assume 0.2 mm layers; snap them off after printing",
            "spans_over_fdm_limit": needs_supports,
            "groups": reports,
        });
        BridgeBuild { structures, columns, report, emitted, summary }
    }

    /// Abutment height: MEDIAN of the land samples on the road's continuation
    /// beyond a deck end (centreline, ½ to 2 cells out). The road meets the
    /// ground there by definition. The end point itself sits on the crest of
    /// the bank, which the DEM grid and its smoothing round off (Bern's
    /// Kirchenfeld end read 15 m low); lateral samples fall off the approach
    /// embankment or down a cliff (Golden Gate's Vista Point end dived to the
    /// bay with a window minimum). The median still ignores a single DSM roof
    /// over the road. An end mapped a bit into the water searches further out
    /// (up to 4 cells).
    fn abutment(&self, surface: &TerrainSurface, end: Pt, out: Pt, _hw: f64) -> Option<f64> {
        let mut samples: Vec<f64> = Vec::new();
        for k in 1..=8 {
            let q = add(end, mul(out, k as f64 * 0.5 * self.cell));
            if let Some((z, false)) = surface.sample(q.0, q.1) {
                samples.push(z);
            }
            if samples.len() >= 4 {
                break;
            }
        }
        if samples.is_empty() {
            return None;
        }
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        Some(samples[(samples.len() - 1) / 2])
    }

    /// Node heights of one group: abutments fixed, every other node the
    /// 1/length-weighted mean of its neighbours (Gauss–Seidel). On a simple
    /// chain this is the linear interpolation between the abutments.
    fn node_heights(&self, g: &Group, fixed: &[Option<f64>]) -> Vec<Option<f64>> {
        let mut v: Vec<Option<f64>> = fixed.to_vec();
        let mut adj: HashMap<usize, Vec<(usize, f64)>> = HashMap::new();
        for &i in &g.pieces {
            let pc = &self.pieces[i];
            let w = 1.0 / pc.length.max(1e-3);
            adj.entry(pc.ends[0]).or_default().push((pc.ends[1], w));
            adj.entry(pc.ends[1]).or_default().push((pc.ends[0], w));
        }
        let free: Vec<usize> = adj.keys().copied().filter(|&n| fixed[n].is_none()).collect();
        for _ in 0..500 {
            let mut change = 0.0f64;
            for &n in &free {
                let (mut num, mut den) = (0.0, 0.0);
                for &(o, w) in &adj[&n] {
                    if let Some(z) = v[o] {
                        num += w * z;
                        den += w;
                    }
                }
                if den > 0.0 {
                    let z = num / den;
                    change = change.max(v[n].map_or(f64::INFINITY, |old| (old - z).abs()));
                    v[n] = Some(z);
                }
            }
            if change < 1e-6 {
                break;
            }
        }
        v
    }

    /// Deck profile along one piece.
    ///
    /// The deck runs straight between its end heights `va`, `vb`. Over water
    /// it is then lifted where the banks are too low for a visible gap (the
    /// Arno's Lungarni print barely 1 mm above the river): the requirement
    /// `water + clearance + T` is spread with a cone of slope LIFT_SLOPE and
    /// capped by the same slope from each abutment, so the deck rises from
    /// the bank like a humped bridge and never steps at its ends.
    #[allow(clippy::too_many_arguments)]
    fn profile(
        &self,
        surface: &TerrainSurface,
        pc: &Piece,
        va: Option<f64>,
        vb: Option<f64>,
        caps: [Option<f64>; 2],
        lift_clearance: f64,
        thick: f64,
        gap_min: f64,
        embed: f64,
        floor: &dyn Fn(f64) -> f64,
        hw: f64,
    ) -> Profile {
        let n = pc.stations.len();
        let (va, vb) = match (va, vb) {
            (Some(a), Some(b)) => (a, b),
            (Some(a), None) => (a, a),
            (None, Some(b)) => (b, b),
            (None, None) => (0.0, 0.0), // replaced by the caller's fallback
        };
        // Ground under each station: (max, min, any water), None outside.
        let ground: Vec<Option<(f64, f64, bool)>> = pc
            .stations
            .iter()
            .enumerate()
            .map(|(k, &(q, _))| {
                let side = perp(tangent(&pc.stations, k));
                let samples: Vec<(f64, bool)> = [-hw, 0.0, hw]
                    .iter()
                    .filter_map(|&d| surface.sample(q.0 + side.0 * d, q.1 + side.1 * d))
                    .collect();
                if samples.is_empty() {
                    return None;
                }
                Some((
                    samples.iter().map(|x| x.0).fold(f64::MIN, f64::max),
                    samples.iter().map(|x| x.0).fold(f64::MAX, f64::min),
                    samples.iter().any(|x| x.1),
                ))
            })
            .collect();
        let s_of = |k: usize| pc.stations[k].1;
        let mut top: Vec<f64> = (0..n).map(|k| va + (vb - va) * (s_of(k) / pc.length.max(1e-9))).collect();
        // Lift over water: cone dilation of the requirement (two passes),
        // capped by the slope from each abutment.
        let req: Vec<f64> = ground
            .iter()
            .map(|g| match g {
                Some((g_max, _, true)) => g_max + lift_clearance + thick,
                _ => f64::NEG_INFINITY,
            })
            .collect();
        if req.iter().any(|r| r.is_finite()) {
            let mut cone = req.clone();
            for k in 1..n {
                cone[k] = cone[k].max(cone[k - 1] - LIFT_SLOPE * (s_of(k) - s_of(k - 1)));
            }
            for k in (0..n.saturating_sub(1)).rev() {
                cone[k] = cone[k].max(cone[k + 1] - LIFT_SLOPE * (s_of(k + 1) - s_of(k)));
            }
            for k in 0..n {
                let mut cap = f64::INFINITY;
                if let Some(a) = caps[0] {
                    cap = cap.min(a + LIFT_SLOPE * s_of(k));
                }
                if let Some(b) = caps[1] {
                    cap = cap.min(b + LIFT_SLOPE * (pc.length - s_of(k)));
                }
                top[k] = top[k].max(cone[k].min(cap));
            }
        }
        let mut prof = Profile {
            top: Vec::with_capacity(n),
            bottom: Vec::with_capacity(n),
            ground_min: Vec::with_capacity(n),
            open: Vec::with_capacity(n),
            gap: Vec::with_capacity(n),
        };
        for k in 0..n {
            let t = top[k];
            let (g_max, g_min) = ground[k].map_or((t - thick, t - thick), |g| (g.0, g.1));
            let gap = t - thick - g_max;
            prof.top.push(t);
            prof.ground_min.push(g_min);
            prof.gap.push(ground[k].is_some().then_some(gap));
            prof.open.push(ground[k].is_some() && gap >= gap_min - 1e-6);
            prof.bottom.push(t - thick);
        }
        // Openings too short to print or read are filled.
        let short: Vec<(usize, usize)> = open_runs(&prof.open)
            .into_iter()
            .filter(|&(a, b)| (pc.stations[b].1 - pc.stations[a].1) * self.mm_per_m < MIN_OPEN_RUN_MM)
            .collect();
        for (a, b) in short {
            for s in a..=b {
                prof.open[s] = false;
            }
        }
        // A slit thinner than GAP between deck and ground would not print:
        // close it down to the ground (a local solid abutment).
        for k in 0..n {
            if !prof.open[k] {
                if let Some(gp) = prof.gap[k] {
                    if gp > 0.0 {
                        prof.bottom[k] = floor(prof.ground_min[k] - embed).min(prof.top[k] - thick);
                    }
                }
            }
            prof.bottom[k] = floor(prof.bottom[k]).min(prof.top[k] - 1e-3);
        }
        prof
    }

    /// Strip geometry of one piece, split at sharp bends. Abutment ends run
    /// END_EXTENSION into the bank; junction ends overlap the neighbouring
    /// deck by half a width; cut ends stop at the bbox.
    #[allow(clippy::too_many_arguments)]
    fn strips(
        &self,
        surface: &TerrainSurface,
        pc: &Piece,
        prof: &Profile,
        values: &[Option<f64>],
        degree: &[usize],
        hw: f64,
        embed: f64,
        floor: &dyn Fn(f64) -> f64,
    ) -> Vec<Vec<(Pt, Pt, f64, f64)>> {
        let st = &pc.stations;
        let n = st.len();
        // Split points: stations where the direction turns more than MAX_BEND.
        let mut cuts = vec![0usize];
        for k in 1..n - 1 {
            let (d0, d1) = (unit(sub(st[k].0, st[k - 1].0)), unit(sub(st[k + 1].0, st[k].0)));
            if dot(d0, d1) < MAX_BEND_DEG.to_radians().cos() {
                cuts.push(k);
            }
        }
        cuts.push(n - 1);
        let mut out = Vec::new();
        for w in cuts.windows(2) {
            let (a, b) = (w[0], w[1]);
            if b <= a {
                continue;
            }
            let mut strip = Vec::with_capacity(b - a + 3);
            for k in a..=b {
                let normal = if k == a {
                    perp(unit(sub(st[k + 1].0, st[k].0)))
                } else if k == b {
                    perp(unit(sub(st[k].0, st[k - 1].0)))
                } else {
                    let n_prev = perp(unit(sub(st[k].0, st[k - 1].0)));
                    let n_mid = perp(tangent(st, k));
                    mul(n_mid, 1.0 / dot(n_mid, n_prev).max(0.5))
                };
                let q = st[k].0;
                strip.push((add(q, mul(normal, hw)), sub(q, mul(normal, hw)), prof.top[k], prof.bottom[k]));
            }
            // End extensions on the piece's real ends.
            for (end_idx, is_start) in [(a, true), (b, false)] {
                if (is_start && a != 0) || (!is_start && b != n - 1) {
                    continue;
                }
                let node = pc.ends[if is_start { 0 } else { 1 }];
                if self.node_cut[node] || values[node].is_none() {
                    continue;
                }
                let dir = if is_start { unit(sub(st[0].0, st[1].0)) } else { unit(sub(st[n - 1].0, st[n - 2].0)) };
                let ext = if degree[node] == 1 { (END_EXTENSION_MM / self.mm_per_m).max(0.0) } else { hw };
                let q = add(st[end_idx].0, mul(dir, ext));
                let normal = if is_start { perp(mul(dir, -1.0)) } else { perp(dir) };
                let top = prof.top[end_idx];
                let bottom = if degree[node] == 1 {
                    let g = surface.sample(q.0, q.1).map_or(prof.bottom[end_idx], |s| s.0 - embed);
                    floor(g.min(prof.bottom[end_idx])).min(top - 1e-3)
                } else {
                    prof.bottom[end_idx]
                };
                let v = (add(q, mul(normal, hw)), sub(q, mul(normal, hw)), top, bottom);
                if is_start {
                    strip.insert(0, v);
                } else {
                    strip.push(v);
                }
            }
            out.push(strip);
        }
        out
    }

    /// Lateral extent [lo, hi] (left positive) of a spine's bundle at arc
    /// length `s`, in the spine's frame.
    fn bundle_range(&self, spine: usize, members: &[usize], s: f64, min_w: f64) -> (f64, f64) {
        let sp = &self.pieces[spine];
        let origin = point_at(&sp.pts, s);
        let t = unit(sub(point_at(&sp.pts, (s + 0.5).min(sp.length)), point_at(&sp.pts, (s - 0.5).max(0.0))));
        let nrm = perp(t);
        let hw = sp.w_eff(min_w) / 2.0;
        let margin = (0.3 / self.mm_per_m).max(2.0);
        let (mut lo, mut hi) = (-hw, hw);
        for &m in members {
            if m == spine {
                continue;
            }
            let pm = &self.pieces[m];
            let hm = pm.w_eff(min_w) / 2.0;
            let (d, sm) = project_on_polyline(origin, &pm.pts);
            if d > hw + hm + margin {
                continue;
            }
            let c = dot(sub(point_at(&pm.pts, sm), origin), nrm);
            lo = lo.min(c - hm);
            hi = hi.max(c + hm);
        }
        (lo, hi)
    }

    /// Rectangle around a support cluster in the deck's local frame (along /
    /// across the axis), widened to `min_size` on each side. Returns its
    /// arc-length position and the rectangle.
    fn support_rect(&self, pc: &Piece, c: &Cluster, min_size: f64) -> Option<(f64, [Pt; 4])> {
        if c.pts.is_empty() {
            return None;
        }
        let n = c.pts.len() as f64;
        let centroid = (c.pts.iter().map(|p| p.0).sum::<f64>() / n, c.pts.iter().map(|p| p.1).sum::<f64>() / n);
        let (_, s) = project_on_polyline(centroid, &pc.pts);
        let origin = point_at(&pc.pts, s);
        let t = unit(sub(point_at(&pc.pts, (s + 0.5).min(pc.length)), point_at(&pc.pts, (s - 0.5).max(0.0))));
        let nrm = perp(t);
        let (mut t0, mut t1, mut n0, mut n1) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
        for &q in &c.pts {
            let d = sub(q, origin);
            t0 = t0.min(dot(d, t));
            t1 = t1.max(dot(d, t));
            n0 = n0.min(dot(d, nrm));
            n1 = n1.max(dot(d, nrm));
        }
        let widen = |lo: f64, hi: f64| {
            if hi - lo >= min_size {
                (lo, hi)
            } else {
                let m = (lo + hi) / 2.0;
                (m - min_size / 2.0, m + min_size / 2.0)
            }
        };
        let (t0, t1) = widen(t0, t1);
        let (n0, n1) = widen(n0, n1);
        let corner = |a: f64, b: f64| add(add(origin, mul(t, a)), mul(nrm, b));
        Some((s, [corner(t0, n0), corner(t1, n0), corner(t1, n1), corner(t0, n1)]))
    }

    /// Breakaway column under the deck at arc length `s`, centred under the
    /// bundle's lateral range [lo, hi]: a 0.8 mm neck on the surface flaring
    /// to 1.2 mm, stopping one layer below the deck.
    #[allow(clippy::too_many_arguments)]
    fn column(
        &self,
        surface: &TerrainSurface,
        pc: &Piece,
        prof: &Profile,
        s: f64,
        lo: f64,
        hi: f64,
        embed: f64,
        floor: &dyn Fn(f64) -> f64,
    ) -> Option<Vec<([Pt; 4], f64)>> {
        let mmu = |v: f64| v / self.mm_per_m;
        let along = mmu(BREAKAWAY_SIZE_MM) / 2.0;
        let across = (0.7 * (hi - lo)).max(mmu(BREAKAWAY_SIZE_MM)) / 2.0;
        let neck = mmu(BREAKAWAY_NECK_MM) / 2.0;
        let c = (lo + hi) / 2.0;
        let full = frame_rect(&pc.pts, s, -along, along, c - across, c + across);
        let thin = frame_rect(&pc.pts, s, -neck, neck, c - neck, c + neck);
        let st = station_index(&pc.stations, s);
        let ground = sample_min(surface, &full).unwrap_or(prof.ground_min[st]);
        let z0 = floor(ground - embed);
        let top = min_over(&pc.stations, &prof.bottom, s - along, s + along) - mmu(BREAKAWAY_TOP_GAP_MM);
        let height = top - z0;
        if height < mmu(0.6) {
            return None;
        }
        let flare = (embed + mmu(BREAKAWAY_FLARE_MM)).min(0.5 * height);
        Some(vec![(thin, z0), (full, z0 + flare), (full, top)])
    }

}

impl BridgeBuild {
    /// Emit decks, piers and towers (inside the building colour range).
    /// Returns (solids emitted, solids rolled back).
    pub fn emit_structures(&self, mesh: &mut TerrainMesh) -> (usize, usize) {
        emit_all(&self.structures, mesh)
    }

    /// Emit the breakaway columns (their own range, after the buildings).
    pub fn emit_columns(&self, mesh: &mut TerrainMesh) -> (usize, usize) {
        emit_all(&self.columns, mesh)
    }
}

/// Maximal runs of `true` as (first, last) station indices.
fn open_runs(open: &[bool]) -> Vec<(usize, usize)> {
    let mut runs = Vec::new();
    let mut start = None;
    for (k, &o) in open.iter().enumerate() {
        match (o, start) {
            (true, None) => start = Some(k),
            (false, Some(a)) => {
                if k - 1 > a {
                    runs.push((a, k - 1));
                }
                start = None;
            }
            _ => {}
        }
    }
    if let Some(a) = start {
        if open.len() - 1 > a {
            runs.push((a, open.len() - 1));
        }
    }
    runs
}

/// Unit tangent at a station (central difference).
fn tangent(st: &[(Pt, f64)], k: usize) -> Pt {
    let a = if k == 0 { 0 } else { k - 1 };
    let b = (k + 1).min(st.len() - 1);
    unit(sub(st[b].0, st[a].0))
}

fn station_index(st: &[(Pt, f64)], s: f64) -> usize {
    match st.binary_search_by(|x| x.1.partial_cmp(&s).unwrap_or(std::cmp::Ordering::Less)) {
        Ok(i) => i,
        Err(i) => i.min(st.len() - 1),
    }
}

/// Lowest value of `v` over the stations within [s0, s1] (nearest station
/// when none falls inside).
fn min_over(st: &[(Pt, f64)], v: &[f64], s0: f64, s1: f64) -> f64 {
    let m = st
        .iter()
        .zip(v)
        .filter(|(x, _)| x.1 >= s0 && x.1 <= s1)
        .map(|(_, &z)| z)
        .fold(f64::INFINITY, f64::min);
    if m.is_finite() {
        m
    } else {
        v[station_index(st, (s0 + s1) / 2.0)]
    }
}

/// Lowest rendered surface under a rectangle (corners + centre).
fn sample_min(surface: &TerrainSurface, rect: &[Pt; 4]) -> Option<f64> {
    let c = mul(add(add(rect[0], rect[1]), add(rect[2], rect[3])), 0.25);
    rect.iter()
        .chain(std::iter::once(&c))
        .filter_map(|q| surface.sample(q.0, q.1).map(|s| s.0))
        .fold(None, |m: Option<f64>, z| Some(m.map_or(z, |m| m.min(z))))
}

/// Arc-length extent of a rectangle projected on a piece.
fn along_extent(pc: &Piece, rect: &[Pt; 4]) -> (f64, f64) {
    rect.iter()
        .map(|&q| project_on_polyline(q, &pc.pts).1)
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), s| (lo.min(s), hi.max(s)))
}

/// Rectangle in the local frame at arc length `s` (t along, n across, left
/// positive), CCW from above.
fn frame_rect(pts: &[Pt], s: f64, t0: f64, t1: f64, n0: f64, n1: f64) -> [Pt; 4] {
    let total = polyline_len(pts);
    let origin = point_at(pts, s);
    let t = unit(sub(point_at(pts, (s + 0.5).min(total)), point_at(pts, (s - 0.5).max(0.0))));
    let nrm = perp(t);
    let corner = |a: f64, b: f64| add(add(origin, mul(t, a)), mul(nrm, b));
    [corner(t0, n0), corner(t1, n0), corner(t1, n1), corner(t0, n1)]
}

/// Emit solids, rolling back any that is not combinatorially closed.
/// Returns (emitted, rolled back).
fn emit_all(solids: &[Solid], mesh: &mut TerrainMesh) -> (usize, usize) {
    let (mut ok, mut bad) = (0, 0);
    for solid in solids {
        let (v0, t0) = (mesh.vertices.len(), mesh.triangles.len());
        match solid {
            Solid::Strip(st) => emit_strip(st, mesh),
            Solid::Prism(rings) => emit_prism(rings, mesh),
        }
        if mesh.triangles.len() > t0 && solid_is_closed(&mesh.triangles[t0..]) {
            ok += 1;
        } else {
            mesh.vertices.truncate(v0);
            mesh.triangles.truncate(t0);
            bad += 1;
        }
    }
    (ok, bad)
}

/// Deck strip: 4 vertices per station (TL, TR, BR, BL), quads on the four
/// faces between stations, two end caps. Closed by construction.
fn emit_strip(st: &[(Pt, Pt, f64, f64)], mesh: &mut TerrainMesh) {
    if st.len() < 2 {
        return;
    }
    let base = mesh.vertices.len();
    for &(l, r, top, bottom) in st {
        mesh.vertices.push([l.0 as f32, l.1 as f32, top as f32]);
        mesh.vertices.push([r.0 as f32, r.1 as f32, top as f32]);
        mesh.vertices.push([r.0 as f32, r.1 as f32, bottom as f32]);
        mesh.vertices.push([l.0 as f32, l.1 as f32, bottom as f32]);
    }
    let v = |k: usize, c: usize| base + 4 * k + c; // c: 0 TL, 1 TR, 2 BR, 3 BL
    for i in 0..st.len() - 1 {
        let j = i + 1;
        let t = &mut mesh.triangles;
        t.push([v(i, 0), v(i, 1), v(j, 1)]); // top
        t.push([v(i, 0), v(j, 1), v(j, 0)]);
        t.push([v(i, 3), v(j, 2), v(i, 2)]); // bottom
        t.push([v(i, 3), v(j, 3), v(j, 2)]);
        t.push([v(i, 0), v(j, 0), v(j, 3)]); // left
        t.push([v(i, 0), v(j, 3), v(i, 3)]);
        t.push([v(i, 1), v(j, 2), v(j, 1)]); // right
        t.push([v(i, 1), v(i, 2), v(j, 2)]);
    }
    let last = st.len() - 1;
    let t = &mut mesh.triangles;
    t.push([v(0, 0), v(0, 3), v(0, 2)]); // start cap
    t.push([v(0, 0), v(0, 2), v(0, 1)]);
    t.push([v(last, 0), v(last, 2), v(last, 3)]); // end cap
    t.push([v(last, 0), v(last, 1), v(last, 2)]);
}

/// Stack of CCW quads joined by side walls, capped at both ends.
fn emit_prism(rings: &[([Pt; 4], f64)], mesh: &mut TerrainMesh) {
    if rings.len() < 2 {
        return;
    }
    let base = mesh.vertices.len();
    for (quad, z) in rings {
        for q in quad {
            mesh.vertices.push([q.0 as f32, q.1 as f32, *z as f32]);
        }
    }
    let v = |r: usize, k: usize| base + 4 * r + (k % 4);
    for r in 0..rings.len() - 1 {
        for k in 0..4 {
            mesh.triangles.push([v(r, k), v(r, k + 1), v(r + 1, k + 1)]);
            mesh.triangles.push([v(r, k), v(r + 1, k + 1), v(r + 1, k)]);
        }
    }
    let top = rings.len() - 1;
    mesh.triangles.push([v(0, 0), v(0, 2), v(0, 1)]);
    mesh.triangles.push([v(0, 0), v(0, 3), v(0, 2)]);
    mesh.triangles.push([v(top, 0), v(top, 1), v(top, 2)]);
    mesh.triangles.push([v(top, 0), v(top, 2), v(top, 3)]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{BoundingBox, BridgeSupport, BridgeWay, ElevationPoint, WaterArea, WaterFeatures, WaterKind};
    use crate::utils::hydro::HydroModel;
    use crate::utils::mesh::MeshGenerator;

    const RES: u32 = 40;

    fn bbox() -> BoundingBox {
        // ~1.2 km × 1.1 km near Bologna → ~0.15 mm/m on a 180 mm print.
        BoundingBox { min_lat: 44.49, min_lon: 11.33, max_lat: 44.50, max_lon: 11.345 }
    }

    fn deck(id: u64, pts: Vec<Pt>) -> BridgeWay {
        BridgeWay {
            id,
            line: pts,
            kind: BridgeKind::Road,
            layer: 1,
            width_m: 10.0,
            width_source: "default",
            name: Some(format!("deck {id}")),
            structure: None,
        }
    }

    fn square(lon: f64, lat: f64, r: f64) -> Vec<Pt> {
        vec![(lon - r, lat - r), (lon + r, lat - r), (lon + r, lat + r), (lon - r, lat + r)]
    }

    struct Scene {
        mesh: TerrainMesh,
        build: BridgeBuild,
        structures: (usize, usize),
        columns: (usize, usize),
        manifold: bool,
        extent_unchanged: bool,
    }

    /// Valley crossing north–south around lat 44.495: river 110 m wide in the
    /// bottom, steep banks up to a plateau `bank_m` above the water from 75 m
    /// off the axis. Runs the whole pipeline: hydro, terrain
    /// mesh, plan, solve, emission, base closure, manifold check.
    fn scene(bank_m: f32, features: &BridgeFeatures) -> Scene {
        scene_with(bank_m, 0.0005, features)
    }

    /// `river_half`: half width of the river in degrees of latitude.
    fn scene_with(bank_m: f32, river_half: f64, features: &BridgeFeatures) -> Scene {
        let b = bbox();
        let bank_from = (river_half * 111_320.0) as f32 + 5.0;
        let z = |_lon: f64, lat: f64| -> f32 {
            let d = ((lat - 44.495).abs() * 111_320.0) as f32; // metres from the river axis
            30.0 + bank_m * ((d - bank_from) / 15.0).clamp(0.0, 1.0)
        };
        let mut elev = Vec::new();
        for i in 0..=RES {
            for j in 0..=RES {
                let lat = b.min_lat + i as f64 * (b.max_lat - b.min_lat) / RES as f64;
                let lon = b.min_lon + j as f64 * (b.max_lon - b.min_lon) / RES as f64;
                elev.push(ElevationPoint { lat, lon, elevation: z(lon, lat) });
            }
        }
        let projector = CoordinateProjector::new(&b).unwrap();
        let mut points = projector.project_points(&elev).unwrap();
        let xs = points.iter().map(|p| p.x).fold(f64::NEG_INFINITY, f64::max)
            - points.iter().map(|p| p.x).fold(f64::INFINITY, f64::min);
        let mm_per_m = 180.0 / xs;
        let river = WaterFeatures {
            areas: vec![WaterArea {
                id: 1,
                kind: WaterKind::Lake,
                outer: vec![
                    (11.32, 44.495 - river_half),
                    (11.36, 44.495 - river_half),
                    (11.36, 44.495 + river_half),
                    (11.32, 44.495 + river_half),
                ],
                holes: Vec::new(),
            }],
            ..Default::default()
        };
        let grid_dim = (RES + 1) as usize;
        let hydro = HydroModel::build(&river, &[], "test", &projector, &mut points, grid_dim, mm_per_m)
            .unwrap()
            .expect("water expected");
        let plan = BridgePlan::build(features, &projector, &points, grid_dim, mm_per_m).unwrap();
        let generator = MeshGenerator::new(projector, 1.0, 1.0, 0.0, 0.6 / mm_per_m as f32, 2.0);
        let (mut mesh, water_tc) = generator.generate_terrain_mesh(&points, Some(&hydro)).unwrap();
        let (terrain_tc, terrain_vc) = (mesh.triangles.len(), mesh.vertices.len());
        let extent = |m: &TerrainMesh| {
            m.vertices.iter().fold([f32::MAX, f32::MAX, f32::MIN, f32::MIN], |b, v| {
                [b[0].min(v[0]), b[1].min(v[1]), b[2].max(v[0]), b[3].max(v[1])]
            })
        };
        let before = extent(&mesh);
        let surface = TerrainSurface::build(&mesh, terrain_tc, water_tc);
        let terrain_min_z = mesh.vertices[..terrain_vc].iter().map(|v| v[2]).fold(f32::INFINITY, f32::min);
        let params = SolveParams { building_scale: 1.0, knee: 36.0, terrain_min_z: terrain_min_z as f64, print_supports: true };
        let build = plan.solve(&surface, &params);
        let structures = build.emit_structures(&mut mesh);
        let columns = build.emit_columns(&mut mesh);
        let extent_unchanged = extent(&mesh) == before;
        let min_after = mesh.vertices.iter().map(|v| v[2]).fold(f32::INFINITY, f32::min);
        assert!(min_after >= terrain_min_z - 1e-4, "no bridge solid below the terrain minimum");
        let mut closed = TerrainMesh { vertices: mesh.vertices.clone(), triangles: mesh.triangles.clone() };
        generator.close_mesh_with_base(&mut closed).unwrap();
        let manifold = generator.validate_manifold(&closed).unwrap();
        Scene { mesh, build, structures, columns, manifold, extent_unchanged }
    }

    fn crossing() -> Vec<Pt> {
        // ~156 m north–south, plateau to plateau (≈ 23 mm printed).
        vec![(11.3375, 44.4943), (11.3375, 44.4957)]
    }

    #[test]
    fn high_banks_give_an_open_deck_with_default_piers() {
        let f = BridgeFeatures { ways: vec![deck(1, crossing())], ..Default::default() };
        let s = scene(20.0, &f);
        let g = &s.build.report["groups"][0];
        assert_eq!(s.build.emitted, 1, "report: {}", s.build.report);
        assert!(g["skip_reason"].is_null());
        assert!(g["gap_mm"]["min"].as_f64().unwrap() >= GAP_MM - 1e-6, "gap {}", g["gap_mm"]);
        assert!(g["open_fraction"].as_f64().unwrap() > 0.5);
        let piers = g["piers_default"].as_u64().unwrap();
        assert!(piers >= 2, "~20 mm opening at ≤ {PIER_SPACING_MM} mm spacing: {piers}");
        assert!(g["max_unsupported_mm"]["before"].as_f64().unwrap() <= MAX_UNSUPPORTED_MM);
        assert_eq!(g["breakaway_columns"].as_u64().unwrap(), 0, "default piers are close enough");
        assert_eq!(s.structures.1, 0, "every solid closed");
        assert_eq!(s.structures.0, 1 + piers as usize);
        assert!(s.manifold, "terrain + water + bridge + base is a closed 2-manifold");
        assert!(s.extent_unchanged, "the bridge never widens the print");
        // The deck meets the RENDERED bank: the ~28 m grid interpolates the
        // steep bank, so the end sits between the water and the plateau.
        let abut = g["abutment_mm"][0].as_f64().unwrap();
        let mm_per_m = g["length_mm"].as_f64().unwrap() / g["length_m"].as_f64().unwrap();
        assert!(abut > 0.6 * 20.0 * mm_per_m && abut <= 20.0 * mm_per_m + 1e-6, "abutment {abut} mm");
    }

    #[test]
    fn low_banks_lift_the_deck_over_the_water() {
        let f = BridgeFeatures { ways: vec![deck(1, crossing())], ..Default::default() };
        let s = scene(1.0, &f);
        let g = &s.build.report["groups"][0];
        assert_eq!(s.build.emitted, 1, "{}", s.build.report);
        assert!(g["gap_mm"]["min"].as_f64().unwrap() >= GAP_MM - 1e-6);
        // Humped profile: higher than the abutments mid-span, equal at the ends.
        let top = &g["profile_mm"][0]["top"];
        let abut: Vec<f64> = g["abutment_mm"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect();
        assert!(top[1].as_f64().unwrap() > abut[0] + GAP_MM, "lifted: {top} vs {abut:?}");
        assert!((top[0].as_f64().unwrap() - abut[0].min(abut[1])).abs() < 1e-6, "no step at the abutment");
        assert_eq!(s.structures.1, 0);
        assert!(s.manifold);
    }

    #[test]
    fn short_low_crossing_is_not_recognisable() {
        // 18 m canal, banks 1 m above it, 5 mm deck: a 0.3 slope cannot
        // climb to a visible gap within half the deck.
        let f = BridgeFeatures { ways: vec![deck(1, vec![(11.3375, 44.49485), (11.3375, 44.49515)])], ..Default::default() };
        let s = scene_with(1.0, 0.00008, &f);
        assert_eq!(s.build.emitted, 0, "{}", s.build.report);
        assert_eq!(s.build.report["groups"][0]["skip_reason"], "no_clearance");
        assert!(s.manifold);
    }

    #[test]
    fn short_and_thin_bridges_are_skipped() {
        let short = deck(1, vec![(11.3375, 44.4948), (11.3375, 44.4950)]); // ~22 m ≈ 3.4 mm
        let mut thin = deck(2, vec![(11.3400, 44.4943), (11.3400, 44.4957)]); // ~155 m ≈ 23 mm…
        thin.width_m = 1.0; // …but 1 m wide ≈ 0.15 mm
        let mut thin_short = thin.clone();
        thin_short.id = 3;
        thin_short.line = vec![(11.3420, 44.4947), (11.3420, 44.4953)]; // ~67 m ≈ 10 mm limit
        thin_short.line[1].1 = 44.49525; // ~61 m ≈ 9.3 mm
        let f = BridgeFeatures { ways: vec![short, thin, thin_short], ..Default::default() };
        let s = scene(20.0, &f);
        let reasons: Vec<&str> = s.build.report["groups"]
            .as_array()
            .unwrap()
            .iter()
            .map(|g| g["skip_reason"].as_str().unwrap_or("emitted"))
            .collect();
        assert_eq!(reasons, vec!["too_short", "emitted", "too_thin"], "{}", s.build.report);
    }

    #[test]
    fn mapped_towers_replace_default_piers_and_long_spans_get_columns() {
        let tower = |id: u64, lat: f64| BridgeSupport {
            id,
            outer: square(11.3375, lat, 0.0001),
            role: SupportRole::Tower,
            height_m: Some(150.0),
            min_height_m: None,
            building: None,
            as_building: false,
        };
        // Towers ~130 m apart ≈ 19 mm printed: too long to bridge.
        let f = BridgeFeatures {
            ways: vec![deck(1, crossing())],
            supports: vec![tower(10, 44.4944), tower(11, 44.4956)],
            ..Default::default()
        };
        let s = scene(20.0, &f);
        let g = &s.build.report["groups"][0];
        assert_eq!(s.build.emitted, 1);
        assert_eq!(g["piers_default"], 0, "fidelity: no invented piers when supports are mapped");
        let towers = g["towers"].as_array().unwrap();
        assert_eq!(towers.len(), 2);
        for t in towers {
            assert!(t["above_deck_mm"].as_f64().unwrap() >= TOWER_ABOVE_DECK_MM - 1e-6);
            // 150 m through the soft knee (36 m): 36 + 114·0.3 = 70.2 m.
            assert!((t["height_compressed_m"].as_f64().unwrap() - 70.2).abs() < 1e-3);
        }
        let cols = g["breakaway_columns"].as_u64().unwrap();
        assert!(cols >= 2, "≈17 mm main span at ≤ {BREAKAWAY_SPACING_MM} mm: {cols}");
        assert!(g["max_unsupported_mm"]["before"].as_f64().unwrap() > MAX_UNSUPPORTED_MM);
        assert!(g["max_unsupported_mm"]["after"].as_f64().unwrap() <= BREAKAWAY_SPACING_MM + 1e-6);
        assert_eq!(s.columns, (cols as usize, 0), "columns emitted, all closed");
        assert!(s.manifold);
        // Columns stop one layer below the deck (flat deck: equal abutments).
        let mm_per_m = g["length_mm"].as_f64().unwrap() / g["length_m"].as_f64().unwrap();
        let deck_top = g["abutment_mm"][0].as_f64().unwrap() / mm_per_m;
        let deck_bottom = deck_top - s.build.report["deck_thickness_mm"].as_f64().unwrap() / mm_per_m;
        let col_tops: Vec<f32> = s.mesh.triangles[s.mesh.triangles.len() - 20 * cols as usize..]
            .iter()
            .flatten()
            .map(|&v| s.mesh.vertices[v][2])
            .collect();
        let top = col_tops.iter().cloned().fold(f32::MIN, f32::max) as f64;
        let expected = deck_bottom - BREAKAWAY_TOP_GAP_MM / mm_per_m;
        assert!((top - expected).abs() < 0.05 / mm_per_m, "column top {top} vs {expected}");
    }

    #[test]
    fn deck_relative_pylon_rises_from_the_deck() {
        let pylon = BridgeSupport {
            id: 7,
            outer: square(11.3375, 44.4950, 0.00005),
            role: SupportRole::Tower,
            height_m: Some(30.0),
            min_height_m: Some(-40.0),
            building: None,
            as_building: false,
        };
        let f = BridgeFeatures { ways: vec![deck(1, crossing())], supports: vec![pylon], ..Default::default() };
        let s = scene(20.0, &f);
        let g = &s.build.report["groups"][0];
        let t = &g["towers"][0];
        let mm_per_m = g["length_mm"].as_f64().unwrap() / g["length_m"].as_f64().unwrap();
        assert!(t["deck_relative"].as_bool().unwrap());
        assert!((t["above_deck_mm"].as_f64().unwrap() - 30.0 * mm_per_m).abs() < 0.3, "{t}");
    }

    #[test]
    fn junction_heights_are_harmonic_and_parallel_decks_share_abutments() {
        // A ramp joins the main deck halfway (its node is interior to way 1),
        // and a sidewalk runs parallel 8 m east.
        let mid = (11.3375, 44.4950);
        let main = deck(1, vec![(11.3375, 44.4943), mid, (11.3375, 44.4957)]);
        let ramp = deck(2, vec![mid, (11.3395, 44.4950), (11.3395, 44.4960)]);
        let mut walk = deck(3, vec![(11.3376, 44.4943), (11.3376, 44.4957)]);
        walk.kind = BridgeKind::Path;
        walk.width_m = 3.0;
        let f = BridgeFeatures { ways: vec![main, ramp, walk], ..Default::default() };
        let chains = merge_ways(&f);
        assert_eq!(chains.len(), 4, "main split at the junction into 2, ramp, sidewalk");
        let s = scene(20.0, &f);
        assert!(s.manifold);
        assert_eq!(s.structures.1, 0);
        let groups = s.build.report["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 2, "junction group + sidewalk group");
        let ends = |g: &serde_json::Value| -> Vec<f64> {
            g["abutment_mm"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect()
        };
        // Pieces of the junction group: [south, mid], [mid, north], [mid, ramp end].
        let (junction, walk) = (ends(&groups[0]), ends(&groups[1]));
        let mid = junction[1];
        assert_eq!(junction[2], mid);
        assert_eq!(junction[4], mid);
        let (south, north, ramp_end) = (junction[0], junction[3], junction[5]);
        let (lo, hi) = (south.min(north).min(ramp_end), south.max(north).max(ramp_end));
        assert!(mid >= lo - 1e-9 && mid <= hi + 1e-9, "harmonic mean {junction:?}");
        // The sidewalk 8 m away shares the main deck's abutments (pooled minimum).
        assert_eq!(walk, vec![south, north], "{junction:?} {walk:?}");
    }

    #[test]
    fn clip_marks_cut_ends() {
        let r = [0.0, 0.0, 10.0, 10.0];
        let pieces = clip_polyline(&[(-5.0, 5.0), (5.0, 5.0), (5.0, 15.0)], r);
        assert_eq!(pieces.len(), 1);
        let (pts, cut_start, cut_end) = &pieces[0];
        assert_eq!((*cut_start, *cut_end), (true, true));
        assert_eq!(pts, &vec![(0.0, 5.0), (5.0, 5.0), (5.0, 10.0)]);
        let inside = clip_polyline(&[(1.0, 1.0), (9.0, 9.0)], r);
        assert_eq!((inside[0].1, inside[0].2), (false, false));
        assert!(clip_polyline(&[(-5.0, -5.0), (-1.0, 20.0)], r).is_empty());
    }

    #[test]
    fn open_runs_need_two_stations() {
        assert_eq!(open_runs(&[false, true, true, false, true, false, true, true, true]), vec![(1, 2), (6, 8)]);
    }

    #[test]
    fn stacked_bases_go_back_to_buildings() {
        use crate::models::{Building, HeightSource, RoofShape};
        let base = square(11.3375, 44.4950, 0.0002);
        let building = |id: u64, fp: Vec<Pt>, part: bool, min_h: f32| Building {
            id,
            footprint: fp,
            holes: Vec::new(),
            height: 40.0,
            min_height: min_h,
            roof_shape: RoofShape::Flat,
            roof_height: 0.0,
            is_part: part,
            kind: "yes".into(),
            height_source: HeightSource::Tag,
        };
        let mut f = BridgeFeatures {
            supports: vec![
                BridgeSupport { id: 1, outer: base.clone(), role: SupportRole::Pier, height_m: Some(9.0), min_height_m: None, building: Some(building(1, base.clone(), false, 0.0)), as_building: false },
                BridgeSupport { id: 2, outer: square(11.3400, 44.4950, 0.0002), role: SupportRole::Pier, height_m: None, min_height_m: None, building: None, as_building: false },
            ],
            ..Default::default()
        };
        let mut buildings = vec![building(9, square(11.3375, 44.4950, 0.0001), true, 9.0)];
        assert_eq!(hand_back_stacked_bases(&mut f, &mut buildings), 1);
        assert_eq!(buildings.len(), 2);
        assert_eq!(f.supports.len(), 2, "still a mapped support");
        assert!(f.supports[0].as_building && f.supports[0].building.is_none());
        assert!(!f.supports[1].as_building);
    }
}
