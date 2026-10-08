//! Sea polygons from OSM `natural=coastline` ways.
//!
//! OSM does not map the sea as an area: the land/sea divide is the network of
//! `natural=coastline` ways, drawn with the LAND ON THE LEFT and the water on
//! the right (OSM wiki, Tag:natural=coastline). Renderers turn it into sea
//! polygons per tile (OSMCoastline / osmdata.openstreetmap.de; mkgmap's sea
//! generation; osmplotr's `osm_line2poly`) with the same recipe used here:
//!
//!   1. join coastline ways end-to-start into chains (never reversing them —
//!      the direction carries the land/sea side);
//!   2. clip every open chain to the bbox: each piece ENTERS and EXITS through
//!      the bbox boundary;
//!   3. a sea ring follows a piece (water on its right), then walks the bbox
//!      boundary CLOCKWISE from the exit to the next entry, follows that piece,
//!      and so on until it closes;
//!   4. closed rings fully inside the bbox are islands when counter-clockwise
//!      (land inside → holes in the sea) and enclosed seas when clockwise.
//!
//! Everything happens in lon/lat, where the request bbox is an axis-aligned
//! rectangle. The boundary walk inserts the DEM grid's own boundary lattice
//! points, so after projection the sea's bbox edges coincide with the
//! terrain hull vertices (no sliver triangles along the border).

use crate::models::{BoundingBox, WaterArea, WaterKind};
use std::collections::HashMap;

type Pt = (f64, f64);

/// Result of the coastline assembly.
#[derive(Debug)]
pub enum SeaOutcome {
    /// No coastline in the bbox: the area is treated as land (an open-ocean
    /// bbox without any coastline is a known limitation).
    NoCoastline,
    Built {
        polygons: Vec<WaterArea>,
        pieces: usize,
        islands: usize,
    },
    /// Coastline present but unusable (incomplete chain, inconsistent
    /// directions). No sea is generated rather than flooding the wrong side.
    Rejected(String),
}

/// Assemble sea polygons for `bbox` from coastline ways (node order kept).
/// `resolution` is the DEM grid resolution (grid = (res+1)² lon/lat lattice).
pub fn assemble_sea(coastlines: &[(u64, Vec<Pt>)], bbox: &BoundingBox, resolution: u32) -> SeaOutcome {
    let ways: Vec<&Vec<Pt>> = coastlines.iter().map(|(_, w)| w).filter(|w| w.len() >= 2).collect();
    if ways.is_empty() {
        return SeaOutcome::NoCoastline;
    }
    let rect = Rect::of(bbox);
    let lattice = boundary_lattice(&rect, resolution.max(1));

    let mut pieces: Vec<Piece> = Vec::new();
    let mut islands: Vec<Vec<Pt>> = Vec::new();
    let mut enclosed_seas: Vec<Vec<Pt>> = Vec::new();

    for chain in join_chains(&ways) {
        let closed = chain.len() >= 4 && chain[0] == *chain.last().unwrap();
        if closed {
            let mut ring = chain;
            ring.pop();
            if ring.iter().all(|&p| rect.contains(p)) {
                if signed_area(&ring) > 0.0 {
                    islands.push(ring); // CCW: land inside
                } else {
                    enclosed_seas.push(ring); // CW: water inside
                }
                continue;
            }
            // Crosses the boundary: reopen it at an outside vertex and clip.
            let k = ring.iter().position(|&p| !rect.contains(p)).unwrap();
            let mut open: Vec<Pt> = ring[k..].iter().chain(ring[..k].iter()).copied().collect();
            open.push(open[0]);
            match clip_chain(&open, &rect) {
                Ok(p) => pieces.extend(p),
                Err(e) => return SeaOutcome::Rejected(e),
            }
        } else {
            for end in [chain[0], *chain.last().unwrap()] {
                if rect.strictly_contains(end) {
                    return SeaOutcome::Rejected(format!(
                        "coastline chain ends inside the bbox at ({:.6}, {:.6}) — incomplete data",
                        end.1, end.0
                    ));
                }
            }
            match clip_chain(&chain, &rect) {
                Ok(p) => pieces.extend(p),
                Err(e) => return SeaOutcome::Rejected(e),
            }
        }
    }

    if pieces.is_empty() && islands.is_empty() && enclosed_seas.is_empty() {
        return SeaOutcome::NoCoastline;
    }

    let mut sea_rings: Vec<Vec<Pt>> = if pieces.is_empty() {
        if islands.is_empty() {
            Vec::new()
        } else {
            // Only islands: the whole bbox is sea around them.
            vec![lattice.iter().map(|&(_, p)| p).collect()]
        }
    } else {
        match walk_sea_rings(&pieces, &lattice) {
            Ok(r) => r,
            Err(e) => return SeaOutcome::Rejected(e),
        }
    };
    sea_rings.extend(enclosed_seas);

    // Islands become holes of the sea ring that contains them; an "island"
    // on the land side is a tagging error and is ignored.
    let n_islands = islands.len();
    let mut polygons: Vec<WaterArea> = sea_rings
        .into_iter()
        .map(|outer| WaterArea { id: 0, kind: WaterKind::Sea, outer, holes: Vec::new() })
        .collect();
    for island in islands {
        if let Some(poly) = polygons.iter_mut().find(|p| point_in_ring(island[0], &p.outer)) {
            poly.holes.push(island);
        }
    }

    SeaOutcome::Built { polygons, pieces: pieces.len(), islands: n_islands }
}

