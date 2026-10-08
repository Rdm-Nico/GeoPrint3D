//! Water geometry and hydro-flattening, in local projected metres.
//!
//! DEMs are noisy over water: lidar/radar returns over a water surface give
//! ripples, voids and rivers that run uphill, and some sources carry
//! bathymetry offshore. Cartographic DEM production fixes this with
//! "hydro-flattening", and this module applies the same rules:
//!   - the sea sits at 0 m, every lake at ONE constant level, rivers step down
//!     monotonically downstream (NGA/SRTM water-body editing, SWBD);
//!   - lakes are flat, rivers are level bank-to-bank and never flow uphill
//!     (USGS Lidar Base Specification; Esri "Hydro-flattening for DEM
//!     production" — lake level = minimum of the shoreline heights);
//!   - the level comes from a LOW PERCENTILE of the shoreline heights rather
//!     than the minimum, so one DEM pit cannot sink a whole lake (GRASS
//!     r.hydro.flatten uses the 14th percentile of the void edge);
//!   - narrow waterways are "burned" along their mapped vector line (stream
//!     burning, AGREE — Hellweger & Maidment).
//!
//! River profiles: stations every grid cell along each flow chain take a low
//! percentile of their cross-section, a running median removes short bumps
//! (bridges baked into the DEM), and weighted isotonic regression (PAVA)
//! yields the least-squares NON-INCREASING profile — unlike a running
//! minimum, one noisy pit does not drag everything downstream to its level.
//!
//! Output: the water region W (union of sea, water areas and buffered
//! waterways, clipped to the terrain hull, cleaned to printable size) whose
//! rings become constraint edges of the terrain triangulation, a level for any
//! point of W, and the DEM grid with its water samples flattened in place.

use crate::models::{ProjectedPoint, WaterArea, WaterFeatures, WaterKind, Waterway};
use crate::utils::mesh::TerrainIndex;
use crate::utils::projection::CoordinateProjector;
use anyhow::Result;
use geo::orient::Direction;
use geo::{
    unary_union, Area, BooleanOps, Buffer, Coord, LineString, MultiLineString, MultiPolygon,
    Orient, Polygon, Simplify,
};
use std::collections::{BTreeMap, HashMap};

/// Printed depth of every water surface below its shore (mm). Makes water
/// read as water on a single-colour print and gives a clean colour boundary
/// (cf. Terrain_Trails' `water_drop`).
pub const WATER_RECESS_MM: f32 = 0.6;

// Printability (all in printed mm; converted with mm_per_m).
const MIN_WATERWAY_PRINT_MM: f64 = 0.8; // two 0.4 mm extrusion lines
const MIN_STREAM_PRINT_MM: f64 = 0.25; // streams narrower than this at true scale are dropped
const MIN_WATER_FEATURE_MM: f64 = 0.4; // morphological opening: thinner slivers vanish
const MIN_WATER_AREA_MM2: f64 = 1.0; // smaller water polygons / islands are dropped
const SIMPLIFY_MM: f64 = 0.15; // Douglas–Peucker tolerance on source outlines

// Levels.
const LAKE_SHORE_PERCENTILE: f32 = 0.15;
const RIVER_SECTION_PERCENTILE: f32 = 0.25;
const PROFILE_MEDIAN_WINDOW: usize = 5;
const FIXED_STATION_WEIGHT: f64 = 1000.0; // stations inside a lake keep its level
const CLAMP_PASSES: usize = 4;
// Canal networks: level = this percentile of the stations' raw estimates.
// Buildings baked into a DEM can only RAISE samples in narrow canals, so the
// lowest observations are the clean ones.
const CANAL_NETWORK_PERCENTILE: f32 = 0.10;
// A level body smaller than this many grid cells has too few samples to be
// trusted against a canal network it is part of (it adopts the network level).
const RELIABLE_BODY_CELLS: f64 = 10.0;

type Pt = (f64, f64);

/// A water body with a single level: sea, lake, or a river area without any
/// mapped centreline in view.
struct LevelBody {
    id: u64,
    kind: WaterKind,
    shape: MultiPolygon<f64>,
    index: PolyIndex,
    level: f32,
}

/// A river/canal AREA levelled by its own centreline(s): inside it, levels
/// come only from the chains that run along it, never from a tributary whose
/// buffer happens to overlap it.
struct FlowArea {
    index: PolyIndex,
    chains: Vec<usize>,
    /// Global ids of the owning chains' stations, and a position index over
    /// them (same order) for assigning cross-section samples.
    ids: Vec<usize>,
    positions: TerrainIndex,
    /// Same stations with their final levels, for `level_at`.
    stations: Option<TerrainIndex>,
}

/// Station along a flow chain (river profile sample).
#[derive(Clone)]
struct Station {
    x: f64,
    y: f64,
    level: f32,
}

pub struct HydroModel {
    /// Water region W in local metres: its rings are the shorelines.
    pub water: MultiPolygon<f64>,
    water_index: PolyIndex,
    bodies: Vec<LevelBody>,
    stations: Vec<Station>,
    station_index: Option<TerrainIndex>,
    flow_areas: Vec<FlowArea>,
    /// Flattened DEM grid (fallback sampling for odd vertices).
    grid: TerrainIndex,
    /// Matching tolerance for "on the shore of a body" (simplification and
    /// cleaning move shorelines by up to about this much).
    tol: f64,
    pub report: serde_json::Value,
    pub features_used: usize,
}

