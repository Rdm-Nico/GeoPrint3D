use crate::models::{Building, ProjectedPoint, RoofShape, TerrainMesh};
use crate::utils::projection::CoordinateProjector;
use anyhow::Result;

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
/// (O(1) amortised per query vs an O(n) scan of every terrain vertex).
struct TerrainIndex {
    verts: Vec<[f32; 3]>,
    inv_cell: f32,
    min_x: f32,
    min_y: f32,
    buckets: std::collections::HashMap<(i32, i32), Vec<u32>>,
}

impl TerrainIndex {
    fn build(verts: Vec<[f32; 3]>) -> Self {
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
    fn nearest_z(&self, x: f32, y: f32) -> Option<f32> {
        if self.verts.is_empty() {
            return None;
        }
        let cx = ((x - self.min_x) * self.inv_cell) as i32;
        let cy = ((y - self.min_y) * self.inv_cell) as i32;
        let mut best_d = f32::MAX;
        let mut best_z = None;
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
                                best_z = Some(v[2]);
                            }
                        }
                    }
                }
            }
            // One extra ring past the first hit guarantees the true nearest.
            if best_z.is_some() && r >= 1 {
                break;
            }
        }
        best_z
    }
}

/// Generates a 3D mesh from elevation data and buildings
pub struct MeshGenerator {
    projector: CoordinateProjector,
    /// Applied only to terrain elevation. Computed adaptively in the handler so that
    /// terrain relief always reaches a visible fraction of the print width.
    terrain_scale: f32,
    /// Applied to building heights. Equal to the user's vertical_scale preference
    /// so buildings stay proportional to what the user expects.
    building_scale: f32,
    base_height: f32,
}

impl MeshGenerator {
    pub fn new(
        projector: CoordinateProjector,
        terrain_scale: f32,
        building_scale: f32,
        base_height: f32,
    ) -> Self {
        Self {
            projector,
            terrain_scale,
            building_scale,
            base_height,
        }
    }

    /// Generate terrain mesh from elevation points using Delaunay triangulation
    pub fn generate_terrain_mesh(&self, points: &[ProjectedPoint]) -> Result<TerrainMesh> {
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

        let mut triangles = Vec::new();
        for i in (0..triangulation.triangles.len()).step_by(3) {
            triangles.push([
                triangulation.triangles[i],
                triangulation.triangles[i + 1],
                triangulation.triangles[i + 2],
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

    /// Add a single building solid, placing its base on the terrain surface.
    ///
    /// Geometry layout:
    ///   - one bottom ring + one top (eave) ring per boundary ring (outer + holes)
    ///   - walls between them (hole rings wind CW so their walls face the courtyard)
    ///   - bottom cap (earcut with holes, facing −Z)
    ///   - roof on the outer top ring according to `roof_shape` (flat cap, ridge,
    ///     pyramid, dome rings, …). Every roof closes the top ring with the same
    ///     ring edges the walls use, so each solid stays combinatorially closed.
    fn add_building_to_mesh(
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
        let top_z = base_elevation + building.height * self.building_scale;
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
