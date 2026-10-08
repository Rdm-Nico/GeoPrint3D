//! Contextual height inference for OSM buildings without height data.
//!
//! In most Italian (and European) cities >90% of OSM outlines carry no
//! `height` / `building:levels`, so the plain per-type default decides the
//! skyline: `building=yes` → 8 m next to `apartments` → 15 m, a 1:2
//! checkerboard driven by tagging habits rather than the real city.
//!
//! Following the urban-form literature (Biljecki et al. 2017, "Generating 3D
//! city models without elevation data"; Milojevic-Dupont et al. 2020,
//! "Learning from urban form to predict building heights": the morphology of
//! the surrounding fabric is highly predictive, and a little local height data
//! helps a lot), each untagged generic outline gets:
//!   1. a density prior — storeys interpolated within a per-type range by the
//!      built-up ratio around the building (dense historic core → tall,
//!      sparse suburb → low);
//!   2. blended with tagged neighbours within NEIGHBOR_RADIUS_M (IDW median);
//!   3. harmonised with the rest of its block (outlines sharing nodes), so
//!      street walls read as continuous like real terraced fabric.
//!
//! Only `TypeDefault` heights of non-part outlines with a generic type are
//! touched; tagged data, building:part elements and special types (churches,
//! garages, industrial…) are left alone.

use crate::models::{BoundingBox, Building, HeightSource};
use std::collections::HashMap;

const LEVEL_M: f32 = 3.2; // must match services::osm (building:levels → metres)

// Density prior: built-up ratio measured in a disk of this radius, mapped
// onto the type's storey range between DENSITY_LO (sparse) and DENSITY_HI
// (dense core). Calibrated leave-one-out on 460 tagged Bologna outlines: the
// measured height depends only weakly on density (bin medians 12.8–16 m), so
// the ranges are deliberately narrow; tagged neighbours carry most of the
// signal (MAE 7.7 m type defaults → 6.2 m with neighbours + blocks).
const DENSITY_RADIUS_M: f64 = 100.0;
const DENSITY_LO: f32 = 0.10;
const DENSITY_HI: f32 = 0.60;

// Tagged-neighbour evidence.
const NEIGHBOR_RADIUS_M: f64 = 150.0;
const MIN_NEIGHBORS: usize = 3;
const MAX_NEIGHBORS_WEIGHT: f32 = 10.0; // cap on n in the blend
const PRIOR_WEIGHT: f32 = 3.0; // prior counts as this many neighbours
const IDW_OFFSET_M: f64 = 20.0;

// Block harmonisation: members of the same block within this radius vote.
const BLOCK_RADIUS_M: f64 = 60.0;
const BLOCK_MEASURED_CAP: f32 = 5.0;
const BLOCK_INFERRED_WEIGHT: f32 = 2.0;

// Footprint guards.
const BIG_BOX_MIN_AREA_M2: f64 = 2500.0;
const BIG_BOX_MAX_DENSITY: f32 = 0.35;
const BIG_BOX_HEIGHT_M: f32 = 9.0;
const TINY_MAX_AREA_M2: f64 = 40.0;
const TINY_MAX_LEVELS: f32 = 2.0;

const JITTER: f32 = 0.04; // ± fraction, deterministic per OSM id

const GRID_CELL_M: f64 = 100.0;

/// Storey range (lo = sparse context, hi = dense core) for types whose height
/// is inferred; `None` keeps the per-type default (garages, churches, sheds…).
fn level_range(kind: &str) -> Option<(f32, f32)> {
    match kind {
        "apartments" | "dormitory" => Some((3.5, 6.0)),
        "yes" | "residential" | "commercial" | "office" | "hotel" | "mixed_use" | "building" => {
            Some((2.5, 5.0))
        }
        "retail" => Some((1.5, 4.0)),
        "terrace" => Some((2.0, 4.0)),
        "house" | "detached" | "semidetached_house" | "bungalow" => Some((1.5, 3.0)),
        "school" | "university" | "college" | "kindergarten" | "hospital" | "public" | "civic"
        | "government" => Some((2.0, 4.0)),
        _ => None,
    }
}

fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Deterministic ±JITTER multiplier seeded by OSM id (stable across runs).
fn jitter(id: u64) -> f32 {
    let h = (id.wrapping_mul(2_654_435_761) % 1000) as f32 / 1000.0; // [0,1)
    1.0 - JITTER + 2.0 * JITTER * h
}

/// Weighted median of (value, weight) samples; `None` when empty.
fn weighted_median(samples: &mut [(f32, f32)]) -> Option<f32> {
    if samples.is_empty() {
        return None;
    }
    samples.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let total: f32 = samples.iter().map(|s| s.1).sum();
    let mut acc = 0.0;
    for &(v, w) in samples.iter() {
        acc += w;
        if acc >= total * 0.5 {
            return Some(v);
        }
    }
    samples.last().map(|s| s.0)
}

/// Summary of one inference run, for logs and the geometry report.
#[derive(Debug, Default, Clone)]
pub struct InferenceReport {
    pub inferred: usize,
    pub by_source: Vec<(&'static str, usize)>,
    pub blocks: usize,
    /// Leave-one-out check on measured generic outlines:
    /// (samples, MAE of this estimator, MAE of the plain type default) in metres.
    pub loo: Option<(usize, f32, f32)>,
}

/// Per-element geometry in a local metric frame.
struct Geo {
    cx: f64,
    cy: f64,
    area: f64,
}

struct Ctx<'a> {
    buildings: &'a [Building],
    geo: Vec<Geo>,
    grid: HashMap<(i32, i32), Vec<usize>>,
    bbox_m: [f64; 4], // min_x, min_y, max_x, max_y of the request bbox
}

impl<'a> Ctx<'a> {
    fn new(buildings: &'a [Building], bbox: &BoundingBox) -> Self {
        let lat0 = (bbox.min_lat + bbox.max_lat) * 0.5;
        let lon0 = (bbox.min_lon + bbox.max_lon) * 0.5;
        let kx = 111_320.0 * lat0.to_radians().cos();
        let ky = 111_320.0;
        let to_m = |(lon, lat): (f64, f64)| ((lon - lon0) * kx, (lat - lat0) * ky);

        let ring_area_centroid = |ring: &[(f64, f64)]| -> (f64, f64, f64) {
            let pts: Vec<(f64, f64)> = ring.iter().map(|&p| to_m(p)).collect();
            let n = pts.len();
            let (mut a, mut cx, mut cy) = (0.0, 0.0, 0.0);
            for i in 0..n {
                let (x0, y0) = pts[i];
                let (x1, y1) = pts[(i + 1) % n];
                let cross = x0 * y1 - x1 * y0;
                a += cross;
                cx += (x0 + x1) * cross;
                cy += (y0 + y1) * cross;
            }
            if a.abs() < 1e-9 {
                let inv = 1.0 / n.max(1) as f64;
                return (
                    0.0,
                    pts.iter().map(|p| p.0).sum::<f64>() * inv,
                    pts.iter().map(|p| p.1).sum::<f64>() * inv,
                );
            }
            (a.abs() * 0.5, cx / (3.0 * a), cy / (3.0 * a))
        };

        let mut geo = Vec::with_capacity(buildings.len());
        let mut grid: HashMap<(i32, i32), Vec<usize>> = HashMap::new();
        for (i, b) in buildings.iter().enumerate() {
            let (outer_area, cx, cy) = ring_area_centroid(&b.footprint);
            let holes: f64 = b.holes.iter().map(|h| ring_area_centroid(h).0).sum();
            geo.push(Geo { cx, cy, area: (outer_area - holes).max(0.0) });
            grid.entry(Self::cell(cx, cy)).or_default().push(i);
        }

        let (x0, y0) = to_m((bbox.min_lon, bbox.min_lat));
        let (x1, y1) = to_m((bbox.max_lon, bbox.max_lat));
        Self { buildings, geo, grid, bbox_m: [x0, y0, x1, y1] }
    }

    fn cell(x: f64, y: f64) -> (i32, i32) {
        ((x / GRID_CELL_M).floor() as i32, (y / GRID_CELL_M).floor() as i32)
    }