impl HydroModel {
    /// Build the water model and flatten `points` (the (grid_dim)² row-major
    /// DEM grid, already projected) in place. Returns `None` when no printable
    /// water remains inside the terrain.
    pub fn build(
        features: &WaterFeatures,
        sea: &[WaterArea],
        sea_status: &str,
        projector: &CoordinateProjector,
        points: &mut [ProjectedPoint],
        grid_dim: usize,
        mm_per_m: f64,
    ) -> Result<Option<HydroModel>> {
        anyhow::ensure!(grid_dim >= 2 && points.len() == grid_dim * grid_dim, "hydro needs the regular DEM grid");
        let m = |mm: f64| mm / mm_per_m; // printed mm → metres
        let cell = {
            let d = |a: &ProjectedPoint, b: &ProjectedPoint| ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt();
            d(&points[0], &points[1]).max(d(&points[0], &points[grid_dim]))
        };
        let eps = m(SIMPLIFY_MM);
        let tol = (0.5 * cell).max(m(MIN_WATER_FEATURE_MM) + eps);

        // ── Terrain hull: the projected boundary ring of the DEM grid ───────
        let hull = grid_hull(points, grid_dim);
        let hull_index = PolyIndex::new(std::iter::once(hull.exterior()), cell);
        let hull_bb = hull_index.bbox;
        let margin = 2.0 * cell;
        let window = rect_polygon(hull_bb[0] - margin, hull_bb[1] - margin, hull_bb[2] + margin, hull_bb[3] + margin);
        let dem = TerrainIndex::build(points.iter().map(|p| [p.x as f32, p.y as f32, p.z]).collect());

        let project = |ring: &[Pt]| -> Result<Vec<Coord<f64>>> {
            ring.iter()
                .map(|&(lon, lat)| projector.project(lon, lat, 0.0).map(|p| Coord { x: p.x, y: p.y }))
                .collect()
        };

        // ── 1. Areas → metres, simplified, clipped to the working window ────
        let mut areas_m: Vec<(WaterKind, u64, MultiPolygon<f64>)> = Vec::new();
        let mut areas_by_kind: BTreeMap<&'static str, usize> = BTreeMap::new();
        for a in features.areas.iter().chain(sea.iter()) {
            let outer = project(&a.outer)?;
            if outer.len() < 3 {
                continue;
            }
            let holes = a
                .holes
                .iter()
                .filter(|h| h.len() >= 3)
                .map(|h| project(h).map(LineString::from))
                .collect::<Result<Vec<_>>>()?;
            let mut poly = Polygon::new(LineString::from(outer), holes);
            // The sea ring carries the DEM lattice along the bbox edges (its
            // border must stay on the hull vertices): never simplify it.
            if a.kind != WaterKind::Sea {
                poly = poly.simplify(eps);
            }
            let clipped = poly.orient(Direction::Default).intersection(&window);
            if clipped.unsigned_area() <= 0.0 {
                continue;
            }
            *areas_by_kind.entry(a.kind.name()).or_insert(0) += 1;
            areas_m.push((a.kind, a.id, clipped));
        }

        // ── 2. Waterways → kept by printability, buffered ───────────────────
        let mut kept_lines: Vec<(&Waterway, MultiLineString<f64>)> = Vec::new();
        let mut buffers: Vec<MultiPolygon<f64>> = Vec::new();
        let mut waterways_by_kind: BTreeMap<&'static str, usize> = BTreeMap::new();
        let mut waterways_dropped = 0usize;
        for w in &features.waterways {
            if w.kind == WaterKind::Stream && (w.width_m as f64) * mm_per_m < MIN_STREAM_PRINT_MM {
                waterways_dropped += 1;
                continue;
            }
            let line = LineString::from(project(&w.line)?);
            let clipped = window.clip(&MultiLineString::new(vec![line]), false);
            if clipped.0.iter().all(|l| l.0.len() < 2) {
                continue;
            }
            let width = (w.width_m as f64).max(m(MIN_WATERWAY_PRINT_MM));
            buffers.push(clipped.simplify(eps).buffer(width / 2.0));
            *waterways_by_kind.entry(w.kind.name()).or_insert(0) += 1;
            kept_lines.push((w, clipped));
        }

        // ── 3. W = (inland ∪ sea) ∩ hull, cleaned to printable size ─────────
        let inland_parts: Vec<Polygon<f64>> = areas_m
            .iter()
            .filter(|(k, _, _)| *k != WaterKind::Sea)
            .flat_map(|(_, _, mp)| mp.0.iter().cloned())
            .chain(buffers.iter().flat_map(|mp| mp.0.iter().cloned()))
            .map(|p| p.orient(Direction::Default))
            .collect();
        let mut inland = unary_union(inland_parts.iter());
        if !inland.0.is_empty() {
            // Morphological opening: drop slivers thinner than the printable
            // minimum (the sea is left alone — its bbox corners must stay sharp).
            let r = m(MIN_WATER_FEATURE_MM) / 2.0;
            inland = inland.buffer(-r).buffer(r);
        }
        let sea_parts: Vec<Polygon<f64>> = areas_m
            .iter()
            .filter(|(k, _, _)| *k == WaterKind::Sea)
            .flat_map(|(_, _, mp)| mp.0.iter().cloned())
            .map(|p| p.orient(Direction::Default))
            .collect();
        let all = if sea_parts.is_empty() { inland } else { inland.union(&unary_union(sea_parts.iter())) };
        let clipped = all.intersection(&hull);

        let min_area = MIN_WATER_AREA_MM2 / (mm_per_m * mm_per_m);
        let mut polys: Vec<Polygon<f64>> = Vec::new();
        for p in clipped.0 {
            if p.unsigned_area() < min_area {
                continue;
            }
            let (ext, ints) = p.into_inner();
            let holes: Vec<LineString<f64>> = ints
                .into_iter()
                .filter(|h| Polygon::new(h.clone(), vec![]).unsigned_area() >= min_area)
                .map(|h| densify_ring(&h, cell))
                .collect();
            polys.push(Polygon::new(densify_ring(&ext, cell), holes));
        }
        let water = MultiPolygon::new(polys);
        if water.0.is_empty() {
            return Ok(None);
        }
        let water_index = PolyIndex::new(water.0.iter().flat_map(|p| std::iter::once(p.exterior()).chain(p.interiors())), cell);

        // ── 4. Flow chains → stations ───────────────────────────────────────
        let chains = join_flow_chains(&kept_lines.iter().map(|(w, _)| (w.kind, w.line.as_slice())).collect::<Vec<_>>());
        let mut stations: Vec<Station> = Vec::new();
        let mut chain_ranges: Vec<std::ops::Range<usize>> = Vec::new();
        let chain_kinds: Vec<WaterKind> = chains.iter().map(|(k, _)| *k).collect();
        for (_, chain) in &chains {
            let start = stations.len();
            let pts = project(chain)?;
            for (x, y) in resample(&pts, cell) {
                if hull_index.contains(x, y) {
                    stations.push(Station { x, y, level: 0.0 });
                }
            }
            chain_ranges.push(start..stations.len());
        }
        let station_xy = (!stations.is_empty())
            .then(|| TerrainIndex::build(stations.iter().map(|s| [s.x as f32, s.y as f32, 0.0]).collect()));

        // ── 5. Level bodies: sea, lakes, river areas without a centreline ───
        let sea_present = areas_m.iter().any(|(k, _, _)| *k == WaterKind::Sea);
        let mut bodies: Vec<LevelBody> = Vec::new();
        let mut flow_areas: Vec<FlowArea> = Vec::new();
        let make_flow_area = |index: PolyIndex, chains: Vec<usize>| {
            let ids: Vec<usize> = chains.iter().flat_map(|&c| chain_ranges[c].clone()).collect();
            let positions =
                TerrainIndex::build(ids.iter().map(|&i| [stations[i].x as f32, stations[i].y as f32, 0.0]).collect());
            FlowArea { index, chains, ids, positions, stations: None }
        };
        // River/canal areas with no centreline of their own, resolved once
        // every area that HAS one is known.
        let mut orphans: Vec<(WaterKind, u64, MultiPolygon<f64>, PolyIndex)> = Vec::new();
        for (kind, id, shape) in areas_m {
            // Unprintable ponds/fountains are not part of W; as level bodies
            // they could only pull a neighbouring river's level around.
            if kind != WaterKind::Sea && shape.unsigned_area() < min_area {
                continue;
            }
            let index = PolyIndex::new(shape.0.iter().flat_map(|p| std::iter::once(p.exterior()).chain(p.interiors())), cell);
            if kind.is_flowing() {
                // The chains that run ALONG this area level it: most of their
                // stations inside it, and a share of the area comparable to the
                // main centreline's. A tributary's mouth, or a short stretch
                // mapped on a bridge / weir inside the river, does not.
                let inside: Vec<usize> = chain_ranges
                    .iter()
                    .map(|r| r.clone().filter(|&i| index.contains(stations[i].x, stations[i].y)).count())
                    .collect();
                let max_inside = inside.iter().copied().max().unwrap_or(0);
                let chains: Vec<usize> = (0..chain_ranges.len())
                    .filter(|&c| {
                        let len = chain_ranges[c].len();
                        len > 0 && inside[c] * 2 >= len && inside[c] * 4 >= max_inside
                    })
                    .collect();
                if chains.is_empty() {
                    orphans.push((kind, id, shape, index));
                } else {
                    flow_areas.push(make_flow_area(index, chains));
                }
                continue;
            }
            let level = if kind == WaterKind::Sea {
                0.0
            } else {
                shore_level(&shape, &index, &hull_index, &water_index, &dem, points, cell, tol)
            };
            bodies.push(LevelBody { id, kind, shape, index, level });
        }
        // An orphan is a piece of a river mapped as several areas (the reach
        // under a bridge, a side basin, a bbox-cropped bank): it belongs to the
        // river area it TOUCHES; failing that, to the chain with the most
        // stations around it; failing that, it is a level body of its own.
        for (kind, id, shape, index) in orphans {
            let verts: Vec<Coord<f64>> = shape.0.iter().flat_map(|p| p.exterior().0.iter().copied()).collect();
            let touching = flow_areas
                .iter()
                .find(|fa| verts.iter().any(|v| fa.index.contains(v.x, v.y) || fa.index.near_boundary(v.x, v.y, tol)))
                .map(|fa| fa.chains.clone());
            let chains = touching.unwrap_or_else(|| {
                let mut votes: BTreeMap<usize, usize> = BTreeMap::new();
                for (c, r) in chain_ranges.iter().enumerate() {
                    for i in r.clone() {
                        let st = &stations[i];
                        if index.contains(st.x, st.y) || index.near_boundary(st.x, st.y, 3.0 * cell) {
                            *votes.entry(c).or_insert(0) += 1;
                        }
                    }
                }
                votes.into_iter().max_by_key(|&(c, n)| (n, std::cmp::Reverse(c))).map(|(c, _)| vec![c]).unwrap_or_default()
            });
            if chains.is_empty() {
                let level = shore_level(&shape, &index, &hull_index, &water_index, &dem, points, cell, tol);
                bodies.push(LevelBody { id, kind, shape, index, level });
            } else {
                flow_areas.push(make_flow_area(index, chains));
            }
        }
        // Standing water open to the sea (harbour basins, lagoons, river
        // mouths mapped as areas) is at sea level, whatever its quays say.
        if sea_present {
            let sea_index = PolyIndex::new(
                bodies
                    .iter()
                    .filter(|b| b.kind == WaterKind::Sea)
                    .flat_map(|b| b.shape.0.iter())
                    .flat_map(|p| std::iter::once(p.exterior()).chain(p.interiors())),
                cell,
            );
            let touch = eps + 0.5;
            for b in bodies.iter_mut().filter(|b| b.kind != WaterKind::Sea) {
                let verts: Vec<Coord<f64>> = b.shape.0.iter().flat_map(|p| p.exterior().0.iter().copied()).collect();
                let at_sea = verts
                    .iter()
                    .filter(|c| sea_index.contains(c.x, c.y) || sea_index.near_boundary(c.x, c.y, touch))
                    .count();
                if !verts.is_empty() && at_sea * 10 >= verts.len() {
                    b.level = 0.0;
                }
            }
        }

        // ── 6. River profiles ───────────────────────────────────────────────
        if let Some(sidx) = &station_xy {
            // Cross-section samples: water grid points outside level bodies,
            // assigned to their nearest station.
            let mut samples: Vec<Vec<f32>> = vec![Vec::new(); stations.len()];
            let mut raw_levels: Vec<f32> = vec![0.0; stations.len()];
            for p in points.iter() {
                if in_closed(&water_index, p.x, p.y) && !bodies.iter().any(|b| b.index.contains(p.x, p.y)) {
                    // Inside a river area: its own centreline's stations only.
                    let own = flow_areas
                        .iter()
                        .find(|fa| fa.index.contains(p.x, p.y))
                        .and_then(|fa| fa.positions.nearest_index(p.x as f32, p.y as f32).map(|k| fa.ids[k]));
                    if let Some(s) = own.or_else(|| sidx.nearest_index(p.x as f32, p.y as f32)) {
                        samples[s].push(p.z);
                    }
                }
            }
            for range in &chain_ranges {
                if range.is_empty() {
                    continue;
                }
                let mut raw: Vec<f64> = Vec::with_capacity(range.len());
                let mut fixed: Vec<Option<f32>> = Vec::with_capacity(range.len());
                for i in range.clone() {
                    let s = &stations[i];
                    let lake = bodies
                        .iter()
                        .filter(|b| b.index.contains(s.x, s.y))
                        .map(|b| b.level)
                        .fold(None, |acc: Option<f32>, l| Some(acc.map_or(l, |a| a.min(l))));
                    fixed.push(lake);
                    let v = if samples[i].len() >= 3 {
                        percentile(&mut samples[i], RIVER_SECTION_PERCENTILE)
                    } else {
                        dem.nearest_z(s.x as f32, s.y as f32).unwrap_or(0.0)
                    };
                    raw_levels[i] = v;
                    raw.push(v as f64);
                }
                let mut smooth = running_median(&raw, PROFILE_MEDIAN_WINDOW);
                let mut weights = vec![1.0; smooth.len()];
                for (k, f) in fixed.iter().enumerate() {
                    if let Some(l) = f {
                        smooth[k] = *l as f64;
                        weights[k] = FIXED_STATION_WEIGHT;
                    }
                }
                let profile = pava_non_increasing(&smooth, &weights);
                for (k, i) in range.clone().enumerate() {
                    stations[i].level = profile[k] as f32;
                }
            }

            // Canals are still water: a connected canal network holds ONE level
            // (Venice, Amsterdam). It opens onto the sea or a large lake at
            // that water's level; otherwise its level is a low percentile of
            // its stations' raw estimates. Small basins it touches are part of
            // the same still water and adopt the network level. Without this,
            // DEM noise in narrow canals shows up as steps where canals meet.
            // Rivers keep their sloping profile.
            let groups = canal_groups(&chain_kinds, &chain_ranges, &stations, 1.5 * cell);
            let reliable_area = RELIABLE_BODY_CELLS * cell * cell;
            for group in groups.iter().filter(|g| chain_kinds[g[0]] == WaterKind::Canal) {
                let members: Vec<usize> = group.iter().flat_map(|&c| chain_ranges[c].clone()).collect();
                let touched: Vec<usize> = (0..bodies.len())
                    .filter(|&k| {
                        let b = &bodies[k];
                        members.iter().any(|&i| {
                            let s = &stations[i];
                            b.index.contains(s.x, s.y) || b.index.near_boundary(s.x, s.y, tol)
                        })
                    })
                    .collect();
                let is_reliable =
                    |b: &LevelBody| b.kind == WaterKind::Sea || b.shape.unsigned_area() >= reliable_area;
                let opens_onto = touched
                    .iter()
                    .filter(|&&k| is_reliable(&bodies[k]))
                    .map(|&k| bodies[k].level)
                    .fold(None, |acc: Option<f32>, l| Some(acc.map_or(l, |a| a.min(l))));
                let level = opens_onto.unwrap_or_else(|| {
                    let mut v: Vec<f32> = members.iter().map(|&i| raw_levels[i]).collect();
                    percentile(&mut v, CANAL_NETWORK_PERCENTILE)
                });
                for &i in &members {
                    stations[i].level = level;
                }
                for &k in &touched {
                    if !is_reliable(&bodies[k]) {
                        bodies[k].level = level;
                    }
                }
            }

            // A chain running mostly INSIDE another river's area without owning
            // it (side channel, weir, fish ladder, a stretch mapped on a bridge)
            // shares that river's level along its whole length.
            for fa in &flow_areas {
                for (c, r) in chain_ranges.iter().enumerate() {
                    if fa.chains.contains(&c) || r.is_empty() {
                        continue;
                    }
                    let inside = r.clone().filter(|&i| fa.index.contains(stations[i].x, stations[i].y)).count();
                    if inside * 2 < r.len() {
                        continue;
                    }
                    for i in r.clone() {
                        if let Some(k) = fa.positions.nearest_index(stations[i].x as f32, stations[i].y as f32) {
                            stations[i].level = stations[fa.ids[k]].level;
                        }
                    }
                }
            }

            // Receiving water is never above the water flowing into it: raise
            // a group (a river chain, or a whole canal network) to the level of
            // whatever receives it at its mouth(s).
            let mut group_of = vec![0usize; chain_ranges.len()];
            for (g, members) in groups.iter().enumerate() {
                for &c in members {
                    group_of[c] = g;
                }
            }
            for _ in 0..CLAMP_PASSES {
                let mut changed = false;
                for (g, members) in groups.iter().enumerate() {
                    let mut recv: Option<f32> = None;
                    for &ci in members {
                        let Some(last) = chain_ranges[ci].clone().last() else { continue };
                        let (ex, ey) = (stations[last].x, stations[last].y);
                        let mut take = |l: f32| recv = Some(recv.map_or(l, |r: f32| r.min(l)));
                        for b in &bodies {
                            if b.index.contains(ex, ey) || b.index.near_boundary(ex, ey, tol) {
                                take(b.level);
                            }
                        }
                        for (cj, r2) in chain_ranges.iter().enumerate() {
                            if group_of[cj] == g || r2.is_empty() {
                                continue;
                            }
                            // A chain's own mouth station belongs upstream of
                            // the junction, never to the receiving side.
                            for i in r2.start..r2.end - 1 {
                                let s = &stations[i];
                                if (s.x - ex).powi(2) + (s.y - ey).powi(2) <= (1.5 * cell).powi(2) {
                                    take(s.level);
                                }
                            }
                        }
                    }
                    if let Some(r) = recv {
                        for &ci in members {
                            for i in chain_ranges[ci].clone() {
                                if stations[i].level < r {
                                    stations[i].level = r;
                                    changed = true;
                                }
                            }
                        }
                    }
                }
                if !changed {
                    break;
                }
            }
            if sea_present {
                for s in &mut stations {
                    s.level = s.level.max(0.0);
                }
            }
        }
        let station_index = (!stations.is_empty())
            .then(|| TerrainIndex::build(stations.iter().map(|s| [s.x as f32, s.y as f32, s.level]).collect()));
        for fa in &mut flow_areas {
            let own: Vec<[f32; 3]> =
                fa.ids.iter().map(|&i| [stations[i].x as f32, stations[i].y as f32, stations[i].level]).collect();
            fa.stations = (!own.is_empty()).then(|| TerrainIndex::build(own));
        }

        let mut model = HydroModel {
            water,
            water_index,
            bodies,
            stations,
            station_index,
            flow_areas,
            grid: dem,
            tol,
            report: serde_json::Value::Null,
            features_used: 0,
        };

        // ── 7. Flatten the DEM grid inside W ────────────────────────────────
        let mut flattened = 0usize;
        for p in points.iter_mut() {
            // Closed test: grid points ON W's border (the hull edge under the
            // sea, a shore through a grid point) are water too.
            if in_closed(&model.water_index, p.x, p.y) {
                p.z = model.level_at(p.x, p.y);
                flattened += 1;
            }
        }
        model.grid = TerrainIndex::build(points.iter().map(|p| [p.x as f32, p.y as f32, p.z]).collect());

        // ── Report ──────────────────────────────────────────────────────────
        let hull_area = hull.unsigned_area().max(1.0);
        let mut profiles = Vec::new();
        for range in &chain_ranges {
            if range.len() >= 2 {
                profiles.push(serde_json::json!({
                    "stations": range.len(),
                    "upstream_level_m": model.stations[range.start].level,
                    "downstream_level_m": model.stations[range.end - 1].level,
                }));
            }
        }
        model.features_used = areas_by_kind.values().sum::<usize>() + kept_lines.len();
        model.report = serde_json::json!({
            "sea": sea_status,
            "areas_by_kind": areas_by_kind,
            "waterways_by_kind": waterways_by_kind,
            "waterways_dropped_unprintable": waterways_dropped,
            "water_polygons": model.water.0.len(),
            "water_area_pct": 100.0 * model.water.unsigned_area() / hull_area,
            "grid_points_flattened": flattened,
            "stations": model.stations.len(),
            "waterway_ids": kept_lines.iter().take(50).map(|(w, _)| w.id).collect::<Vec<_>>(),
            "bodies": model.bodies.iter().take(30).map(|b| serde_json::json!({
                "osm_id": b.id,
                "kind": b.kind.name(),
                "level_m": b.level,
                "area_m2": b.shape.unsigned_area(),
            })).collect::<Vec<_>>(),
            "river_profiles": profiles,
        });
        Ok(Some(model))
    }

