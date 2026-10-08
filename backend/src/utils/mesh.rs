use crate::models::{Building, ProjectedPoint, RoofShape, TerrainMesh};
use crate::utils::hydro::HydroModel;
use crate::utils::projection::CoordinateProjector;
use anyhow::Result;
use spade::{ConstrainedDelaunayTriangulation, HasPosition, Point2, Triangulation};

/// Vertex of the constrained terrain triangulation. `grid` is the index of the
/// DEM grid point it came from; shoreline / split vertices have none.
#[derive(Clone, Copy, Debug)]
struct CdtVertex {
    pos: Point2<f64>,
    grid: Option<u32>,
}

impl HasPosition for CdtVertex {
    type Scalar = f64;
    fn position(&self) -> Point2<f64> {
        self.pos
    }
}

/// Signed area (shoelace) of a 2D polygon ring; positive ⇒ counter-clockwise.
fn signed_area(pts: &[(f32, f32)]) -> f32 {
    let n = pts.len();
    let mut a = 0.0f32;
    for i in 0..n {
        let (x0, y0) = pts[i];
        let (x1, y1) = pts[(i + 1) % n];
        a += x0 * y1 - x1 * y0;
    }
    a * 0.5
}

/// Where a building solid ended up, in mesh Z units (terrain-relative, BEFORE
/// print normalization). `vertices_added == 0` means the element emitted no
/// geometry — `skip_reason` says why. Used by the geometry debug report.
#[derive(Debug, Clone)]
pub struct BuildingPlacement {
    pub id: u64,
    pub base_elevation: f32,
    pub bottom_z: f32,
    pub eave_z: f32,
    pub top_z: f32,
    pub roof_z: f32,
    pub vertices_added: usize,
    pub skip_reason: Option<String>,
}

impl BuildingPlacement {
    fn skipped(id: u64, reason: &str) -> Self {
        Self {
            id,
            base_elevation: 0.0,
            bottom_z: 0.0,
            eave_z: 0.0,
            top_z: 0.0,
            roof_z: 0.0,
            vertices_added: 0,
            skip_reason: Some(reason.to_string()),
        }
    }
}

/// Area centroid of a simple polygon (vertex mean when the ring is degenerate).
fn ring_centroid(pts: &[(f32, f32)]) -> (f32, f32) {
    let n = pts.len();
    let mut a = 0.0f32;
    let (mut cx, mut cy) = (0.0f32, 0.0f32);
    for i in 0..n {
        let (x0, y0) = pts[i];
        let (x1, y1) = pts[(i + 1) % n];
        let cross = x0 * y1 - x1 * y0;
        a += cross;
        cx += (x0 + x1) * cross;
        cy += (y0 + y1) * cross;
    }
    if a.abs() < 1e-6 {
        let inv = 1.0 / n.max(1) as f32;
        return (
            pts.iter().map(|p| p.0).sum::<f32>() * inv,
            pts.iter().map(|p| p.1).sum::<f32>() * inv,
        );
    }
    (cx / (3.0 * a), cy / (3.0 * a))
}

/// True when the triangle set forms a closed 2-manifold surface
/// (every undirected edge is used by exactly two triangles).
fn solid_is_closed(triangles: &[[usize; 3]]) -> bool {
    let mut edges: std::collections::HashMap<(usize, usize), i32> =
        std::collections::HashMap::with_capacity(triangles.len() * 3 / 2);
    for t in triangles {
        for k in 0..3 {
            let (a, b) = (t[k], t[(k + 1) % 3]);
            let key = if a < b { (a, b) } else { (b, a) };
            *edges.entry(key).or_insert(0) += 1;
        }
    }
    edges.values().all(|&c| c == 2)
}

/// Axis-aligned bbox of a ring: [min_x, min_y, max_x, max_y].
fn ring_bbox(pts: &[(f32, f32)]) -> [f32; 4] {
    let mut b = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
    for &(x, y) in pts {
        b[0] = b[0].min(x);
        b[1] = b[1].min(y);
        b[2] = b[2].max(x);
        b[3] = b[3].max(y);
    }
    b
}

/// Ray-casting point-in-polygon test on projected coordinates.
fn point_in_ring_f32(p: (f32, f32), ring: &[(f32, f32)]) -> bool {
    let (px, py) = p;
    let mut inside = false;
    let n = ring.len();
    for i in 0..n {
        let (x0, y0) = ring[i];
        let (x1, y1) = ring[(i + 1) % n];
        if (y0 > py) != (y1 > py) {
            let x_cross = x0 + (py - y0) / (y1 - y0) * (x1 - x0);
            if px < x_cross {
                inside = !inside;
            }
        }
    }
    inside
}

/// Per-vertex eave heights for a skillion (single-slope) roof: lowest along the
/// footprint's longest edge, rising linearly to the far side.
fn skillion_heights(ring: &[(f32, f32)], eave_z: f32, roof_z: f32) -> Vec<f32> {
    let n = ring.len();
    let (mut best, mut best_len2) = (0usize, -1.0f32);
    for i in 0..n {
        let j = (i + 1) % n;
        let (dx, dy) = (ring[j].0 - ring[i].0, ring[j].1 - ring[i].1);
        let len2 = dx * dx + dy * dy;
        if len2 > best_len2 {
            best_len2 = len2;
            best = i;
        }
    }
    let (ax, ay) = ring[best];
    let (bx, by) = ring[(best + 1) % n];
    let len = ((bx - ax).powi(2) + (by - ay).powi(2)).sqrt().max(1e-3);
    let (nx, ny) = (-(by - ay) / len, (bx - ax) / len);
    let dists: Vec<f32> = ring
        .iter()
        .map(|&(x, y)| ((x - ax) * nx + (y - ay) * ny).abs())
        .collect();
    let d_max = dists.iter().cloned().fold(0.0f32, f32::max).max(1e-3);
    dists.iter().map(|d| eave_z + roof_z * d / d_max).collect()
}

/// Pyramid roof: a fan from every eave-ring edge to a single apex above the
/// centroid. Combinatorially closed for any simple ring.
fn add_pyramid_roof(mesh: &mut TerrainMesh, top0: usize, ring: &[(f32, f32)], apex_z: f32) {
    let (cx, cy) = ring_centroid(ring);
    let apex = mesh.vertices.len();
    mesh.vertices.push([cx, cy, apex_z]);
    let n = ring.len();
    for i in 0..n {
        mesh.triangles.push([top0 + i, top0 + (i + 1) % n, apex]);
    }
}

/// Dome roof: stacked copies of the eave ring, shrunk toward the centroid with a
/// cos profile and raised with a sin profile (quarter circle), closed by an apex
/// fan. On a circular footprint this is a low-poly hemisphere scaled to roof_z.
fn add_dome_roof(mesh: &mut TerrainMesh, top0: usize, ring: &[(f32, f32)], eave_z: f32, roof_z: f32) {
    const SEGS: usize = 6;
    let (cx, cy) = ring_centroid(ring);
    let n = ring.len();
    let mut prev: Vec<usize> = (top0..top0 + n).collect();
    for k in 1..SEGS {
        let t = (k as f32 / SEGS as f32) * std::f32::consts::FRAC_PI_2;
        let s = t.cos();
        let z = eave_z + roof_z * t.sin();
        let start = mesh.vertices.len();
        for &(x, y) in ring {
            mesh.vertices.push([cx + (x - cx) * s, cy + (y - cy) * s, z]);
        }
        for i in 0..n {
            let j = (i + 1) % n;
            mesh.triangles.push([prev[i], prev[j], start + i]);
            mesh.triangles.push([start + i, prev[j], start + j]);
        }
        prev = (start..start + n).collect();
    }
    let apex = mesh.vertices.len();
    mesh.vertices.push([cx, cy, eave_z + roof_z]);
    for i in 0..n {
        mesh.triangles.push([prev[i], prev[(i + 1) % n], apex]);
    }
}

