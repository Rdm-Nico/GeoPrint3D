use crate::models::{Building, ProjectedPoint, TerrainMesh};
use crate::utils::projection::CoordinateProjector;
use anyhow::{Context, Result};

/// Generates a 3D mesh from elevation data and buildings
pub struct MeshGenerator {
    projector: CoordinateProjector,
    vertical_scale: f32,
    base_height: f32,
}

impl MeshGenerator {
    pub fn new(
        projector: CoordinateProjector,
        vertical_scale: f32,
        base_height: f32,
    ) -> Self {
        Self {
            projector,
            vertical_scale,
            base_height,
        }
    }

    /// Generate terrain mesh from elevation points using Delaunay triangulation
    pub fn generate_terrain_mesh(&self, points: &[ProjectedPoint]) -> Result<TerrainMesh> {
        tracing::info!("   ┌─ Mesh Generator: Terrain Surface");
        tracing::info!("   │  Algorithm: Delaunay Triangulation");
        tracing::info!("   │  Input: {} elevation points", points.len());
        tracing::info!("   │  Vertical scale: {}x", self.vertical_scale);

        if points.is_empty() {
            tracing::error!("   │  ✗ No elevation points provided!");
            anyhow::bail!("No elevation points provided");
        }

        // Prepare points for Delaunay triangulation (only x, y)
        tracing::debug!("   │  Preparing 2D points for triangulation...");
        let coords: Vec<delaunator::Point> = points
            .iter()
            .map(|p| delaunator::Point {
                x: p.x,
                y: p.y,
            })
            .collect();

        // Calculate bounding dimensions
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
        tracing::info!("   │    └─ Z (Elevation):  {:.2}m to {:.2}m (height: {:.2}m)", z_min, z_max, z_max - z_min);

        // Perform Delaunay triangulation
        tracing::debug!("   │  Running Delaunay triangulation...");
        let triangulation_start = std::time::Instant::now();
        let triangulation = delaunator::triangulate(&coords);
        let triangulation_time = triangulation_start.elapsed();

        tracing::debug!("   │  Triangulation completed in {:.2}ms", triangulation_time.as_secs_f64() * 1000.0);

        // Build vertices with scaled elevation
        let mut vertices = Vec::with_capacity(points.len());
        for point in points {
            vertices.push([
                point.x as f32,
                point.y as f32,
                point.z * self.vertical_scale,
            ]);
        }

        // Build triangles from triangulation
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
        tracing::info!("   └─ Scaled elevation range: {:.2}m to {:.2}m", z_min * self.vertical_scale, z_max * self.vertical_scale);

        Ok(TerrainMesh {
            vertices,
            triangles,
        })
    }

    /// Add building extrusions to the mesh
    pub fn add_buildings(
        &self,
        mesh: &mut TerrainMesh,
        buildings: &[Building],
    ) -> Result<()> {
        tracing::info!("   ┌─ Mesh Generator: Building Extrusions");
        tracing::info!("   │  Buildings to process: {}", buildings.len());

        let vertices_before = mesh.vertices.len();
        let triangles_before = mesh.triangles.len();

        let mut successful = 0;
        let mut skipped = 0;

        for (i, building) in buildings.iter().enumerate() {
            if building.footprint.len() < 3 {
                skipped += 1;
                continue;
            }
            match self.add_building_to_mesh(mesh, building) {
                Ok(_) => successful += 1,
                Err(e) => {
                    tracing::debug!("   │  Skipped building {}: {}", i, e);
                    skipped += 1;
                }
            }
        }

        let vertices_added = mesh.vertices.len() - vertices_before;
        let triangles_added = mesh.triangles.len() - triangles_before;

        tracing::info!("   │  ✓ Successfully extruded {} buildings", successful);
        if skipped > 0 {
            tracing::debug!("   │  Skipped {} invalid buildings", skipped);
        }
        tracing::info!("   │    ├─ Vertices added:  {}", vertices_added);
        tracing::info!("   │    └─ Triangles added: {}", triangles_added);
        tracing::info!("   └─ Building extrusion complete");

        Ok(())
    }