    /// True when (x, y) lies inside the water region W.
    pub fn is_water(&self, x: f64, y: f64) -> bool {
        self.water_index.contains(x, y)
    }

    /// Water surface level (true metres) at a point of W (or on its shore):
    /// the containing level body (lowest one on shared shores), otherwise the
    /// nearest river-profile station, otherwise the flattened DEM.
    pub fn level_at(&self, x: f64, y: f64) -> f32 {
        let mut best: Option<f32> = None;
        for b in &self.bodies {
            if b.index.contains(x, y) || b.index.near_boundary(x, y, self.tol) {
                best = Some(best.map_or(b.level, |l| l.min(b.level)));
            }
        }
        if let Some(l) = best {
            return l;
        }
        // Inside a river/canal area: its own centreline profile.
        for fa in &self.flow_areas {
            if fa.index.contains(x, y) {
                if let Some(z) = fa.stations.as_ref().and_then(|s| s.nearest_z(x as f32, y as f32)) {
                    best = Some(best.map_or(z, |l| l.min(z)));
                }
            }
        }
        if let Some(l) = best {
            return l;
        }
        if let Some(z) = self.station_index.as_ref().and_then(|s| s.nearest_z(x as f32, y as f32)) {
            return z;
        }
        self.grid.nearest_z(x as f32, y as f32).unwrap_or(0.0)
    }

