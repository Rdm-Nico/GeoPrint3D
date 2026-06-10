use crate::models::{Building, ProjectedPoint, TerrainMesh};
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
    pub fn add_buildings(
        &self,
        mesh: &mut TerrainMesh,
        buildings: &[Building],
        terrain_vertex_count: usize,
    ) -> Result<()> {
        tracing::info!("   ┌─ Mesh Generator: Building Extrusions");
        tracing::info!("   │  Buildings to process: {}", buildings.len());
        tracing::info!("   │  Building scale: {:.2}x", self.building_scale);

        let vertices_before = mesh.vertices.len();
        let triangles_before = mesh.triangles.len();

        // Clone terrain vertices once so we can look up ground elevation while
        // mutably borrowing the mesh to push building geometry.
        let terrain_verts: Vec<[f32; 3]> = mesh.vertices[..terrain_vertex_count].to_vec();

        let mut successful = 0;
        let mut skipped = 0;

        for (i, building) in buildings.iter().enumerate() {
            if building.footprint.len() < 3 {
                skipped += 1;
                continue;
            }
            match self.add_building_to_mesh(mesh, building, &terrain_verts) {
                Ok(_) => successful += 1,
                Err(e) => {
                    tracing::debug!("   │  Skipped building {}: {}", i, e);
                    skipped += 1;
                }
            }
        }

        let vertices_added = mesh.vertices.len() - vertices_before;
        let triangles_added = mesh.triangles.len() - triangles_before;

        tracing::info!("   │  ✓ Successfully extruded {} buildings ({} skipped)", successful, skipped);
        tracing::info!("   │    ├─ Vertices added:  {}", vertices_added);
        tracing::info!("   │    └─ Triangles added: {}", triangles_added);
        tracing::info!("   └─ Building extrusion complete");

        Ok(())
    }

    /// Add a single building extrusion, placing its base on the terrain surface.
    fn add_building_to_mesh(
        &self,
        mesh: &mut TerrainMesh,
        building: &Building,
        terrain_verts: &[[f32; 3]],
    ) -> Result<()> {
        if building.footprint.len() < 3 {
            return Ok(());
        }

        let mut footprint: Vec<(f32, f32)> = Vec::with_capacity(building.footprint.len());
        for &(lon, lat) in &building.footprint {
            let point = self.projector.project(lon, lat, 0.0)?;
            footprint.push((point.x as f32, point.y as f32));
        }

        // OSM closed ways repeat the first node as the last — drop it before triangulating.
        if footprint.len() >= 2 && footprint.first() == footprint.last() {
            footprint.pop();
        }
        let n = footprint.len();
        if n < 3 {
            return Ok(());
        }

        // Normalise winding to counter-clockwise so the top cap normal points +Z.
        if signed_area(&footprint) < 0.0 {
            footprint.reverse();
        }

        // Find the terrain elevation directly beneath this building by searching for
        // the nearest terrain vertex to the building centroid. This places buildings
        // on the actual ground rather than always at the river/minimum level.
        let cx = footprint.iter().map(|(x, _)| *x).sum::<f32>() / n as f32;
        let cy = footprint.iter().map(|(_, y)| *y).sum::<f32>() / n as f32;

        let base_elevation = if terrain_verts.is_empty() {
            0.0_f32
        } else {
            terrain_verts
                .iter()
                .min_by(|a, b| {
                    let da = (a[0] - cx).powi(2) + (a[1] - cy).powi(2);
                    let db = (b[0] - cx).powi(2) + (b[1] - cy).powi(2);
                    da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
                })
                .map(|v| v[2])
                .unwrap_or(0.0)
        };

        // building_scale is independent of terrain_scale so that building heights
        // stay proportional to reality even when terrain is heavily exaggerated.
        let top_elevation = base_elevation + building.height * self.building_scale;

        // Triangulate the (possibly concave) footprint with ear clipping.
        let flat: Vec<f64> = footprint.iter().flat_map(|&(x, y)| [x as f64, y as f64]).collect();
        let cap = earcutr::earcut(&flat, &[], 2)
            .map_err(|e| anyhow::anyhow!("earcut failed: {:?}", e))?;
        if cap.is_empty() {
            anyhow::bail!("degenerate footprint (no triangles produced)");
        }

        let base_index = mesh.vertices.len();
        for &(x, y) in &footprint {
            mesh.vertices.push([x, y, base_elevation]);
        }
        for &(x, y) in &footprint {
            mesh.vertices.push([x, y, top_elevation]);
        }
        let top0 = base_index + n;

        // Walls: one quad (two triangles) per footprint edge, with wrap-around.
        for i in 0..n {
            let j = (i + 1) % n;
            let bc = base_index + i;
            let bn = base_index + j;
            let tc = top0 + i;
            let tn = top0 + j;
            mesh.triangles.push([bc, bn, tc]);
            mesh.triangles.push([tc, bn, tn]);
        }

        // Caps from the ear-clip result: top CCW (+Z), bottom reversed (−Z).
        for t in cap.chunks_exact(3) {
            mesh.triangles.push([top0 + t[0], top0 + t[1], top0 + t[2]]);
            mesh.triangles.push([base_index + t[0], base_index + t[2], base_index + t[1]]);
        }

        Ok(())
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