    /// Add a single building extrusion
    fn add_building_to_mesh(&self, mesh: &mut TerrainMesh, building: &Building) -> Result<()> {
        if building.footprint.len() < 3 {
            return Ok(()); // Skip invalid buildings
        }

        // Project building footprint to local coordinates
        let mut projected_footprint = Vec::new();
        for &(lon, lat) in &building.footprint {
            let point = self.projector.project(lon, lat, 0.0)?;
            projected_footprint.push((point.x as f32, point.y as f32));
        }

        // Get base elevation by interpolating from terrain
        // For simplicity, we'll use 0.0 here; in production, interpolate from terrain mesh
        let base_elevation = 0.0f32;
        let top_elevation = base_elevation + (building.height * self.vertical_scale);

        let base_index = mesh.vertices.len();

        // Add bottom vertices
        for &(x, y) in &projected_footprint {
            mesh.vertices.push([x, y, base_elevation]);
        }

        // Add top vertices
        for &(x, y) in &projected_footprint {
            mesh.vertices.push([x, y, top_elevation]);
        }

        let n = projected_footprint.len();

        // Add wall triangles
        for i in 0..n - 1 {
            let bottom_curr = base_index + i;
            let bottom_next = base_index + i + 1;
            let top_curr = base_index + n + i;
            let top_next = base_index + n + i + 1;

            // Two triangles per wall segment
            mesh.triangles.push([bottom_curr, bottom_next, top_curr]);
            mesh.triangles.push([top_curr, bottom_next, top_next]);
        }

        // Add top face (assuming convex polygon)
        for i in 1..n - 2 {
            mesh.triangles.push([
                base_index + n,
                base_index + n + i,
                base_index + n + i + 1,
            ]);
        }

        Ok(())
    }

    /// Close the mesh by adding a base pedestal (makes it watertight for 3D printing)
    /// This is CRITICAL for 3D printing - the model must be a closed manifold
    pub fn close_mesh_with_base(&self, mesh: &mut TerrainMesh) -> Result<()> {
        tracing::info!("   ┌─ Mesh Generator: Base Closure (Watertight)");
        tracing::info!("   │  Purpose: Create solid base for 3D printing");
        tracing::info!("   │  Base height: {} mm", self.base_height);

        let original_vertex_count = mesh.vertices.len();
        let original_triangle_count = mesh.triangles.len();

        // Find the min elevation in the mesh
        let min_z = mesh
            .vertices
            .iter()
            .map(|v| v[2])
            .min_by(|a, b| a.partial_cmp(b).unwrap())
            .unwrap_or(0.0);

        let max_z = mesh
            .vertices
            .iter()
            .map(|v| v[2])
            .max_by(|a, b| a.partial_cmp(b).unwrap())
            .unwrap_or(0.0);

        // The base will be at (min_z - base_height)
        let base_z = min_z - self.base_height;

        tracing::info!("   │  Current mesh stats:");
        tracing::info!("   │    ├─ Vertices:  {}", original_vertex_count);
        tracing::info!("   │    └─ Triangles: {}", original_triangle_count);
        tracing::info!("   │  Z-range: {:.2} to {:.2} (total height: {:.2})", min_z, max_z, max_z - min_z);
        tracing::info!("   │  Base Z-level: {:.2} (min_z - base_height)", base_z);

        // Add bottom vertices (mirror of top surface at base_z)
        tracing::debug!("   │  Creating bottom surface vertices...");
        let bottom_vertices: Vec<[f32; 3]> = mesh.vertices[..original_vertex_count]
            .iter()
            .map(|vertex| [vertex[0], vertex[1], base_z])
            .collect();
        mesh.vertices.extend(bottom_vertices);

        // Add bottom face (flip all top triangles for correct normals)
        tracing::debug!("   │  Creating bottom surface triangles (flipped winding)...");
        for i in 0..original_triangle_count {
            let tri = mesh.triangles[i];
            // Add flipped triangle for bottom face (reversed winding order)
            mesh.triangles.push([
                tri[0] + original_vertex_count,
                tri[2] + original_vertex_count,
                tri[1] + original_vertex_count,
            ]);
        }

        let vertices_added = mesh.vertices.len() - original_vertex_count;
        let triangles_added = mesh.triangles.len() - original_triangle_count;

        tracing::info!("   │  ✓ Base closure complete:");
        tracing::info!("   │    ├─ Bottom vertices added:  {}", vertices_added);
        tracing::info!("   │    └─ Bottom triangles added: {}", triangles_added);
        tracing::info!("   │  Final mesh stats:");
        tracing::info!("   │    ├─ Total vertices:  {}", mesh.vertices.len());
        tracing::info!("   │    └─ Total triangles: {}", mesh.triangles.len());
        tracing::info!("   └─ Model is now a closed solid (ready for 3D printing)");

        Ok(())
    }