    /// Flattened DEM elevation nearest to (x, y) (true metres).
    pub fn ground_z(&self, x: f64, y: f64) -> Option<f32> {
        self.grid.nearest_z(x as f32, y as f32)
    }

    pub fn summary(&self) -> String {
        let sea = self.bodies.iter().filter(|b| b.kind == WaterKind::Sea).count();
        let lakes = self.bodies.len() - sea;
        format!(
            "{} water polygon(s), {} sea body(ies), {} level body(ies), {} profile station(s)",
            self.water.0.len(), sea, lakes, self.stations.len()
        )
    }
}

// ── Levels ────────────────────────────────────────────────────────────────────

/// Level of a standing water body: a low percentile of the DEM along its real
/// shoreline. Shore samples are skipped where the outline is an artificial
/// cut (outside / on the terrain hull) or runs through other water (a lake
/// edge shared with a river area). Falls back to the median DEM inside the
/// body when no shore is in view (bbox entirely inside a lake).
#[allow(clippy::too_many_arguments)]
fn shore_level(
    shape: &MultiPolygon<f64>,
    index: &PolyIndex,
    hull_index: &PolyIndex,
    water_index: &PolyIndex,
    dem: &TerrainIndex,
    points: &[ProjectedPoint],
    cell: f64,
    tol: f64,
) -> f32 {
    let mut shore: Vec<f32> = Vec::new();
    for poly in &shape.0 {
        for ring in std::iter::once(poly.exterior()).chain(poly.interiors()) {
            let pts: Vec<Coord<f64>> = ring.0.clone();
            for (x, y) in resample(&pts, 0.5 * cell) {
                if !hull_index.contains(x, y) || hull_index.near_boundary(x, y, 0.5 * cell) {
                    continue;
                }
                if water_index.contains(x, y) && !water_index.near_boundary(x, y, 2.0 * tol) {
                    continue;
                }
                if let Some(z) = dem.nearest_z(x as f32, y as f32) {
                    shore.push(z);
                }
            }
        }
    }
    if !shore.is_empty() {
        return percentile(&mut shore, LAKE_SHORE_PERCENTILE);
    }
    let mut inside: Vec<f32> = points.iter().filter(|p| index.contains(p.x, p.y)).map(|p| p.z).collect();
    if !inside.is_empty() {
        return percentile(&mut inside, 0.5);
    }
    let c = shape.0[0].exterior().0[0];
    dem.nearest_z(c.x as f32, c.y as f32).unwrap_or(0.0)
}