// ── Chains ────────────────────────────────────────────────────────────────────

fn key(p: Pt) -> (u64, u64) {
    (p.0.to_bits(), p.1.to_bits())
}

/// Join ways whose end node is the next way's start node. Shared OSM nodes
/// produce bit-identical coordinates, so exact matching is safe. Chains start
/// at ways nobody flows into; whatever is left afterwards forms cycles.
fn join_chains(ways: &[&Vec<Pt>]) -> Vec<Vec<Pt>> {
    let mut by_start: HashMap<(u64, u64), Vec<usize>> = HashMap::new();
    let mut has_incoming: HashMap<(u64, u64), bool> = HashMap::new();
    for (i, w) in ways.iter().enumerate() {
        by_start.entry(key(w[0])).or_default().push(i);
        has_incoming.insert(key(*w.last().unwrap()), true);
    }

    let mut used = vec![false; ways.len()];
    let mut chains = Vec::new();
    let follow = |start: usize, used: &mut Vec<bool>| -> Vec<Pt> {
        used[start] = true;
        let mut chain: Vec<Pt> = ways[start].to_vec();
        loop {
            let end = *chain.last().unwrap();
            if chain.len() >= 4 && end == chain[0] {
                break; // closed
            }
            let next = by_start
                .get(&key(end))
                .and_then(|c| c.iter().copied().find(|&j| !used[j]));
            match next {
                Some(j) => {
                    used[j] = true;
                    chain.extend(ways[j].iter().skip(1).copied());
                }
                None => break,
            }
        }
        chain
    };

    for i in 0..ways.len() {
        if !used[i] && !has_incoming.contains_key(&key(ways[i][0])) {
            chains.push(follow(i, &mut used));
        }
    }
    for i in 0..ways.len() {
        if !used[i] {
            chains.push(follow(i, &mut used));
        }
    }
    chains
}

// ── Clipping ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
struct Rect {
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
}

impl Rect {
    fn of(b: &BoundingBox) -> Self {
        Self { x0: b.min_lon, y0: b.min_lat, x1: b.max_lon, y1: b.max_lat }
    }

    fn contains(&self, p: Pt) -> bool {
        p.0 >= self.x0 && p.0 <= self.x1 && p.1 >= self.y0 && p.1 <= self.y1
    }

    fn strictly_contains(&self, p: Pt) -> bool {
        p.0 > self.x0 && p.0 < self.x1 && p.1 > self.y0 && p.1 < self.y1
    }