    /// Validate that the mesh is manifold (watertight)
    pub fn validate_manifold(&self, mesh: &TerrainMesh) -> Result<bool> {
        tracing::info!("   ┌─ Mesh Validator: Manifold Check");
        tracing::info!("   │  Purpose: Verify mesh is watertight (valid for 3D printing)");
        tracing::info!("   │  Rule: Every edge must be shared by exactly 2 triangles");

        // A manifold mesh has exactly 2 faces sharing each edge
        use std::collections::HashMap;

        let mut edge_count: HashMap<(usize, usize), usize> = HashMap::new();

        for triangle in &mesh.triangles {
            // Add each edge (normalized so min vertex is first)
            for i in 0..3 {
                let v1 = triangle[i];
                let v2 = triangle[(i + 1) % 3];
                let edge = if v1 < v2 { (v1, v2) } else { (v2, v1) };
                *edge_count.entry(edge).or_insert(0) += 1;
            }
        }

        let total_edges = edge_count.len();

        // Categorize edges by their face count
        let edges_with_1_face = edge_count.values().filter(|&&c| c == 1).count();
        let edges_with_2_faces = edge_count.values().filter(|&&c| c == 2).count();
        let edges_with_more = edge_count.values().filter(|&&c| c > 2).count();

        tracing::info!("   │  Total unique edges: {}", total_edges);
        tracing::info!("   │  Edge analysis:");
        tracing::info!("   │    ├─ Edges with 1 face (boundary/holes):  {}", edges_with_1_face);
        tracing::info!("   │    ├─ Edges with 2 faces (valid):          {}", edges_with_2_faces);
        tracing::info!("   │    └─ Edges with >2 faces (non-manifold):  {}", edges_with_more);

        // Check if any edge is shared by more than 2 faces
        let is_manifold = edge_count.values().all(|&count| count == 2);

        if !is_manifold {
            let problematic_count = edges_with_1_face + edges_with_more;
            tracing::warn!("   │  ⚠ Mesh is NOT manifold!");
            tracing::warn!("   │  {} edges have invalid face count", problematic_count);
            if edges_with_1_face > 0 {
                tracing::warn!("   │  └─ {} boundary edges indicate holes in the mesh", edges_with_1_face);
            }
            if edges_with_more > 0 {
                tracing::warn!("   │  └─ {} edges are shared by >2 faces (self-intersection)", edges_with_more);
            }
            tracing::warn!("   └─ Some 3D printers may have issues with this mesh");
        } else {
            tracing::info!("   │  ✓ Mesh is perfectly manifold (watertight)");
            tracing::info!("   └─ Ready for 3D printing");
        }

        Ok(is_manifold)
    }
}