/// Nearest-rank percentile (p in [0, 1]); sorts in place.
fn percentile(v: &mut [f32], p: f32) -> f32 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    v[((v.len() - 1) as f32 * p).round() as usize]
}

/// Centred running median (window shrinks at the ends).
fn running_median(v: &[f64], window: usize) -> Vec<f64> {
    let h = window / 2;
    (0..v.len())
        .map(|i| {
            let lo = i.saturating_sub(h);
            let hi = (i + h + 1).min(v.len());
            let mut w: Vec<f64> = v[lo..hi].to_vec();
            w.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            w[w.len() / 2]
        })
        .collect()
}

/// Weighted isotonic regression (pool-adjacent-violators) constrained to a
/// NON-INCREASING sequence: the least-squares monotone fit, so water never
/// flows uphill while noise is averaged instead of clipped.
fn pava_non_increasing(values: &[f64], weights: &[f64]) -> Vec<f64> {
    // Blocks of pooled values: (weighted sum, weight, count).
    let mut blocks: Vec<(f64, f64, usize)> = Vec::with_capacity(values.len());
    for (&v, &w) in values.iter().zip(weights) {
        blocks.push((v * w, w, 1));
        while blocks.len() >= 2 {
            let n = blocks.len();
            let prev = blocks[n - 2].0 / blocks[n - 2].1;
            let last = blocks[n - 1].0 / blocks[n - 1].1;
            if prev >= last {
                break;
            }
            let b = blocks.pop().unwrap();
            let a = blocks.last_mut().unwrap();
            a.0 += b.0;
            a.1 += b.1;
            a.2 += b.2;
        }
    }
    blocks.iter().flat_map(|&(s, w, n)| std::iter::repeat(s / w).take(n)).collect()
}