    /// Indices of elements whose centroid lies within `r` of element `i`'s centroid.
    fn within(&self, i: usize, r: f64) -> impl Iterator<Item = (usize, f64)> + '_ {
        let (cx, cy) = (self.geo[i].cx, self.geo[i].cy);
        let (gx, gy) = Self::cell(cx, cy);
        let span = (r / GRID_CELL_M).ceil() as i32;
        (gx - span..=gx + span)
            .flat_map(move |x| (gy - span..=gy + span).map(move |y| (x, y)))
            .filter_map(move |key| self.grid.get(&key))
            .flatten()
            .filter_map(move |&j| {
                let d = ((self.geo[j].cx - cx).powi(2) + (self.geo[j].cy - cy).powi(2)).sqrt();
                (d <= r).then_some((j, d))
            })
    }

    /// Built-up ratio around element `i` (ground-level footprints, courtyards
    /// excluded), corrected for the part of the disk that falls outside the bbox.
    fn density(&self, i: usize) -> f32 {
        let r = DENSITY_RADIUS_M;
        let built: f64 = self
            .within(i, r)
            .filter(|&(j, _)| self.buildings[j].min_height < 0.5)
            .map(|(j, _)| self.geo[j].area)
            .sum();
        let (cx, cy) = (self.geo[i].cx, self.geo[i].cy);
        let [bx0, by0, bx1, by1] = self.bbox_m;
        let fx = ((cx + r).min(bx1) - (cx - r).max(bx0)).max(0.0) / (2.0 * r);
        let fy = ((cy + r).min(by1) - (cy - r).max(by0)).max(0.0) / (2.0 * r);
        let disk = std::f64::consts::PI * r * r * (fx * fy).max(0.25);
        (built / disk) as f32
    }
}

/// Union-find over generic ground-level outlines that share an OSM node
/// (terraced fabric: every isolato becomes one component).
fn blocks(buildings: &[Building], eligible: &[bool]) -> Vec<usize> {
    let mut parent: Vec<usize> = (0..buildings.len()).collect();
    fn find(p: &mut [usize], mut x: usize) -> usize {
        while p[x] != x {
            p[x] = p[p[x]];
            x = p[x];
        }
        x
    }
    let mut owner: HashMap<(i64, i64), usize> = HashMap::new();
    for (i, b) in buildings.iter().enumerate() {
        if !eligible[i] {
            continue;
        }
        for &(lon, lat) in &b.footprint {
            let key = ((lon * 1e7).round() as i64, (lat * 1e7).round() as i64);
            match owner.get(&key) {
                Some(&j) => {
                    let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
                    if ri != rj {
                        parent[ri.max(rj)] = ri.min(rj);
                    }
                }
                None => {
                    owner.insert(key, i);
                }
            }
        }
    }
    (0..buildings.len()).map(|i| find(&mut parent, i)).collect()
}

/// Density prior (+ footprint guards) for element `i`. Returns (height, big_box).
fn prior(ctx: &Ctx, i: usize, (lo, hi): (f32, f32)) -> (f32, bool) {
    let b = &ctx.buildings[i];
    let rho = ctx.density(i);
    let area = ctx.geo[i].area;
    if area > BIG_BOX_MIN_AREA_M2
        && rho < BIG_BOX_MAX_DENSITY
        && b.holes.is_empty()
        && matches!(b.kind.as_str(), "yes" | "commercial" | "retail")
    {
        return (BIG_BOX_HEIGHT_M, true);
    }
    let hi = if area < TINY_MAX_AREA_M2 { hi.min(TINY_MAX_LEVELS) } else { hi };
    let lo = lo.min(hi);
    let t = smoothstep((rho - DENSITY_LO) / (DENSITY_HI - DENSITY_LO));
    ((lo + (hi - lo) * t) * LEVEL_M, false)
}

/// Blend the prior with measured neighbours (excluding `skip`).
/// Returns (height, used_neighbours).
fn with_neighbors(ctx: &Ctx, i: usize, h_prior: f32, measured: &[bool], skip: Option<usize>) -> (f32, bool) {
    let mut samples: Vec<(f32, f32)> = ctx
        .within(i, NEIGHBOR_RADIUS_M)
        .filter(|&(j, _)| j != i && Some(j) != skip && measured[j])
        .map(|(j, d)| (ctx.buildings[j].height, (1.0 / (d + IDW_OFFSET_M)) as f32))
        .collect();
    if samples.len() < MIN_NEIGHBORS {
        return (h_prior, false);
    }
    let n = (samples.len() as f32).min(MAX_NEIGHBORS_WEIGHT);
    let h_nbr = weighted_median(&mut samples).unwrap_or(h_prior);
    ((n * h_nbr + PRIOR_WEIGHT * h_prior) / (n + PRIOR_WEIGHT), true)
}