/// Gabled / hipped roof over a quad footprint. The ridge runs between the
/// midpoints of the two SHORT edges; `inset_frac` pulls the ridge endpoints
/// toward each other (0 = gabled with vertical gable ends, ~0.3 = hipped).
fn add_ridge_roof(
    mesh: &mut TerrainMesh,
    top0: usize,
    ring: &[(f32, f32)],
    ridge_z: f32,
    inset_frac: f32,
) {
    debug_assert_eq!(ring.len(), 4);
    let edge_len = |i: usize, j: usize| {
        ((ring[j].0 - ring[i].0).powi(2) + (ring[j].1 - ring[i].1).powi(2)).sqrt()
    };
    // Rotate labels so the slope faces sit over the two LONG edges.
    let idx: [usize; 4] = if edge_len(0, 1) + edge_len(2, 3) >= edge_len(1, 2) + edge_len(3, 0) {
        [0, 1, 2, 3]
    } else {
        [1, 2, 3, 0]
    };
    let v = |k: usize| ring[idx[k]];
    let mid = |a: (f32, f32), b: (f32, f32)| ((a.0 + b.0) * 0.5, (a.1 + b.1) * 0.5);
    let ra = mid(v(1), v(2));
    let rb = mid(v(3), v(0));
    let lerp = |a: (f32, f32), b: (f32, f32), t: f32| (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t);
    let (ra0, rb0) = (ra, rb);
    let ra = lerp(ra0, rb0, inset_frac * 0.5);
    let rb = lerp(rb0, ra0, inset_frac * 0.5);

    let ia = mesh.vertices.len();
    mesh.vertices.push([ra.0, ra.1, ridge_z]);
    let ib = mesh.vertices.len();
    mesh.vertices.push([rb.0, rb.1, ridge_z]);

    let t = |k: usize| top0 + idx[k];
    // Slope faces over the long edges…
    mesh.triangles.push([t(0), t(1), ia]);
    mesh.triangles.push([t(0), ia, ib]);
    mesh.triangles.push([t(2), t(3), ib]);
    mesh.triangles.push([t(2), ib, ia]);
    // …and gable (or hip) faces over the short edges.
    mesh.triangles.push([t(1), t(2), ia]);
    mesh.triangles.push([t(3), t(0), ib]);
}

/// Fallback pitched roof for non-quad footprints: a frustum from the eave ring
/// to an inset, raised copy of it, capped flat. `cap` is the ear-clip
/// triangulation of the eave ring (valid for the inset ring too, since scaling
/// toward the centroid is affine).
fn add_frustum_roof(
    mesh: &mut TerrainMesh,
    top0: usize,
    ring: &[(f32, f32)],
    top_z: f32,
    cap: &[usize],
) {
    const INSET_SCALE: f32 = 0.55;
    let (cx, cy) = ring_centroid(ring);
    let n = ring.len();
    let start = mesh.vertices.len();
    for &(x, y) in ring {
        mesh.vertices.push([cx + (x - cx) * INSET_SCALE, cy + (y - cy) * INSET_SCALE, top_z]);
    }
    for i in 0..n {
        let j = (i + 1) % n;
        mesh.triangles.push([top0 + i, top0 + j, start + i]);
        mesh.triangles.push([start + i, top0 + j, start + j]);
    }
    for t in cap.chunks_exact(3) {
        mesh.triangles.push([start + t[0], start + t[1], start + t[2]]);
    }
}

/// Coarse uniform-grid spatial index over terrain vertices for fast nearest-Z
/// lookups. Used to anchor building bases to the ground beneath their footprint
/// (O(1) amortised per query vs an O(n) scan of every terrain vertex), and by
/// `utils::hydro` to sample the DEM and look up river-profile stations.
pub(crate) struct TerrainIndex {
    verts: Vec<[f32; 3]>,
    inv_cell: f32,
    min_x: f32,
    min_y: f32,
    buckets: std::collections::HashMap<(i32, i32), Vec<u32>>,
}

impl TerrainIndex {
    pub(crate) fn build(verts: Vec<[f32; 3]>) -> Self {
        let (mut min_x, mut min_y) = (f32::MAX, f32::MAX);
        let (mut max_x, mut max_y) = (f32::MIN, f32::MIN);
        for v in &verts {
            min_x = min_x.min(v[0]);
            max_x = max_x.max(v[0]);
            min_y = min_y.min(v[1]);
            max_y = max_y.max(v[1]);
        }
        let span = (max_x - min_x).max(max_y - min_y).max(1.0);
        // ~sqrt(n) cells per axis → roughly one point per cell on a regular grid.
        let cells = (verts.len().max(1) as f32).sqrt().max(1.0);
        let cell = (span / cells).max(1e-3);
        let inv_cell = 1.0 / cell;
        let mut buckets: std::collections::HashMap<(i32, i32), Vec<u32>> =
            std::collections::HashMap::new();
        for (i, v) in verts.iter().enumerate() {
            let key = (((v[0] - min_x) * inv_cell) as i32, ((v[1] - min_y) * inv_cell) as i32);
            buckets.entry(key).or_default().push(i as u32);
        }
        Self { verts, inv_cell, min_x, min_y, buckets }
    }

    /// Nearest terrain Z to (x, y), searching outward in grid rings.
    pub(crate) fn nearest_z(&self, x: f32, y: f32) -> Option<f32> {
        self.nearest_index(x, y).map(|i| self.verts[i][2])
    }

    /// Index (into the vertices the index was built from) of the vertex
    /// nearest to (x, y), searching outward in grid rings.
    pub(crate) fn nearest_index(&self, x: f32, y: f32) -> Option<usize> {
        if self.verts.is_empty() {
            return None;
        }
        let cx = ((x - self.min_x) * self.inv_cell) as i32;
        let cy = ((y - self.min_y) * self.inv_cell) as i32;
        let mut best_d = f32::MAX;
        let mut best = None;
        for r in 0..8 {
            for gx in (cx - r)..=(cx + r) {
                for gy in (cy - r)..=(cy + r) {
                    // Only visit the ring at radius r; interior was searched already.
                    if r > 0 && gx > cx - r && gx < cx + r && gy > cy - r && gy < cy + r {
                        continue;
                    }
                    if let Some(ids) = self.buckets.get(&(gx, gy)) {
                        for &id in ids {
                            let v = self.verts[id as usize];
                            let d = (v[0] - x).powi(2) + (v[1] - y).powi(2);
                            if d < best_d {
                                best_d = d;
                                best = Some(id as usize);
                            }
                        }
                    }
                }
            }
            // One extra ring past the first hit guarantees the true nearest.
            if best.is_some() && r >= 1 {
                break;
            }
        }
        best
    }
}

/// Generates a 3D mesh from elevation data and buildings
pub struct MeshGenerator {
    projector: CoordinateProjector,
    /// Applied only to terrain elevation. Computed adaptively in the handler so that
    /// terrain relief always reaches a visible fraction of the print width.
    terrain_scale: f32,
    /// Applied to building heights: a bounded exaggeration of the true XY
    /// scale, computed in the handler from the scene's typical building.
    building_scale: f32,
    /// Smallest wall height (mesh Z units) of a ground-level building outline,
    /// so tiny structures don't vanish into the terrain on large-area prints.
    min_building_height: f32,
    /// Depth (mesh Z units) of water surfaces below their shore.
    water_recess: f32,
    base_height: f32,
}

impl MeshGenerator {
    pub fn new(
        projector: CoordinateProjector,
        terrain_scale: f32,
        building_scale: f32,
        min_building_height: f32,
        water_recess: f32,
        base_height: f32,
    ) -> Self {
        Self {
            projector,
            terrain_scale,
            building_scale,
            min_building_height,
            water_recess,
            base_height,
        }
    }

    /// Generate the terrain surface. Without water this is the plain Delaunay
    /// triangulation of the DEM grid; with water the shorelines become
    /// constraint edges (see `generate_terrain_with_water`).
    /// Returns the mesh and the number of trailing water-surface triangles.
    pub fn generate_terrain_mesh(
        &self,
        points: &[ProjectedPoint],
        hydro: Option<&HydroModel>,
    ) -> Result<(TerrainMesh, usize)> {
        match hydro {
            Some(h) => self.generate_terrain_with_water(points, h),
            None => Ok((self.generate_plain_terrain(points)?, 0)),
        }
    }