// ── Flow chains ───────────────────────────────────────────────────────────────

fn node_key(p: Pt) -> (u64, u64) {
    (p.0.to_bits(), p.1.to_bits())
}

/// Join waterway ways (drawn in flow direction) into maximal simple chains of
/// one kind: way A continues into way B only where exactly one way ends and
/// exactly one starts at the shared node, and both are the same kind —
/// confluences and bifurcations split chains, and their consistency is
/// restored by the receiving-water clamp.
fn join_flow_chains(lines: &[(WaterKind, &[Pt])]) -> Vec<(WaterKind, Vec<Pt>)> {
    let lines: Vec<(WaterKind, &[Pt])> = lines.iter().copied().filter(|(_, l)| l.len() >= 2).collect();
    let mut starts: HashMap<(u64, u64), Vec<usize>> = HashMap::new();
    let mut ends: HashMap<(u64, u64), usize> = HashMap::new();
    for (i, (_, l)) in lines.iter().enumerate() {
        starts.entry(node_key(l[0])).or_default().push(i);
        *ends.entry(node_key(*l.last().unwrap())).or_insert(0) += 1;
    }
    let successor = |i: usize| -> Option<usize> {
        let k = node_key(*lines[i].1.last().unwrap());
        match (ends.get(&k), starts.get(&k)) {
            (Some(1), Some(s)) if s.len() == 1 && s[0] != i && lines[s[0]].0 == lines[i].0 => Some(s[0]),
            _ => None,
        }
    };
    let mut has_pred = vec![false; lines.len()];
    for i in 0..lines.len() {
        if let Some(j) = successor(i) {
            has_pred[j] = true;
        }
    }

    let mut used = vec![false; lines.len()];
    let mut chains = Vec::new();
    let order: Vec<usize> = (0..lines.len()).filter(|&i| !has_pred[i]).chain(0..lines.len()).collect();
    for i in order {
        if used[i] {
            continue;
        }
        used[i] = true;
        let mut chain: Vec<Pt> = lines[i].1.to_vec();
        let mut cur = i;
        while let Some(j) = successor(cur) {
            if used[j] {
                break;
            }
            used[j] = true;
            chain.extend(lines[j].1.iter().skip(1).copied());
            cur = j;
        }
        chains.push((lines[i].0, chain));
    }
    chains
}