fn clamp_to_range(h: f32, (lo, hi): (f32, f32)) -> f32 {
    h.clamp(lo * LEVEL_M, (hi + 1.0) * LEVEL_M)
}

/// Replace per-type default heights of generic outlines with contextual
/// estimates. Deterministic: every estimate is computed from the input state
/// (two-phase), never from heights updated earlier in the same pass.
pub fn infer_missing_heights(buildings: &mut [Building], bbox: &BoundingBox) -> InferenceReport {
    let n = buildings.len();
    let mut report = InferenceReport::default();
    if n == 0 {
        return report;
    }

    let range: Vec<Option<(f32, f32)>> = buildings
        .iter()
        .map(|b| if b.is_part { None } else { level_range(&b.kind) })
        .collect();
    // Measured generic outlines are the evidence; special types (a tagged
    // 40 m church) must not pull ordinary houses up.
    let measured: Vec<bool> = (0..n)
        .map(|i| range[i].is_some() && buildings[i].height_source.is_measured())
        .collect();
    let target: Vec<bool> = (0..n)
        .map(|i| range[i].is_some() && buildings[i].height_source == HeightSource::TypeDefault)
        .collect();

    let ctx = Ctx::new(buildings, bbox);
    let eligible: Vec<bool> = (0..n).map(|i| range[i].is_some() && buildings[i].min_height < 0.5).collect();
    let block_of = blocks(buildings, &eligible);

    // ── Phase 1: per-building estimate (density prior + neighbours) ─────────
    let mut estimate: Vec<Option<(f32, HeightSource, bool)>> = vec![None; n];
    for i in 0..n {
        if !target[i] {
            continue;
        }
        let r = range[i].unwrap();
        let (h_prior, big_box) = prior(&ctx, i, r);
        if big_box {
            estimate[i] = Some((h_prior, HeightSource::Density, true));
            continue;
        }
        let (h, used) = with_neighbors(&ctx, i, h_prior, &measured, None);
        let src = if used { HeightSource::Neighbors } else { HeightSource::Density };
        estimate[i] = Some((clamp_to_range(h, r), src, false));
    }

    // ── Phase 2: block harmonisation ────────────────────────────────────────
    let mut block_sizes: HashMap<usize, usize> = HashMap::new();
    for i in 0..n {
        if eligible[i] {
            *block_sizes.entry(block_of[i]).or_insert(0) += 1;
        }
    }
    report.blocks = block_sizes.values().filter(|&&c| c >= 2).count();

    let mut final_h: Vec<Option<(f32, HeightSource)>> = vec![None; n];
    for i in 0..n {
        let Some((h_self, src, big_box)) = estimate[i] else { continue };
        let blk = block_of[i];
        if big_box || !eligible[i] || block_sizes.get(&blk).copied().unwrap_or(0) < 2 {
            final_h[i] = Some((h_self, src));
            continue;
        }
        let mut meas: Vec<(f32, f32)> = Vec::new();
        let mut inf: Vec<(f32, f32)> = Vec::new();
        for (j, _) in ctx.within(i, BLOCK_RADIUS_M) {
            if block_of[j] != blk || !eligible[j] {
                continue;
            }
            let w = ctx.geo[j].area.max(1.0) as f32;
            if measured[j] {
                meas.push((buildings[j].height, w));
            } else if let Some((h, _, false)) = estimate[j] {
                inf.push((h, w));
            }
        }
        if meas.is_empty() && inf.len() < 2 {
            final_h[i] = Some((h_self, src));
            continue;
        }
        let h_inf = weighted_median(&mut inf).unwrap_or(h_self);
        let h = match weighted_median(&mut meas) {
            Some(h_meas) => {
                let m = (meas.len() as f32).min(BLOCK_MEASURED_CAP);
                (m * h_meas + BLOCK_INFERRED_WEIGHT * h_inf) / (m + BLOCK_INFERRED_WEIGHT)
            }
            None => h_inf,
        };
        final_h[i] = Some((clamp_to_range(h, range[i].unwrap()), HeightSource::Block));
    }

    // ── Leave-one-out self-check on measured generic outlines ───────────────
    let mut loo_new = 0.0f64;
    let mut loo_type = 0.0f64;
    let mut loo_n = 0usize;
    for i in 0..n {
        if !measured[i] {
            continue;
        }
        let r = range[i].unwrap();
        let truth = buildings[i].height;
        let (h_prior, big_box) = prior(&ctx, i, r);
        let mut h = if big_box {
            h_prior
        } else {
            clamp_to_range(with_neighbors(&ctx, i, h_prior, &measured, Some(i)).0, r)
        };
        if !big_box && eligible[i] {
            let mut meas: Vec<(f32, f32)> = ctx
                .within(i, BLOCK_RADIUS_M)
                .filter(|&(j, _)| j != i && block_of[j] == block_of[i] && measured[j] && eligible[j])
                .map(|(j, _)| (buildings[j].height, ctx.geo[j].area.max(1.0) as f32))
                .collect();
            if let Some(h_meas) = weighted_median(&mut meas) {
                let m = (meas.len() as f32).min(BLOCK_MEASURED_CAP);
                h = clamp_to_range((m * h_meas + BLOCK_INFERRED_WEIGHT * h) / (m + BLOCK_INFERRED_WEIGHT), r);
            }
        }
        loo_new += (h - truth).abs() as f64;
        loo_type += (crate::services::osm::default_height_for_type(&buildings[i].kind) - truth).abs() as f64;
        loo_n += 1;
    }
    if loo_n > 0 {
        report.loo = Some((loo_n, (loo_new / loo_n as f64) as f32, (loo_type / loo_n as f64) as f32));
    }

    // ── Apply ───────────────────────────────────────────────────────────────
    let mut counts: HashMap<HeightSource, usize> = HashMap::new();
    for (i, f) in final_h.into_iter().enumerate() {
        let Some((h, src)) = f else { continue };
        let b = &mut buildings[i];
        b.height = (h * jitter(b.id)).max(b.min_height + 0.5);
        b.height_source = src;
        *counts.entry(src).or_insert(0) += 1;
        report.inferred += 1;
    }
    let mut by_source: Vec<(HeightSource, usize)> = counts.into_iter().collect();
    by_source.sort();
    report.by_source = by_source.into_iter().map(|(s, c)| (s.name(), c)).collect();
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::RoofShape;

    const LAT0: f64 = 44.65;
    const LON0: f64 = 10.92;

    fn bbox() -> BoundingBox {
        BoundingBox { min_lat: LAT0 - 0.01, min_lon: LON0 - 0.01, max_lat: LAT0 + 0.01, max_lon: LON0 + 0.01 }
    }

    /// Axis-aligned square footprint of side `s` metres with its SW corner at
    /// (x, y) metres from (LON0, LAT0).
    fn square(x: f64, y: f64, s: f64) -> Vec<(f64, f64)> {
        let kx = 111_320.0 * LAT0.to_radians().cos();
        let p = |dx: f64, dy: f64| (LON0 + (x + dx) / kx, LAT0 + (y + dy) / 111_320.0);
        vec![p(0.0, 0.0), p(s, 0.0), p(s, s), p(0.0, s)]
    }

    fn bld(id: u64, fp: Vec<(f64, f64)>, kind: &str, height: f32, src: HeightSource) -> Building {
        Building {
            id,
            footprint: fp,
            holes: Vec::new(),
            height,
            min_height: 0.0,
            roof_shape: RoofShape::Flat,
            roof_height: 0.0,
            is_part: false,
            kind: kind.to_string(),
            height_source: src,
        }
    }

    /// A dense 14×14 grid of 14 m blocks with 2 m gaps (~77% built, indices
    /// 0..196) vs a sparse 4×4 grid of 10 m houses 60 m apart (~3% built,
    /// indices 196..212, index = 196 + 4·gx + gy).
    fn dense_and_sparse() -> Vec<Building> {
        let mut v = Vec::new();
        let mut id = 1;
        for gx in 0..14 {
            for gy in 0..14 {
                v.push(bld(id, square(gx as f64 * 16.0, gy as f64 * 16.0, 14.0), "yes", 8.0, HeightSource::TypeDefault));
                id += 1;
            }
        }
        for gx in 0..4 {
            for gy in 0..4 {
                v.push(bld(id, square(600.0 + gx as f64 * 60.0, gy as f64 * 60.0, 10.0), "yes", 8.0, HeightSource::TypeDefault));
                id += 1;
            }
        }
        v
    }

    #[test]
    fn density_prior_makes_dense_core_taller_than_sparse_suburb() {
        let mut v = dense_and_sparse();
        infer_missing_heights(&mut v, &bbox());
        let dense = v[7 * 14 + 7].height; // centre of the dense grid
        let sparse = v[201].height;
        assert!(dense > 14.5, "dense core should reach ~5 storeys, got {dense}");
        assert!(sparse < 8.5, "sparse suburb should stay ~2.5 storeys, got {sparse}");
        assert!(v.iter().all(|b| b.height_source != HeightSource::TypeDefault));
    }

    #[test]
    fn tagged_neighbours_pull_estimates() {
        let mut v = dense_and_sparse();
        // Tag the first column of sparse houses at 18 m.
        for i in [196, 197, 198, 199] {
            v[i].height = 18.0;
            v[i].height_source = HeightSource::Levels;
        }
        let before = {
            let mut w = dense_and_sparse();
            infer_missing_heights(&mut w, &bbox());
            w[201].height
        };
        let report = infer_missing_heights(&mut v, &bbox());
        assert_eq!(v[201].height_source, HeightSource::Neighbors);
        assert!(v[201].height > before + 2.0, "{} vs {}", v[201].height, before);
        let (loo_n, _, _) = report.loo.unwrap();
        assert_eq!(loo_n, 4);
    }

    #[test]
    fn block_members_sharing_nodes_are_harmonised() {
        // Three terraced buildings sharing walls, tagged with different
        // generic types (the old defaults split them 8 m / 15 m / 8 m).
        let mut v = vec![
            bld(1, square(0.0, 0.0, 30.0), "yes", 8.0, HeightSource::TypeDefault),
            bld(2, square(30.0, 0.0, 30.0), "residential", 15.0, HeightSource::TypeDefault),
            bld(3, square(60.0, 0.0, 30.0), "yes", 8.0, HeightSource::TypeDefault),
        ];
        // Make them share exact nodes along the common walls.
        v[1].footprint[0] = v[0].footprint[1];
        v[1].footprint[3] = v[0].footprint[2];
        v[2].footprint[0] = v[1].footprint[1];
        v[2].footprint[3] = v[1].footprint[2];
        let report = infer_missing_heights(&mut v, &bbox());
        assert_eq!(report.blocks, 1);
        assert!(v.iter().all(|b| b.height_source == HeightSource::Block));
        let (lo, hi) = (
            v.iter().map(|b| b.height).fold(f32::MAX, f32::min),
            v.iter().map(|b| b.height).fold(0.0f32, f32::max),
        );
        // Only the ±4% jitter separates them (vs the old 8 m / 15 m split).
        assert!(hi / lo < 1.1, "block heights {lo}..{hi}");
    }

    #[test]
    fn special_types_parts_and_tags_are_untouched() {
        let mut v = dense_and_sparse();
        v[0].kind = "church".into();
        v[0].height = 18.0;
        v[1].is_part = true;
        v[1].height = 11.0;
        v[2].height = 22.0;
        v[2].height_source = HeightSource::Tag;
        infer_missing_heights(&mut v, &bbox());
        assert_eq!(v[0].height, 18.0);
        assert_eq!(v[0].height_source, HeightSource::TypeDefault);
        assert_eq!(v[1].height, 11.0);
        assert_eq!(v[2].height, 22.0);
    }

    #[test]
    fn deterministic() {
        let mut a = dense_and_sparse();
        let mut b = dense_and_sparse();
        infer_missing_heights(&mut a, &bbox());
        infer_missing_heights(&mut b, &bbox());
        assert!(a.iter().zip(&b).all(|(x, y)| x.height == y.height));
    }
}