    /// Terrain with vector-accurate water (constrained Delaunay, spade).
    ///
    /// The DEM grid points are triangulated together with the shorelines of
    /// the water region W as CONSTRAINT edges, so no triangle straddles a
    /// shore and every triangle is either land or water (classified by its
    /// centroid). Shore vertices are duplicated: a land copy at the water
    /// level (the shore IS the waterline, like a hydro-flattening breakline)
    /// and a water copy `water_recess` lower; a vertical wall strip joins the
    /// two along every land/water edge, so the surface stays a single closed
    /// 2-manifold with a printable step at the waterline. Output order:
    /// land triangles, shore walls, then water triangles (counted last).
    fn generate_terrain_with_water(&self, points: &[ProjectedPoint], hydro: &HydroModel) -> Result<(TerrainMesh, usize)> {
        tracing::info!("   ┌─ Mesh Generator: Terrain Surface with water");
        tracing::info!("   │  Algorithm: Constrained Delaunay (shorelines as constraint edges)");
        anyhow::ensure!(!points.is_empty(), "No elevation points provided");

        let z_min = points.iter().map(|p| p.z).fold(f32::INFINITY, f32::min);
        let ts = self.terrain_scale;

        // ── Triangulate grid + shoreline constraints ─────────────────────────
        let grid: Vec<CdtVertex> = points
            .iter()
            .enumerate()
            .map(|(i, p)| CdtVertex { pos: Point2::new(p.x, p.y), grid: Some(i as u32) })
            .collect();
        let mut cdt: ConstrainedDelaunayTriangulation<CdtVertex> = ConstrainedDelaunayTriangulation::bulk_load(grid)
            .map_err(|e| anyhow::anyhow!("triangulation failed: {e:?}"))?;
        let mut constraints = 0usize;
        for poly in &hydro.water.0 {
            for ring in std::iter::once(poly.exterior()).chain(poly.interiors()) {
                let mut handles = Vec::with_capacity(ring.0.len());
                for c in &ring.0 {
                    let p = Point2::new(c.x, c.y);
                    let h = match cdt.locate_vertex(p) {
                        Some(v) => v.fix(),
                        None => cdt
                            .insert(CdtVertex { pos: p, grid: None })
                            .map_err(|e| anyhow::anyhow!("shore vertex insertion failed: {e:?}"))?,
                    };
                    handles.push(h);
                }
                for w in handles.windows(2) {
                    if w[0] != w[1] {
                        constraints += cdt
                            .add_constraint_and_split(w[0], w[1], |pos| CdtVertex { pos, grid: None })
                            .len();
                    }
                }
            }
        }

        // ── Classify faces and find shore vertices ───────────────────────────
        let nv = cdt.num_vertices();
        let mut face_water = vec![false; cdt.num_all_faces()];
        let mut has_land = vec![false; nv];
        let mut has_water = vec![false; nv];
        for face in cdt.inner_faces() {
            let [a, b, c] = face.positions();
            let w = hydro.is_water((a.x + b.x + c.x) / 3.0, (a.y + b.y + c.y) / 3.0);
            face_water[face.fix().index()] = w;
            for v in face.vertices() {
                if w {
                    has_water[v.fix().index()] = true;
                } else {
                    has_land[v.fix().index()] = true;
                }
            }
        }

        // ── Vertex heights (mesh units, pre-normalisation) ───────────────────
        // Land: DEM (grid points) — except shore vertices, which sit at the
        // water level. Water: level − recess.
        let mut land_z = vec![0.0f32; nv];
        let mut water_z = vec![0.0f32; nv];
        for v in cdt.vertices() {
            let i = v.fix().index();
            let p = v.position();
            if has_water[i] {
                let level = hydro.level_at(p.x, p.y);
                water_z[i] = (level - z_min) * ts - self.water_recess;
                land_z[i] = (level - z_min) * ts;
            } else {
                let z = match v.data().grid {
                    Some(g) => points[g as usize].z,
                    None => hydro.ground_z(p.x, p.y).unwrap_or(z_min),
                };
                land_z[i] = (z - z_min) * ts;
            }
        }
        // Bank clamp: land next to the shore never dips below the waterline,
        // otherwise the bank would read as a pit lower than the water itself.
        let mut clamped = 0usize;
        for face in cdt.inner_faces() {
            if face_water[face.fix().index()] {
                continue;
            }
            let ids = face.vertices().map(|v| v.fix().index());
            let shore_max = ids.iter().filter(|&&i| has_water[i]).map(|&i| land_z[i]).fold(f32::NEG_INFINITY, f32::max);
            if shore_max.is_finite() {
                for &i in &ids {
                    if !has_water[i] && land_z[i] < shore_max {
                        land_z[i] = shore_max;
                        clamped += 1;
                    }
                }
            }
        }

        // ── Emit vertices: one copy per side a vertex touches ────────────────
        let mut vertices: Vec<[f32; 3]> = Vec::with_capacity(nv + nv / 8);
        let mut land_idx = vec![usize::MAX; nv];
        let mut water_idx = vec![usize::MAX; nv];
        for v in cdt.vertices() {
            let i = v.fix().index();
            let p = v.position();
            if has_land[i] {
                land_idx[i] = vertices.len();
                vertices.push([p.x as f32, p.y as f32, land_z[i]]);
            }
            if has_water[i] {
                water_idx[i] = vertices.len();
                vertices.push([p.x as f32, p.y as f32, water_z[i]]);
            }
        }

        // ── Emit triangles: land, shore walls, water ─────────────────────────
        let mut land_tris: Vec<[usize; 3]> = Vec::new();
        let mut walls: Vec<[usize; 3]> = Vec::new();
        let mut water_tris: Vec<[usize; 3]> = Vec::new();
        for face in cdt.inner_faces() {
            let w = face_water[face.fix().index()];
            let map = if w { &water_idx } else { &land_idx };
            let [a, b, c] = face.vertices().map(|v| map[v.fix().index()]);
            if w {
                water_tris.push([a, b, c]);
                continue;
            }
            land_tris.push([a, b, c]);
            // Every land/water edge (constraint or not — classification is
            // the single source of truth) gets a wall facing the water.
            for e in face.adjacent_edges() {
                let Some(other) = e.rev().face().as_inner() else { continue };
                if !face_water[other.fix().index()] {
                    continue;
                }
                let (from, to) = (e.from().fix().index(), e.to().fix().index());
                let (al, bl, aw, bw) = (land_idx[from], land_idx[to], water_idx[from], water_idx[to]);
                walls.push([bl, al, aw]);
                walls.push([bl, aw, bw]);
            }
        }

        let water_tc = water_tris.len();
        let mut triangles = land_tris;
        let n_land = triangles.len();
        let n_walls = walls.len();
        triangles.extend(walls);
        triangles.extend(water_tris);

        tracing::info!("   │  Grid points: {}, shoreline constraint edges: {}", points.len(), constraints);
        tracing::info!("   │  ✓ Terrain mesh generated:");
        tracing::info!("   │    ├─ Vertices:  {}", vertices.len());
        tracing::info!(
            "   │    ├─ Triangles: {} land + {} shore wall + {} water",
            n_land, n_walls, water_tc
        );
        tracing::info!("   │    └─ Bank clamp raised {} land vertex(es) to the waterline", clamped);
        tracing::info!("   └─ Water recess: {:.3} mesh units below the shore", self.water_recess);

        Ok((TerrainMesh { vertices, triangles }, water_tc))
    }