/// Group chains for levelling: every canal chain joins the canal chains it
/// touches (an end within `reach` of another canal's station) into one
/// network; every other chain is its own group.
fn canal_groups(
    kinds: &[WaterKind],
    ranges: &[std::ops::Range<usize>],
    stations: &[Station],
    reach: f64,
) -> Vec<Vec<usize>> {
    let n = kinds.len();
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(p: &mut [usize], mut x: usize) -> usize {
        while p[x] != x {
            p[x] = p[p[x]];
            x = p[x];
        }
        x
    }
    let canals: Vec<usize> = (0..n).filter(|&c| kinds[c] == WaterKind::Canal && !ranges[c].is_empty()).collect();
    for &a in &canals {
        let ends = [ranges[a].start, ranges[a].end - 1];
        for &b in &canals {
            if a == b {
                continue;
            }
            let touches = ends.iter().any(|&e| {
                ranges[b].clone().any(|i| {
                    (stations[i].x - stations[e].x).powi(2) + (stations[i].y - stations[e].y).powi(2) <= reach * reach
                })
            });
            if touches {
                let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
                parent[ra] = rb;
            }
        }
    }
    let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for c in 0..n {
        let r = find(&mut parent, c);
        groups.entry(r).or_default().push(c);
    }
    groups.into_values().collect()
}

/// Points every `step` metres along a polyline (both ends included).
fn resample(pts: &[Coord<f64>], step: f64) -> Vec<Pt> {
    let mut out = Vec::new();
    if pts.is_empty() {
        return out;
    }
    out.push((pts[0].x, pts[0].y));
    let mut carry = 0.0;
    for w in pts.windows(2) {
        let (a, b) = (w[0], w[1]);
        let len = ((b.x - a.x).powi(2) + (b.y - a.y).powi(2)).sqrt();
        if len <= 0.0 {
            continue;
        }
        let mut d = step - carry;
        while d < len {
            let t = d / len;
            out.push((a.x + (b.x - a.x) * t, a.y + (b.y - a.y) * t));
            d += step;
        }
        carry = len - (d - step);
    }
    let last = pts[pts.len() - 1];
    if out.last() != Some(&(last.x, last.y)) {
        out.push((last.x, last.y));
    }
    out
}

// ── Geometry helpers ──────────────────────────────────────────────────────────

/// Counter-clockwise ring of the DEM grid's boundary points (row-major grid,
/// row 0 = south, column 0 = west): exactly the terrain triangulation hull.
fn grid_hull(points: &[ProjectedPoint], n: usize) -> Polygon<f64> {
    let at = |i: usize, j: usize| Coord { x: points[i * n + j].x, y: points[i * n + j].y };
    let mut ring = Vec::with_capacity(4 * n);
    for j in 0..n - 1 {
        ring.push(at(0, j)); // south, west → east
    }
    for i in 0..n - 1 {
        ring.push(at(i, n - 1)); // east, south → north
    }
    for j in (1..n).rev() {
        ring.push(at(n - 1, j)); // north, east → west
    }
    for i in (1..n).rev() {
        ring.push(at(i, 0)); // west, north → south
    }
    Polygon::new(LineString::from(ring), vec![])
}

fn rect_polygon(x0: f64, y0: f64, x1: f64, y1: f64) -> Polygon<f64> {
    Polygon::new(
        LineString::from(vec![
            Coord { x: x0, y: y0 },
            Coord { x: x1, y: y0 },
            Coord { x: x1, y: y1 },
            Coord { x: x0, y: y1 },
        ]),
        vec![],
    )
}

/// Insert points so no ring edge is longer than `max_len`, and drop
/// near-duplicate consecutive vertices.
fn densify_ring(ring: &LineString<f64>, max_len: f64) -> LineString<f64> {
    const MIN_GAP: f64 = 1e-3;
    let mut out: Vec<Coord<f64>> = Vec::with_capacity(ring.0.len());
    for w in ring.0.windows(2) {
        let (a, b) = (w[0], w[1]);
        if out.last().map_or(true, |l: &Coord<f64>| (l.x - a.x).abs() > MIN_GAP || (l.y - a.y).abs() > MIN_GAP) {
            out.push(a);
        }
        let len = ((b.x - a.x).powi(2) + (b.y - a.y).powi(2)).sqrt();
        let n = (len / max_len).ceil() as usize;
        for k in 1..n {
            let t = k as f64 / n as f64;
            out.push(Coord { x: a.x + (b.x - a.x) * t, y: a.y + (b.y - a.y) * t });
        }
    }
    LineString::from(out) // geo closes the ring
}

/// Edge-bucket index for fast point-in-polygon (even-odd over all rings) and
/// "near the boundary" queries: edges are bucketed by the horizontal bands
/// their y-range spans, so a query only looks at the edges of its band.
struct PolyIndex {
    bbox: [f64; 4],
    inv_h: f64,
    buckets: Vec<Vec<[f64; 4]>>,
}

impl PolyIndex {
    fn new<'a>(rings: impl Iterator<Item = &'a LineString<f64>>, band: f64) -> Self {
        let edges: Vec<[f64; 4]> = rings
            .flat_map(|r| r.0.windows(2).map(|w| [w[0].x, w[0].y, w[1].x, w[1].y]).collect::<Vec<_>>())
            .collect();
        let mut bbox = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
        for e in &edges {
            bbox[0] = bbox[0].min(e[0].min(e[2]));
            bbox[1] = bbox[1].min(e[1].min(e[3]));
            bbox[2] = bbox[2].max(e[0].max(e[2]));
            bbox[3] = bbox[3].max(e[1].max(e[3]));
        }
        if edges.is_empty() {
            return Self { bbox: [0.0, 0.0, -1.0, -1.0], inv_h: 1.0, buckets: Vec::new() };
        }
        let band = band.max(1e-3);
        let n = (((bbox[3] - bbox[1]) / band).floor() as usize + 1).min(100_000);
        let inv_h = n as f64 / (bbox[3] - bbox[1]).max(1e-9);
        let mut buckets = vec![Vec::new(); n];
        for e in edges {
            let lo = (((e[1].min(e[3]) - bbox[1]) * inv_h) as usize).min(n - 1);
            let hi = (((e[1].max(e[3]) - bbox[1]) * inv_h) as usize).min(n - 1);
            for b in &mut buckets[lo..=hi] {
                b.push(e);
            }
        }
        Self { bbox, inv_h, buckets }
    }

    fn band(&self, y: f64) -> Option<usize> {
        if self.buckets.is_empty() || y < self.bbox[1] || y > self.bbox[3] {
            return None;
        }
        Some((((y - self.bbox[1]) * self.inv_h) as usize).min(self.buckets.len() - 1))
    }

    fn contains(&self, x: f64, y: f64) -> bool {
        if x < self.bbox[0] || x > self.bbox[2] {
            return false;
        }
        let Some(b) = self.band(y) else { return false };
        let mut inside = false;
        for e in &self.buckets[b] {
            if (e[1] > y) != (e[3] > y) {
                let xc = e[0] + (y - e[1]) / (e[3] - e[1]) * (e[2] - e[0]);
                if x < xc {
                    inside = !inside;
                }
            }
        }
        inside
    }

    fn near_boundary(&self, x: f64, y: f64, tol: f64) -> bool {
        if self.buckets.is_empty()
            || x < self.bbox[0] - tol
            || x > self.bbox[2] + tol
            || y < self.bbox[1] - tol
            || y > self.bbox[3] + tol
        {
            return false;
        }
        let lo = (((y - tol - self.bbox[1]).max(0.0) * self.inv_h) as usize).min(self.buckets.len() - 1);
        let hi = (((y + tol - self.bbox[1]).max(0.0) * self.inv_h) as usize).min(self.buckets.len() - 1);
        let tol2 = tol * tol;
        self.buckets[lo..=hi].iter().flatten().any(|e| seg_dist2(x, y, e) <= tol2)
    }
}