    /// Clockwise perimeter parameter in [0, 4): NW→NE (top), NE→SE (right),
    /// SE→SW (bottom), SW→NW (left) — clockwise with north up.
    fn perimeter_t(&self, p: Pt) -> f64 {
        let (w, h) = (self.x1 - self.x0, self.y1 - self.y0);
        let d = [
            (p.1 - self.y1).abs() / h, // top
            (p.0 - self.x1).abs() / w, // right
            (p.1 - self.y0).abs() / h, // bottom
            (p.0 - self.x0).abs() / w, // left
        ];
        let edge = (0..4).min_by(|&a, &b| d[a].partial_cmp(&d[b]).unwrap()).unwrap();
        let t = match edge {
            0 => (p.0 - self.x0) / w,
            1 => 1.0 + (self.y1 - p.1) / h,
            2 => 2.0 + (self.x1 - p.0) / w,
            _ => 3.0 + (p.1 - self.y0) / h,
        };
        t.clamp(0.0, 4.0) % 4.0
    }
}

/// A coastline piece inside the bbox: enters at `t_in`, leaves at `t_out`.
#[derive(Debug, Clone)]
struct Piece {
    pts: Vec<Pt>,
    t_in: f64,
    t_out: f64,
}

/// Liang–Barsky: parametric sub-range [t0, t1] of segment a→b inside `r`.
fn clip_segment(a: Pt, b: Pt, r: &Rect) -> Option<(f64, f64)> {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let (mut t0, mut t1) = (0.0f64, 1.0f64);
    for (p, q) in [(-dx, a.0 - r.x0), (dx, r.x1 - a.0), (-dy, a.1 - r.y0), (dy, r.y1 - a.1)] {
        if p == 0.0 {
            if q < 0.0 {
                return None;
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
    (t0 <= t1).then_some((t0, t1))
}

/// Cut an open chain into the pieces that lie inside `r`. Every piece starts
/// and ends on the bbox boundary (the chain's own ends were checked to be
/// outside or on the boundary).
fn clip_chain(chain: &[Pt], r: &Rect) -> Result<Vec<Piece>, String> {
    let lerp = |a: Pt, b: Pt, t: f64| (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t);
    let mut pieces = Vec::new();
    let mut current: Option<Vec<Pt>> = None;
    let close = |pts: Vec<Pt>, pieces: &mut Vec<Piece>| {
        // A tangential touch yields a zero-length piece: ignore it.
        if pts.len() >= 2 && pts.first() != pts.last() {
            pieces.push(Piece { t_in: r.perimeter_t(pts[0]), t_out: r.perimeter_t(*pts.last().unwrap()), pts });
        }
    };

    for w in chain.windows(2) {
        let (a, b) = (w[0], w[1]);
        match clip_segment(a, b, r) {
            Some((t0, t1)) => {
                let pts = current.get_or_insert_with(|| vec![lerp(a, b, t0)]);
                let pb = if t1 >= 1.0 { b } else { lerp(a, b, t1) };
                if pts.last() != Some(&pb) {
                    pts.push(pb);
                }
                if t1 < 1.0 {
                    close(current.take().unwrap(), &mut pieces);
                }
            }
            None => {
                if let Some(pts) = current.take() {
                    close(pts, &mut pieces);
                }
            }
        }
    }
    if let Some(pts) = current.take() {
        if r.strictly_contains(*pts.last().unwrap()) {
            return Err("coastline piece ends inside the bbox".into());
        }
        close(pts, &mut pieces);
    }
    Ok(pieces)
}

// ── Boundary walk ─────────────────────────────────────────────────────────────

/// Boundary points of the (res+1)² lon/lat DEM lattice, with their clockwise
/// perimeter parameter, sorted (corners included).
fn boundary_lattice(r: &Rect, res: u32) -> Vec<(f64, Pt)> {
    let n = res as usize;
    let dx = (r.x1 - r.x0) / res as f64;
    let dy = (r.y1 - r.y0) / res as f64;
    let mut pts: Vec<(f64, Pt)> = Vec::with_capacity(4 * n);
    for j in 0..n {
        // top, west → east (t in [0,1))
        pts.push((j as f64 / n as f64, (r.x0 + j as f64 * dx, r.y1)));
        // right, north → south
        pts.push((1.0 + j as f64 / n as f64, (r.x1, r.y1 - j as f64 * dy)));
        // bottom, east → west
        pts.push((2.0 + j as f64 / n as f64, (r.x1 - j as f64 * dx, r.y0)));
        // left, south → north
        pts.push((3.0 + j as f64 / n as f64, (r.x0, r.y0 + j as f64 * dy)));
    }
    pts.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    pts
}

/// Clockwise distance along the perimeter from `a` to `b`, in [0, 4).
fn cw(a: f64, b: f64) -> f64 {
    (b - a).rem_euclid(4.0)
}

/// Close pieces into sea rings: exit → clockwise along the boundary → next
/// entry → its piece → … Water lies on the right of every piece, and a
/// clockwise boundary walk keeps the bbox interior on the right too, so each
/// resulting ring is a clockwise sea polygon.
fn walk_sea_rings(pieces: &[Piece], lattice: &[(f64, Pt)]) -> Result<Vec<Vec<Pt>>, String> {
    const EPS: f64 = 1e-12;
    let mut used = vec![false; pieces.len()];
    let mut rings = Vec::new();

    for start in 0..pieces.len() {
        if used[start] {
            continue;
        }
        let mut ring: Vec<Pt> = Vec::new();
        let mut cur = start;
        loop {
            used[cur] = true;
            ring.extend(pieces[cur].pts.iter().copied());
            let t_out = pieces[cur].t_out;

            let next = (0..pieces.len())
                .min_by(|&a, &b| cw(t_out, pieces[a].t_in).partial_cmp(&cw(t_out, pieces[b].t_in)).unwrap())
                .unwrap();
            let gap = cw(t_out, pieces[next].t_in);
            // Entries and exits must alternate around the boundary; another
            // exit before the next entry means inconsistent directions.
            if pieces
                .iter()
                .enumerate()
                .any(|(k, p)| k != cur && cw(t_out, p.t_out) > EPS && cw(t_out, p.t_out) < gap - EPS)
            {
                return Err("coastline directions are inconsistent along the bbox boundary".into());
            }
            // Boundary lattice points strictly between exit and next entry,
            // ordered by clockwise distance (the walk may wrap past NW).
            let mut walked: Vec<(f64, Pt)> = lattice
                .iter()
                .map(|&(t, p)| (cw(t_out, t), p))
                .filter(|&(d, _)| d > EPS && d < gap - EPS)
                .collect();
            walked.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
            ring.extend(walked.into_iter().map(|(_, p)| p));

            if next == start {
                break;
            }
            if used[next] {
                return Err("coastline pieces do not close into rings".into());
            }
            cur = next;
        }
        rings.push(ring);
    }
    Ok(rings)
}

// ── Small geometry helpers ────────────────────────────────────────────────────

/// Shoelace signed area (lon/lat units); positive ⇒ counter-clockwise.
fn signed_area(ring: &[Pt]) -> f64 {
    let n = ring.len();
    (0..n)
        .map(|i| {
            let (a, b) = (ring[i], ring[(i + 1) % n]);
            a.0 * b.1 - b.0 * a.1
        })
        .sum::<f64>()
        * 0.5
}

fn point_in_ring(p: Pt, ring: &[Pt]) -> bool {
    let mut inside = false;
    let n = ring.len();
    for i in 0..n {
        let (x0, y0) = ring[i];
        let (x1, y1) = ring[(i + 1) % n];
        if (y0 > p.1) != (y1 > p.1) {
            let x = x0 + (p.1 - y0) / (y1 - y0) * (x1 - x0);
            if p.0 < x {
                inside = !inside;
            }
        }
    }
    inside
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bbox() -> BoundingBox {
        BoundingBox { min_lat: 0.0, min_lon: 0.0, max_lat: 10.0, max_lon: 10.0 }
    }

    fn sea_of(coast: Vec<Vec<Pt>>) -> SeaOutcome {
        let ways: Vec<(u64, Vec<Pt>)> = coast.into_iter().enumerate().map(|(i, w)| (i as u64, w)).collect();
        assemble_sea(&ways, &bbox(), 10)
    }

    fn polygons(o: SeaOutcome) -> Vec<WaterArea> {
        match o {
            SeaOutcome::Built { polygons, .. } => polygons,
            other => panic!("expected sea, got {other:?}"),
        }
    }

    fn is_sea(polys: &[WaterArea], p: Pt) -> bool {
        polys.iter().any(|a| point_in_ring(p, &a.outer) && !a.holes.iter().any(|h| point_in_ring(p, h)))
    }

    #[test]
    fn west_to_east_coast_puts_sea_south() {
        // Land on the left of an eastbound way = north.
        let polys = polygons(sea_of(vec![vec![(-1.0, 5.0), (11.0, 5.0)]]));
        assert_eq!(polys.len(), 1);
        assert!(is_sea(&polys, (5.0, 2.0)));
        assert!(!is_sea(&polys, (5.0, 8.0)));
        assert!((signed_area(&polys[0].outer).abs() - 50.0).abs() < 1e-9);
        assert!(signed_area(&polys[0].outer) < 0.0, "sea rings are clockwise");
    }

    #[test]
    fn reversed_coast_puts_sea_north() {
        let polys = polygons(sea_of(vec![vec![(11.0, 5.0), (-1.0, 5.0)]]));
        assert!(is_sea(&polys, (5.0, 8.0)));
        assert!(!is_sea(&polys, (5.0, 2.0)));
        assert!((signed_area(&polys[0].outer).abs() - 50.0).abs() < 1e-9);
    }

    #[test]
    fn split_ways_are_joined() {
        let polys = polygons(sea_of(vec![
            vec![(5.0, 5.0), (11.0, 5.0)],
            vec![(-1.0, 5.0), (5.0, 5.0)],
        ]));
        assert_eq!(polys.len(), 1);
        assert!(is_sea(&polys, (8.0, 1.0)));
        assert!(!is_sea(&polys, (2.0, 9.0)));
    }

    #[test]
    fn bay_entering_and_leaving_through_same_edge() {
        // North, east, then south: land on the left (outside the U), water inside.
        let polys = polygons(sea_of(vec![vec![(3.0, -1.0), (3.0, 4.0), (7.0, 4.0), (7.0, -1.0)]]));
        assert!(is_sea(&polys, (5.0, 2.0)));
        assert!(!is_sea(&polys, (1.0, 2.0)));
        assert!(!is_sea(&polys, (5.0, 6.0)));
    }

    #[test]
    fn island_only_floods_bbox_with_hole() {
        // CCW square: land inside.
        let island = vec![(4.0, 4.0), (6.0, 4.0), (6.0, 6.0), (4.0, 6.0), (4.0, 4.0)];
        let polys = polygons(sea_of(vec![island]));
        assert_eq!(polys.len(), 1);
        assert_eq!(polys[0].holes.len(), 1);
        assert!(is_sea(&polys, (1.0, 1.0)));
        assert!(!is_sea(&polys, (5.0, 5.0)));
    }

    #[test]
    fn island_in_sea_becomes_hole() {
        let island = vec![(4.0, 1.0), (6.0, 1.0), (6.0, 3.0), (4.0, 3.0), (4.0, 1.0)];
        let polys = polygons(sea_of(vec![vec![(-1.0, 5.0), (11.0, 5.0)], island]));
        assert!(is_sea(&polys, (2.0, 2.0)));
        assert!(!is_sea(&polys, (5.0, 2.0)));
    }

    #[test]
    fn no_coastline_means_no_sea() {
        assert!(matches!(sea_of(vec![]), SeaOutcome::NoCoastline));
        // Coastline entirely outside the bbox.
        assert!(matches!(sea_of(vec![vec![(-5.0, 20.0), (15.0, 20.0)]]), SeaOutcome::NoCoastline));
    }

    #[test]
    fn broken_chain_is_rejected() {
        assert!(matches!(sea_of(vec![vec![(-1.0, 5.0), (5.0, 5.0)]]), SeaOutcome::Rejected(_)));
    }

    #[test]
    fn sea_boundary_uses_grid_lattice_points() {
        let polys = polygons(sea_of(vec![vec![(-1.0, 5.0), (11.0, 5.0)]]));
        // Lattice step is 1.0: every integer point of the southern border is in the ring.
        for x in 0..=10 {
            assert!(polys[0].outer.contains(&(x as f64, 0.0)), "missing ({x}, 0)");
        }
    }
}