    /// Generate terrain mesh from elevation points using Delaunay triangulation
    fn generate_plain_terrain(&self, points: &[ProjectedPoint]) -> Result<TerrainMesh> {
        tracing::info!("   ┌─ Mesh Generator: Terrain Surface");
        tracing::info!("   │  Algorithm: Delaunay Triangulation");
        tracing::info!("   │  Input: {} elevation points", points.len());
        tracing::info!("   │  Terrain scale: {:.2}x (adaptive)", self.terrain_scale);

        if points.is_empty() {
            tracing::error!("   │  ✗ No elevation points provided!");
            anyhow::bail!("No elevation points provided");
        }

        let coords: Vec<delaunator::Point> = points
            .iter()
            .map(|p| delaunator::Point { x: p.x, y: p.y })
            .collect();

        let x_coords: Vec<f64> = points.iter().map(|p| p.x).collect();
        let y_coords: Vec<f64> = points.iter().map(|p| p.y).collect();
        let z_coords: Vec<f32> = points.iter().map(|p| p.z).collect();

        let x_min = x_coords.iter().cloned().min_by(|a, b| a.partial_cmp(b).unwrap()).unwrap_or(0.0);
        let x_max = x_coords.iter().cloned().max_by(|a, b| a.partial_cmp(b).unwrap()).unwrap_or(0.0);
        let y_min = y_coords.iter().cloned().min_by(|a, b| a.partial_cmp(b).unwrap()).unwrap_or(0.0);
        let y_max = y_coords.iter().cloned().max_by(|a, b| a.partial_cmp(b).unwrap()).unwrap_or(0.0);
        let z_min = z_coords.iter().cloned().min_by(|a, b| a.partial_cmp(b).unwrap()).unwrap_or(0.0);
        let z_max = z_coords.iter().cloned().max_by(|a, b| a.partial_cmp(b).unwrap()).unwrap_or(0.0);

        tracing::info!("   │  Terrain dimensions (meters):");
        tracing::info!("   │    ├─ X (East-West):  {:.2}m to {:.2}m (width: {:.2}m)", x_min, x_max, x_max - x_min);
        tracing::info!("   │    ├─ Y (North-South): {:.2}m to {:.2}m (depth: {:.2}m)", y_min, y_max, y_max - y_min);
        tracing::info!("   │    └─ Z (Elevation):  {:.2}m to {:.2}m (range: {:.2}m)", z_min, z_max, z_max - z_min);

        let triangulation = delaunator::triangulate(&coords);

        // Elevation is stored relative to the terrain minimum so that z=0 is always
        // the lowest point and vertical_scale is a pure exaggeration of actual relief.
        let mut vertices = Vec::with_capacity(points.len());
        for point in points {
            vertices.push([
                point.x as f32,
                point.y as f32,
                (point.z - z_min) * self.terrain_scale,
            ]);
        }

        // delaunator emits clockwise triangles; flip them to counter-clockwise
        // (normal up) like the building caps and the water path, so the closed
        // solid's normals point outward instead of inside-out.
        let mut triangles = Vec::new();
        for i in (0..triangulation.triangles.len()).step_by(3) {
            triangles.push([
                triangulation.triangles[i],
                triangulation.triangles[i + 2],
                triangulation.triangles[i + 1],
            ]);
        }

        tracing::info!("   │  ✓ Terrain mesh generated:");
        tracing::info!("   │    ├─ Vertices:  {}", vertices.len());
        tracing::info!("   │    └─ Triangles: {}", triangles.len());
        tracing::info!("   └─ Scaled relief: 0 → {:.2} (terrain_scale={:.2}x)", (z_max - z_min) * self.terrain_scale, self.terrain_scale);

        Ok(TerrainMesh { vertices, triangles })
    }