/// Point in the CLOSED region (interior or within 1 mm of the boundary).
fn in_closed(index: &PolyIndex, x: f64, y: f64) -> bool {
    index.contains(x, y) || index.near_boundary(x, y, 1e-3)
}

fn seg_dist2(px: f64, py: f64, e: &[f64; 4]) -> f64 {
    let (dx, dy) = (e[2] - e[0], e[3] - e[1]);
    let len2 = dx * dx + dy * dy;
    let t = if len2 > 0.0 { (((px - e[0]) * dx + (py - e[1]) * dy) / len2).clamp(0.0, 1.0) } else { 0.0 };
    let (cx, cy) = (e[0] + t * dx, e[1] + t * dy);
    (px - cx).powi(2) + (py - cy).powi(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pava_flattens_uphill_bump_and_never_rises() {
        let v = [10.0, 9.0, 9.5, 8.0, 8.5, 7.0];
        let out = pava_non_increasing(&v, &[1.0; 6]);
        assert!(out.windows(2).all(|w| w[0] >= w[1]), "{out:?}");
        // Violating pairs are pooled to their mean.
        assert!((out[1] - 9.25).abs() < 1e-9 && (out[2] - 9.25).abs() < 1e-9);
        assert!((out[3] - 8.25).abs() < 1e-9 && (out[4] - 8.25).abs() < 1e-9);
        // Mean preserved (least-squares fit).
        assert!((out.iter().sum::<f64>() - v.iter().sum::<f64>()).abs() < 1e-9);
    }

    #[test]
    fn pava_respects_heavy_fixed_station() {
        // A lake station (heavy) at 5.0 between noisy river stations.
        let out = pava_non_increasing(&[6.0, 4.8, 5.0, 4.0], &[1.0, 1.0, 1000.0, 1.0]);
        assert!(out.windows(2).all(|w| w[0] >= w[1]));
        assert!((out[2] - 5.0).abs() < 0.01, "{out:?}");
    }

    #[test]
    fn running_median_removes_single_bridge_spike() {
        let v = [5.0, 4.9, 9.0, 4.8, 4.7];
        let out = running_median(&v, 5);
        assert!(out.iter().all(|&x| x < 6.0), "{out:?}");
    }

    #[test]
    fn flow_chains_split_at_confluence() {
        let a: Vec<Pt> = vec![(0.0, 0.0), (1.0, 0.0)];
        let b: Vec<Pt> = vec![(1.0, 0.0), (2.0, 0.0)]; // continues a
        let trib: Vec<Pt> = vec![(2.0, 1.0), (2.0, 0.0)]; // joins at (2,0)
        let c: Vec<Pt> = vec![(2.0, 0.0), (3.0, 0.0)];
        let r = WaterKind::River;
        let chains = join_flow_chains(&[(r, &a), (r, &b), (r, &trib), (r, &c)]);
        assert_eq!(chains.len(), 3, "{chains:?}");
        assert!(chains.iter().any(|(_, ch)| ch.len() == 3 && ch[0] == (0.0, 0.0) && ch[2] == (2.0, 0.0)));
        // A river does not continue into a canal.
        let mixed = join_flow_chains(&[(r, &a), (WaterKind::Canal, &b)]);
        assert_eq!(mixed.len(), 2);
    }

    #[test]
    fn poly_index_contains_and_boundary() {
        let sq = rect_polygon(0.0, 0.0, 10.0, 10.0);
        let hole = LineString::from(vec![(4.0, 4.0), (6.0, 4.0), (6.0, 6.0), (4.0, 6.0), (4.0, 4.0)]);
        let poly = Polygon::new(sq.exterior().clone(), vec![hole]);
        let idx = PolyIndex::new(std::iter::once(poly.exterior()).chain(poly.interiors()), 1.0);
        assert!(idx.contains(1.0, 1.0));
        assert!(!idx.contains(5.0, 5.0));
        assert!(!idx.contains(11.0, 5.0));
        assert!(idx.near_boundary(10.2, 5.0, 0.5));
        assert!(!idx.near_boundary(2.0, 2.0, 0.5));
    }

    #[test]
    fn resample_spacing() {
        let pts = vec![Coord { x: 0.0, y: 0.0 }, Coord { x: 10.0, y: 0.0 }];
        let r = resample(&pts, 2.5);
        assert_eq!(r.len(), 5);
        assert_eq!(r[1], (2.5, 0.0));
        assert_eq!(*r.last().unwrap(), (10.0, 0.0));
    }
}