    /// Add building extrusions to the mesh.
    /// `terrain_vertex_count` is the number of terrain vertices already in the mesh —
    /// used to sample the terrain elevation beneath each building footprint.
    /// Returns one placement record per input building (vertices_added == 0 means
    /// the element produced no geometry) for the geometry debug report.
    pub fn add_buildings(
        &self,
        mesh: &mut TerrainMesh,
        buildings: &[Building],
        terrain_vertex_count: usize,
    ) -> Result<Vec<BuildingPlacement>> {
        tracing::info!("   ┌─ Mesh Generator: Building Extrusions");
        tracing::info!("   │  Buildings to process: {}", buildings.len());
        tracing::info!("   │  Building scale: {:.2}x", self.building_scale);

        let vertices_before = mesh.vertices.len();
        let triangles_before = mesh.triangles.len();

        // Build a spatial index of terrain vertices once so we can look up ground
        // elevation cheaply while mutably borrowing the mesh to push geometry.
        let terrain = TerrainIndex::build(mesh.vertices[..terrain_vertex_count].to_vec());

        // ── Pass 1: project rings and resolve base elevations ────────────────
        // Each building gets the MIN ground under its own footprint. Stacked
        // building:part elements (min_height > 0) then ADOPT the base of the
        // ground element containing their centroid: the DEM can vary by metres
        // across one landmark, and independently-anchored parts end up floating
        // above (or sunk into) the part they stand on.
        struct Prepared {
            outer: Vec<(f32, f32)>,
            holes: Vec<Vec<(f32, f32)>>,
            base: f32,
        }
        let mut prepared: Vec<Option<Prepared>> = Vec::with_capacity(buildings.len());
        for building in buildings {
            let mut outer = match self.project_clean_ring(&building.footprint) {
                Ok(r) if r.len() >= 3 => r,
                _ => {
                    prepared.push(None);
                    continue;
                }
            };
            // Outer winds counter-clockwise so wall + cap normals point outward/up;
            // holes wind clockwise so the same wall loop faces INTO the courtyard
            // (also the conventional earcut orientation).
            if signed_area(&outer) < 0.0 {
                outer.reverse();
            }
            let mut holes = Vec::new();
            for h in &building.holes {
                if let Ok(mut ring) = self.project_clean_ring(h) {
                    if ring.len() >= 3 {
                        if signed_area(&ring) > 0.0 {
                            ring.reverse();
                        }
                        // Pull the courtyard a hair toward its centroid: OSM inner
                        // rings routinely share nodes/edges with the outer ring, and
                        // a hole touching the boundary makes the ear-clip bridge
                        // reuse an edge (non-manifold). ~0.1% is invisible in print.
                        let (cx, cy) = ring_centroid(&ring);
                        for p in ring.iter_mut() {
                            p.0 = cx + (p.0 - cx) * 0.999;
                            p.1 = cy + (p.1 - cy) * 0.999;
                        }
                        holes.push(ring);
                    }
                }
            }
            // Anchor the base to the LOWEST ground under the footprint. A single
            // flat base on sloped terrain otherwise leaves large buildings floating
            // (a gap on the downhill side) or sunk on the uphill side; taking the
            // minimum embeds the uphill edge instead, so the building meets the ground.
            let mut base = f32::INFINITY;
            for &(x, y) in &outer {
                if let Some(z) = terrain.nearest_z(x, y) {
                    base = base.min(z);
                }
            }
            if !base.is_finite() {
                base = 0.0;
            }
            prepared.push(Some(Prepared { outer, holes, base }));
        }

        // Ground elements (anything starting at terrain level) that can support
        // stacked parts: (index, bbox, area), most specific (smallest) first.
        let mut supporters: Vec<(usize, [f32; 4], f32)> = Vec::new();
        for (i, b) in buildings.iter().enumerate() {
            if b.min_height < 0.5 {
                if let Some(p) = &prepared[i] {
                    let bb = ring_bbox(&p.outer);
                    supporters.push((i, bb, (bb[2] - bb[0]) * (bb[3] - bb[1])));
                }
            }
        }
        supporters.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));

        let adopted_bases: Vec<Option<f32>> = buildings
            .iter()
            .enumerate()
            .map(|(i, b)| {
                if b.min_height < 0.5 {
                    return None;
                }
                let p = prepared[i].as_ref()?;
                let c = ring_centroid(&p.outer);
                supporters
                    .iter()
                    .find(|(j, bb, _)| {
                        *j != i
                            && c.0 >= bb[0] && c.0 <= bb[2]
                            && c.1 >= bb[1] && c.1 <= bb[3]
                            && point_in_ring_f32(c, &prepared[*j].as_ref().unwrap().outer)
                    })
                    .map(|(j, _, _)| prepared[*j].as_ref().unwrap().base)
            })
            .collect();
        for (i, adopted) in adopted_bases.iter().enumerate() {
            if let (Some(base), Some(p)) = (adopted, prepared[i].as_mut()) {
                p.base = *base;
            }
        }

        // ── Pass 2: extrude ──────────────────────────────────────────────────
        let mut placements = Vec::with_capacity(buildings.len());
        let mut successful = 0;
        let mut skipped = 0;

        for (i, building) in buildings.iter().enumerate() {
            let Some(p) = &prepared[i] else {
                skipped += 1;
                placements.push(BuildingPlacement::skipped(building.id, "degenerate footprint"));
                continue;
            };
            match self.add_building_to_mesh(mesh, building, &p.outer, &p.holes, p.base) {
                Ok(p) => {
                    successful += 1;
                    placements.push(p);
                }
                Err(e) => {
                    tracing::debug!("   │  Skipped building {}: {}", i, e);
                    skipped += 1;
                    placements.push(BuildingPlacement::skipped(building.id, &e.to_string()));
                }
            }
        }

        let vertices_added = mesh.vertices.len() - vertices_before;
        let triangles_added = mesh.triangles.len() - triangles_before;

        tracing::info!("   │  ✓ Successfully extruded {} buildings ({} skipped)", successful, skipped);
        tracing::info!("   │    ├─ Vertices added:  {}", vertices_added);
        tracing::info!("   │    └─ Triangles added: {}", triangles_added);
        tracing::info!("   └─ Building extrusion complete");

        Ok(placements)
    }

    /// Project a lon/lat ring to local metric coordinates and drop consecutive
    /// duplicate / near-duplicate vertices (incl. the repeated closing node of
    /// OSM ways). These create zero-area slivers that make earcut emit
    /// degenerate triangles and produce visible rendering artifacts.
    fn project_clean_ring(&self, ring: &[(f64, f64)]) -> Result<Vec<(f32, f32)>> {
        const EPS_M: f32 = 0.05;
        let mut projected: Vec<(f32, f32)> = Vec::with_capacity(ring.len());
        for &(lon, lat) in ring {
            let point = self.projector.project(lon, lat, 0.0)?;
            projected.push((point.x as f32, point.y as f32));
        }
        let mut clean: Vec<(f32, f32)> = Vec::with_capacity(projected.len());
        for &p in &projected {
            if let Some(&last) = clean.last() {
                if (p.0 - last.0).abs() < EPS_M && (p.1 - last.1).abs() < EPS_M {
                    continue;
                }
            }
            clean.push(p);
        }
        if clean.len() >= 2 {
            let (f, l) = (clean[0], *clean.last().unwrap());
            if (f.0 - l.0).abs() < EPS_M && (f.1 - l.1).abs() < EPS_M {
                clean.pop();
            }
        }
        Ok(clean)
    }

    /// Add a single building solid, verifying it is combinatorially CLOSED
    /// (every edge shared by exactly two triangles). Ear clipping occasionally
    /// degenerates on awkward real-world footprints (hole bridges reusing an
    /// edge, collinear runs from bbox clipping); instead of poisoning the whole
    /// mesh, such a solid is rolled back and retried without courtyard holes,
    /// and skipped entirely if even that fails.
    fn add_building_to_mesh(
        &self,
        mesh: &mut TerrainMesh,
        building: &Building,
        outer: &[(f32, f32)],
        holes: &[Vec<(f32, f32)>],
        base_elevation: f32,
    ) -> Result<BuildingPlacement> {
        let (v0, t0) = (mesh.vertices.len(), mesh.triangles.len());
        let rollback = |mesh: &mut TerrainMesh| {
            mesh.vertices.truncate(v0);
            mesh.triangles.truncate(t0);
        };

        match self.emit_building_solid(mesh, building, outer, holes, base_elevation) {
            Ok(p) if solid_is_closed(&mesh.triangles[t0..]) => return Ok(p),
            Ok(_) => rollback(mesh),
            Err(e) => {
                rollback(mesh);
                return Err(e);
            }
        }

        if holes.is_empty() {
            anyhow::bail!("solid not closed (degenerate footprint)");
        }
        tracing::debug!(
            "   │  Building {}: courtyard triangulation degenerated — retrying without holes",
            building.id
        );
        match self.emit_building_solid(mesh, building, outer, &[], base_elevation) {
            Ok(p) if solid_is_closed(&mesh.triangles[t0..]) => Ok(p),
            Ok(_) => {
                rollback(mesh);
                anyhow::bail!("solid not closed even without holes");
            }
            Err(e) => {
                rollback(mesh);
                Err(e)
            }
        }
    }

    /// Emit the raw geometry for one building solid.
    ///
    /// Geometry layout:
    ///   - one bottom ring + one top (eave) ring per boundary ring (outer + holes)
    ///   - walls between them (hole rings wind CW so their walls face the courtyard)
    ///   - bottom cap (earcut with holes, facing −Z)
    ///   - roof on the outer top ring according to `roof_shape` (flat cap, ridge,
    ///     pyramid, dome rings, …). Every roof closes the top ring with the same
    ///     ring edges the walls use, so each solid stays combinatorially closed.
    fn emit_building_solid(
        &self,
        mesh: &mut TerrainMesh,
        building: &Building,
        outer: &[(f32, f32)],
        holes: &[Vec<(f32, f32)>],
        base_elevation: f32,
    ) -> Result<BuildingPlacement> {
        let vertices_at_start = mesh.vertices.len();

        // ── Vertical extent ──────────────────────────────────────────────────
        // building_scale is independent of terrain_scale so that building heights
        // stay proportional to reality even when terrain is heavily exaggerated.
        let bottom_z = base_elevation + building.min_height * self.building_scale;
        let mut top_z = base_elevation + building.height * self.building_scale;
        // Printability floor for ground-level outlines only: building:part
        // solids stack on each other's heights and must keep the shared mapping.
        if !building.is_part && building.min_height < 0.5 {
            top_z = top_z.max(bottom_z + self.min_building_height);
        }
        if top_z - bottom_z < 1e-3 {
            anyhow::bail!("zero-height solid");
        }

        // ── Roof selection ───────────────────────────────────────────────────
        // Courtyard solids keep a flat roof: ridge/dome construction assumes a
        // single boundary ring.
        let shape = if holes.is_empty() { building.roof_shape } else { RoofShape::Flat };

        // Footprint extent drives auto roof sizing (untagged roof heights).
        let (mut min_x, mut min_y, mut max_x, mut max_y) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for &(x, y) in outer {
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        }
        let min_extent = (max_x - min_x).min(max_y - min_y).max(0.1);

        // Roof height in mesh Z units. XY stays in real metres until print
        // normalization while Z carries building_scale, so footprint-derived
        // auto heights (hemisphere = half the footprint width) are used as-is
        // and only tag-derived metre values get multiplied by building_scale.
        let mut roof_z = if shape == RoofShape::Flat {
            0.0
        } else if building.roof_height > 0.0 {
            building.roof_height * self.building_scale
        } else {
            match shape {
                RoofShape::Dome => 0.5 * min_extent,
                _ => (3.0 * self.building_scale).min(0.4 * min_extent),
            }
        };
        // The roof never swallows the walls entirely.
        roof_z = roof_z.min(0.8 * (top_z - bottom_z));
        let eave_z = top_z - roof_z;

        // ── Vertex layout: bottom rings, then top (eave) rings ───────────────
        let ring_sizes: Vec<usize> =
            std::iter::once(outer.len()).chain(holes.iter().map(|h| h.len())).collect();
        let total: usize = ring_sizes.iter().sum();

        let base_index = mesh.vertices.len();
        for &(x, y) in outer.iter().chain(holes.iter().flatten()) {
            mesh.vertices.push([x, y, bottom_z]);
        }

        // Skillion roofs tilt the eave ring itself (lowest along the longest
        // edge, rising to the far side); everything else has a level eave.
        let top0 = mesh.vertices.len();
        debug_assert_eq!(top0, base_index + total);
        if shape == RoofShape::Skillion && roof_z > 0.0 {
            let tilt = skillion_heights(&outer, eave_z, roof_z);
            for (k, &(x, y)) in outer.iter().enumerate() {
                mesh.vertices.push([x, y, tilt[k]]);
            }
        } else {
            for &(x, y) in outer {
                mesh.vertices.push([x, y, eave_z]);
            }
        }
        for &(x, y) in holes.iter().flatten() {
            mesh.vertices.push([x, y, eave_z]);
        }

        // ── Walls: one quad (two triangles) per ring edge, with wrap-around ──
        let mut offset = 0;
        for &size in &ring_sizes {
            for i in 0..size {
                let j = (i + 1) % size;
                let bc = base_index + offset + i;
                let bn = base_index + offset + j;
                let tc = top0 + offset + i;
                let tn = top0 + offset + j;
                mesh.triangles.push([bc, bn, tc]);
                mesh.triangles.push([tc, bn, tn]);
            }
            offset += size;
        }

        // ── Bottom cap (and flat/skillion top cap) via ear clipping ──────────
        let mut flat_coords: Vec<f64> = Vec::with_capacity(total * 2);
        for &(x, y) in outer.iter().chain(holes.iter().flatten()) {
            flat_coords.push(x as f64);
            flat_coords.push(y as f64);
        }
        let mut hole_indices: Vec<usize> = Vec::with_capacity(holes.len());
        let mut acc = outer.len();
        for h in holes {
            hole_indices.push(acc);
            acc += h.len();
        }
        let cap = earcutr::earcut(&flat_coords, &hole_indices, 2)
            .map_err(|e| anyhow::anyhow!("earcut failed: {:?}", e))?;
        if cap.is_empty() {
            anyhow::bail!("degenerate footprint (no triangles produced)");
        }

        for t in cap.chunks_exact(3) {
            // Bottom cap faces −Z (reversed winding).
            mesh.triangles.push([base_index + t[0], base_index + t[2], base_index + t[1]]);
        }

        // ── Roof ─────────────────────────────────────────────────────────────
        match shape {
            // A skillion top ring lies on a single tilted plane, so the 2D ear-clip
            // triangulation lifted to the per-vertex heights is exact.
            RoofShape::Flat | RoofShape::Skillion => {
                for t in cap.chunks_exact(3) {
                    mesh.triangles.push([top0 + t[0], top0 + t[1], top0 + t[2]]);
                }
            }
            RoofShape::Pyramidal => {
                add_pyramid_roof(mesh, top0, &outer, eave_z + roof_z);
            }
            RoofShape::Dome => {
                add_dome_roof(mesh, top0, &outer, eave_z, roof_z);
            }
            RoofShape::Gabled | RoofShape::Hipped => {
                if outer.len() == 4 {
                    let inset = if shape == RoofShape::Hipped { 0.3 } else { 0.0 };
                    add_ridge_roof(mesh, top0, &outer, eave_z + roof_z, inset);
                } else {
                    // Non-quad footprint: approximate with a flat-topped hip
                    // (frustum to an inset copy of the ring) — watertight for any
                    // simple polygon and reads as a generic pitched roof in print.
                    add_frustum_roof(mesh, top0, &outer, eave_z + roof_z, &cap);
                }
            }
        }

        Ok(BuildingPlacement {
            id: building.id,
            base_elevation,
            bottom_z,
            eave_z,
            top_z,
            roof_z,
            vertices_added: mesh.vertices.len() - vertices_at_start,
            skip_reason: None,
        })
    }

    /// Close the mesh by adding a flat base pedestal, making it watertight for 3D printing.
    pub fn close_mesh_with_base(&self, mesh: &mut TerrainMesh) -> Result<()> {
        tracing::info!("   ┌─ Mesh Generator: Base Closure (Watertight)");
        tracing::info!("   │  Base height: {:.2} mm", self.base_height);

        let original_vertex_count = mesh.vertices.len();
        let original_triangle_count = mesh.triangles.len();

        let min_z = mesh.vertices.iter().map(|v| v[2]).min_by(|a, b| a.partial_cmp(b).unwrap()).unwrap_or(0.0);
        let max_z = mesh.vertices.iter().map(|v| v[2]).max_by(|a, b| a.partial_cmp(b).unwrap()).unwrap_or(0.0);
        let base_z = min_z - self.base_height;

        tracing::info!("   │  Z-range: {:.2} to {:.2} mm (total: {:.2} mm)", min_z, max_z, max_z - min_z);
        tracing::info!("   │  Base Z-level: {:.2} mm", base_z);

        // Mirror every top vertex to the flat base plate
        let bottom_vertices: Vec<[f32; 3]> = mesh.vertices[..original_vertex_count]
            .iter()
            .map(|v| [v[0], v[1], base_z])
            .collect();
        mesh.vertices.extend(bottom_vertices);

        // Flipped bottom triangles
        for i in 0..original_triangle_count {
            let tri = mesh.triangles[i];
            mesh.triangles.push([
                tri[0] + original_vertex_count,
                tri[2] + original_vertex_count,
                tri[1] + original_vertex_count,
            ]);
        }

        // Side walls: a directed edge (v1→v2) is a boundary edge iff (v2→v1) never
        // appears in any top-surface triangle. Connect each boundary edge to the base.
        use std::collections::HashSet;
        let mut directed_edges: HashSet<(usize, usize)> = HashSet::new();
        for i in 0..original_triangle_count {
            let tri = mesh.triangles[i];
            directed_edges.insert((tri[0], tri[1]));
            directed_edges.insert((tri[1], tri[2]));
            directed_edges.insert((tri[2], tri[0]));
        }

        let mut side_triangles: Vec<[usize; 3]> = Vec::new();
        for i in 0..original_triangle_count {
            let tri = mesh.triangles[i];
            for k in 0..3 {
                let v1 = tri[k];
                let v2 = tri[(k + 1) % 3];
                if !directed_edges.contains(&(v2, v1)) {
                    let bv1 = v1 + original_vertex_count;
                    let bv2 = v2 + original_vertex_count;
                    side_triangles.push([v1, bv1, bv2]);
                    side_triangles.push([v1, bv2, v2]);
                }
            }
        }
        let side_wall_count = side_triangles.len();
        mesh.triangles.extend(side_triangles);

        tracing::info!("   │  ✓ Added {} bottom + {} side-wall triangles", original_triangle_count, side_wall_count);
        tracing::info!("   │  Final: {} vertices, {} triangles", mesh.vertices.len(), mesh.triangles.len());
        tracing::info!("   └─ Mesh is a closed solid");

        Ok(())
    }

    /// Scale all mesh vertices so the largest XY dimension equals `target_size_mm`.
    /// Z is scaled by the same factor. Call this BEFORE close_mesh_with_base so
    /// that base_height is applied in mm units.
    pub fn normalize_to_print_size(&self, mesh: &mut TerrainMesh, target_size_mm: f32) {
        let x_min = mesh.vertices.iter().map(|v| v[0]).min_by(|a, b| a.partial_cmp(b).unwrap()).unwrap_or(0.0);
        let x_max = mesh.vertices.iter().map(|v| v[0]).max_by(|a, b| a.partial_cmp(b).unwrap()).unwrap_or(1.0);
        let y_min = mesh.vertices.iter().map(|v| v[1]).min_by(|a, b| a.partial_cmp(b).unwrap()).unwrap_or(0.0);
        let y_max = mesh.vertices.iter().map(|v| v[1]).max_by(|a, b| a.partial_cmp(b).unwrap()).unwrap_or(1.0);
        let z_max = mesh.vertices.iter().map(|v| v[2]).max_by(|a, b| a.partial_cmp(b).unwrap()).unwrap_or(0.0);

        let x_range = (x_max - x_min).max(f32::EPSILON);
        let y_range = (y_max - y_min).max(f32::EPSILON);
        let scale = target_size_mm / x_range.max(y_range);

        tracing::info!("   ┌─ Mesh Normalizer: Print Dimensions");
        tracing::info!("   │  XY: {:.1}m × {:.1}m → {:.1}mm × {:.1}mm", x_range, y_range, x_range * scale, y_range * scale);
        tracing::info!("   │  Z max after scaling: {:.2}mm", z_max * scale);
        tracing::info!("   └─ Scale factor: {:.6}", scale);

        for vertex in &mut mesh.vertices {
            vertex[0] = (vertex[0] - x_min) * scale;
            vertex[1] = (vertex[1] - y_min) * scale;
            vertex[2] *= scale;
        }
    }

    /// Validate that the mesh is manifold (every edge shared by exactly 2 triangles).
    pub fn validate_manifold(&self, mesh: &TerrainMesh) -> Result<bool> {
        tracing::info!("   ┌─ Mesh Validator: Manifold Check");

        use std::collections::HashMap;
        let mut edge_count: HashMap<(usize, usize), usize> = HashMap::new();

        for triangle in &mesh.triangles {
            for i in 0..3 {
                let v1 = triangle[i];
                let v2 = triangle[(i + 1) % 3];
                let edge = if v1 < v2 { (v1, v2) } else { (v2, v1) };
                *edge_count.entry(edge).or_insert(0) += 1;
            }
        }

        let edges_with_1_face = edge_count.values().filter(|&&c| c == 1).count();
        let edges_with_2_faces = edge_count.values().filter(|&&c| c == 2).count();
        let edges_with_more = edge_count.values().filter(|&&c| c > 2).count();

        tracing::info!("   │  Total unique edges: {}", edge_count.len());
        tracing::info!("   │    ├─ 1-face (holes):       {}", edges_with_1_face);
        tracing::info!("   │    ├─ 2-face (valid):        {}", edges_with_2_faces);
        tracing::info!("   │    └─ >2-face (non-manifold):{}", edges_with_more);

        let is_manifold = edge_count.values().all(|&c| c == 2);

        if is_manifold {
            tracing::info!("   └─ ✓ Mesh is perfectly manifold");
        } else {
            tracing::warn!("   └─ ⚠ Mesh is NOT manifold ({} problem edges)", edges_with_1_face + edges_with_more);
        }

        Ok(is_manifold)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{BoundingBox, ElevationPoint, WaterArea, WaterFeatures, WaterKind, Waterway};
    use crate::services::water::{assemble_sea, SeaOutcome};

    const RES: u32 = 40;

    fn bbox() -> BoundingBox {
        // ~1.1 km × 1.2 km near Bologna.
        BoundingBox { min_lat: 44.49, min_lon: 11.33, max_lat: 44.50, max_lon: 11.345 }
    }

    fn rect(lon0: f64, lat0: f64, lon1: f64, lat1: f64) -> Vec<(f64, f64)> {
        vec![(lon0, lat0), (lon1, lat0), (lon1, lat1), (lon0, lat1)]
    }

    /// Project a synthetic DEM, build the water model, triangulate, close and
    /// validate. Returns (mesh before base closure, water_tc, hydro, z_min).
    fn build(
        features: &WaterFeatures,
        sea: &[WaterArea],
        z: impl Fn(f64, f64) -> f32,
    ) -> (TerrainMesh, usize, HydroModel, bool, f32) {
        let b = bbox();
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
        let hydro = HydroModel::build(features, sea, "test", &projector, &mut points, (RES + 1) as usize, mm_per_m)
            .unwrap()
            .expect("water expected");
        let z_min = points.iter().map(|p| p.z).fold(f32::INFINITY, f32::min);
        let generator = MeshGenerator::new(projector, 1.0, 1.0, 0.0, 0.6 / mm_per_m as f32, 2.0);
        let (mesh, water_tc) = generator.generate_terrain_mesh(&points, Some(&hydro)).unwrap();
        let mut closed = TerrainMesh { vertices: mesh.vertices.clone(), triangles: mesh.triangles.clone() };
        generator.close_mesh_with_base(&mut closed).unwrap();
        let manifold = generator.validate_manifold(&closed).unwrap();
        (mesh, water_tc, hydro, manifold, z_min)
    }

    fn signed_area_xy(m: &TerrainMesh, t: &[usize; 3]) -> f32 {
        let [a, b, c] = t.map(|i| m.vertices[i]);
        ((b[0] - a[0]) * (c[1] - a[1]) - (c[0] - a[0]) * (b[1] - a[1])) * 0.5
    }

    fn lake(outer: Vec<(f64, f64)>) -> WaterFeatures {
        WaterFeatures {
            areas: vec![WaterArea { id: 1, kind: WaterKind::Lake, outer, holes: Vec::new() }],
            ..Default::default()
        }
    }

    // Gentle deterministic DEM noise around 50 m.
    fn noisy(lon: f64, lat: f64) -> f32 {
        50.0 + ((lon * 7919.0).sin() * (lat * 6271.0).cos()) as f32 * 0.8
    }

    #[test]
    fn lake_in_the_middle_is_flat_recessed_and_watertight() {
        let (mesh, water_tc, hydro, manifold, z_min) = build(&lake(rect(11.336, 44.493, 11.339, 44.497)), &[], noisy);
        assert!(manifold, "terrain + lake + base must be a closed 2-manifold");
        assert!(water_tc > 0);
        let n = mesh.triangles.len();
        let water = &mesh.triangles[n - water_tc..];
        let zs: Vec<f32> = water.iter().flatten().map(|&i| mesh.vertices[i][2]).collect();
        let (lo, hi) = zs.iter().fold((f32::MAX, f32::MIN), |(l, h), &z| (l.min(z), h.max(z)));
        assert!(hi - lo < 1e-4, "a lake is ONE level ({lo}..{hi})");
        // Level comes from the shoreline (DEM 49.2–50.8), recessed below it.
        let level = hydro.level_at(0.0, 0.0);
        assert!((49.0..51.0).contains(&level), "level {level}");
        assert!(lo < (level - z_min) - 1e-3, "water surface sits below its shore");
        // Everything faces up (CCW seen from above), like the building caps
        // (shore walls are vertical: zero XY area). f32 rounding may flip a
        // sub-mm² sliver, nothing larger.
        for t in mesh.triangles[..n - water_tc].iter().chain(water) {
            let a = signed_area_xy(&mesh, t);
            assert!(a >= -1e-2, "downward-facing triangle (area {a} m²)");
        }
    }

    #[test]
    fn lake_cut_by_the_bbox_edge_stays_watertight() {
        // Extends ~400 m past the eastern edge.
        let (_, water_tc, _, manifold, _) = build(&lake(rect(11.341, 44.493, 11.35, 44.497)), &[], noisy);
        assert!(water_tc > 0);
        assert!(manifold);
    }

    #[test]
    fn plain_terrain_faces_up_too() {
        let b = bbox();
        let projector = CoordinateProjector::new(&b).unwrap();
        let elev: Vec<ElevationPoint> = (0..=10)
            .flat_map(|i| (0..=10).map(move |j| (i, j)))
            .map(|(i, j)| ElevationPoint {
                lat: b.min_lat + i as f64 * 0.001,
                lon: b.min_lon + j as f64 * 0.0015,
                elevation: 10.0,
            })
            .collect();
        let points = projector.project_points(&elev).unwrap();
        let generator = MeshGenerator::new(projector, 1.0, 1.0, 0.0, 0.0, 2.0);
        let (mesh, water_tc) = generator.generate_terrain_mesh(&points, None).unwrap();
        assert_eq!(water_tc, 0);
        // f32 rounding can flip a hull sliver; real triangles all face up.
        let down = mesh.triangles.iter().filter(|t| signed_area_xy(&mesh, t) < -1e-2).count();
        assert_eq!(down, 0, "delaunator path orientation");
    }

    #[test]
    fn sea_from_coastline_ignores_bathymetry() {
        // Coastline running west → east through the middle: land north, sea
        // south. The DEM carries 30 m deep bathymetry offshore.
        let b = bbox();
        let mid = (b.min_lat + b.max_lat) / 2.0;
        let features = WaterFeatures {
            coastlines: vec![(7, vec![(11.32, mid), (11.35, mid)])],
            ..Default::default()
        };
        let sea = match assemble_sea(&features.coastlines, &b, RES) {
            SeaOutcome::Built { polygons, .. } => polygons,
            other => panic!("{other:?}"),
        };
        let (mesh, water_tc, hydro, manifold, z_min) =
            build(&features, &sea, |_, lat| if lat < mid { -30.0 } else { 4.0 });
        assert!(manifold);
        assert!(water_tc > 0);
        assert!(z_min >= -1e-6, "bathymetry must be flattened to sea level (z_min {z_min})");
        assert_eq!(hydro.level_at(0.0, -300.0), 0.0);
        let n = mesh.triangles.len();
        assert!(mesh.triangles[n - water_tc..].iter().flatten().all(|&i| mesh.vertices[i][2] < 0.0));
    }

    #[test]
    fn river_profile_never_runs_uphill() {
        // River flowing east on a valley floor sloping 50 → 40 m, with a
        // 6 m "bridge" bump in the DEM halfway.
        let b = bbox();
        let mid = (b.min_lat + b.max_lat) / 2.0;
        let features = WaterFeatures {
            waterways: vec![Waterway {
                id: 3,
                kind: WaterKind::River,
                line: vec![(11.325, mid), (11.3375, mid), (11.35, mid)],
                width_m: 40.0,
            }],
            ..Default::default()
        };
        let z = |lon: f64, _lat: f64| {
            let t = ((lon - 11.33) / 0.015) as f32;
            let bridge = if (lon - 11.3375).abs() < 0.0006 { 6.0 } else { 0.0 };
            50.0 - 10.0 * t + bridge
        };
        let (_, water_tc, hydro, manifold, _) = build(&features, &[], z);
        assert!(manifold);
        assert!(water_tc > 0);
        // Sample the surface along the centreline, west → east.
        let projector = CoordinateProjector::new(&b).unwrap();
        let levels: Vec<f32> = (0..=20)
            .map(|k| {
                let lon = 11.331 + k as f64 * 0.013 / 20.0;
                let p = projector.project(lon, mid, 0.0).unwrap();
                hydro.level_at(p.x, p.y)
            })
            .collect();
        assert!(levels.windows(2).all(|w| w[0] >= w[1] - 1e-4), "uphill water: {levels:?}");
        assert!(levels[0] - levels[20] > 5.0, "profile keeps the valley slope: {levels:?}");
        assert!(levels.iter().all(|&l| l < 51.0), "bridge bump leaked into the profile: {levels:?}");
    }

    #[test]
    fn connected_canals_share_one_level() {
        // A T of two canals; the DEM along the eastern branch is polluted by
        // buildings (8 m) while the rest sits at 2 m.
        let b = bbox();
        let mid = (b.min_lat + b.max_lat) / 2.0;
        let canal = |id, line| Waterway { id, kind: WaterKind::Canal, line, width_m: 20.0 };
        let features = WaterFeatures {
            waterways: vec![
                canal(1, vec![(11.325, mid), (11.3375, mid)]),
                canal(2, vec![(11.3375, mid), (11.35, mid)]),
                canal(3, vec![(11.3375, 44.505), (11.3375, mid)]),
            ],
            ..Default::default()
        };
        let (_, _, hydro, manifold, _) = build(&features, &[], |lon, _| if lon > 11.339 { 8.0 } else { 2.0 });
        assert!(manifold);
        let projector = CoordinateProjector::new(&b).unwrap();
        let at = |lon: f64, lat: f64| {
            let p = projector.project(lon, lat, 0.0).unwrap();
            hydro.level_at(p.x, p.y)
        };
        let (west, east, north) = (at(11.332, mid), at(11.343, mid), at(11.3375, 44.498));
        assert!((west - east).abs() < 1e-4 && (west - north).abs() < 1e-4, "{west} {east} {north}");
        assert!(west < 5.0, "network level must not follow the polluted branch: {west}");
    }

    #[test]
    fn basin_open_to_the_sea_is_at_sea_level() {
        let b = bbox();
        let mid = (b.min_lat + b.max_lat) / 2.0;
        let mut features = WaterFeatures {
            coastlines: vec![(7, vec![(11.32, mid), (11.35, mid)])],
            ..Default::default()
        };
        // A harbour basin straddling the coastline, quays at 6 m.
        features.areas.push(WaterArea {
            id: 9,
            kind: WaterKind::Lake,
            outer: rect(11.336, mid - 0.001, 11.339, mid + 0.002),
            holes: Vec::new(),
        });
        let sea = match assemble_sea(&features.coastlines, &b, RES) {
            SeaOutcome::Built { polygons, .. } => polygons,
            other => panic!("{other:?}"),
        };
        let (_, _, hydro, manifold, _) = build(&features, &sea, |_, lat| if lat < mid { -10.0 } else { 6.0 });
        assert!(manifold);
        let projector = CoordinateProjector::new(&b).unwrap();
        let p = projector.project(11.3375, mid + 0.0015, 0.0).unwrap();
        assert_eq!(hydro.level_at(p.x, p.y), 0.0, "inner part of the basin");
    }

    #[test]
    fn tributary_buffer_does_not_raise_the_main_river() {
        // A river area (with its centreline) at ~40 m, and a hillside stream
        // at ~55 m whose buffered mouth overlaps the river area.
        let b = bbox();
        let mid = (b.min_lat + b.max_lat) / 2.0;
        let features = WaterFeatures {
            areas: vec![WaterArea {
                id: 1,
                kind: WaterKind::River,
                outer: rect(11.325, mid - 0.0006, 11.35, mid + 0.0006),
                holes: Vec::new(),
            }],
            waterways: vec![
                Waterway { id: 2, kind: WaterKind::River, line: vec![(11.325, mid), (11.35, mid)], width_m: 100.0 },
                Waterway {
                    id: 3,
                    kind: WaterKind::Stream,
                    line: vec![(11.3375, 44.4995), (11.3375, mid)],
                    width_m: 30.0,
                },
            ],
            ..Default::default()
        };
        let (_, _, hydro, manifold, _) =
            build(&features, &[], |_, lat| if (lat - mid).abs() < 0.0007 { 40.0 } else { 55.0 });
        assert!(manifold);
        let projector = CoordinateProjector::new(&b).unwrap();
        // Inside the river area, right where the stream's buffer overlaps it.
        let p = projector.project(11.3375, mid + 0.0004, 0.0).unwrap();
        let l = hydro.level_at(p.x, p.y);
        assert!(l < 42.0, "river level polluted by the tributary: {l}");
    }

    #[test]
    fn short_stretch_on_a_bridge_does_not_own_the_river() {
        // A river area with its centreline at ~40 m, and a short "stream"
        // mapped across it right where a bridge deck (52 m) sits in the DEM.
        let b = bbox();
        let mid = (b.min_lat + b.max_lat) / 2.0;
        let features = WaterFeatures {
            areas: vec![WaterArea {
                id: 1,
                kind: WaterKind::River,
                outer: rect(11.325, mid - 0.0008, 11.35, mid + 0.0008),
                holes: Vec::new(),
            }],
            waterways: vec![
                Waterway { id: 2, kind: WaterKind::River, line: vec![(11.325, mid), (11.35, mid)], width_m: 100.0 },
                Waterway {
                    id: 3,
                    kind: WaterKind::Stream,
                    line: vec![(11.3375, mid - 0.0005), (11.3375, mid + 0.0005)],
                    width_m: 10.0,
                },
            ],
            ..Default::default()
        };
        let (_, _, hydro, manifold, _) = build(&features, &[], |lon, lat| {
            if (lat - mid).abs() >= 0.0009 {
                55.0
            } else if (lon - 11.3375).abs() < 0.0003 {
                52.0 // bridge deck
            } else {
                40.0
            }
        });
        assert!(manifold);
        let projector = CoordinateProjector::new(&b).unwrap();
        let p = projector.project(11.3376, mid + 0.0003, 0.0).unwrap();
        let l = hydro.level_at(p.x, p.y);
        assert!(l < 42.0, "bridge stretch leaked into the river level: {l}");
    }
}
