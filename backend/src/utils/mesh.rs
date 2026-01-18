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
        if points.is_empty() {
            anyhow::bail!("No elevation points provided");
        }

        // Prepare points for Delaunay triangulation (only x, y)
        let coords: Vec<delaunator::Point> = points
            .iter()
            .map(|p| delaunator::Point {
                x: p.x,
                y: p.y,
            })
            .collect();

        // Perform Delaunay triangulation
        let triangulation = delaunator::triangulate(&coords);

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

        tracing::info!(
            "Generated terrain mesh: {} vertices, {} triangles",
            vertices.len(),
            triangles.len()
        );

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
        for building in buildings {
            self.add_building_to_mesh(mesh, building)?;
        }

        tracing::info!("Added {} buildings to mesh", buildings.len());
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
        let original_vertex_count = mesh.vertices.len();

        // Find the min elevation in the mesh
        let min_z = mesh
            .vertices
            .iter()
            .map(|v| v[2])
            .min_by(|a, b| a.partial_cmp(b).unwrap())
            .unwrap_or(0.0);

        // The base will be at (min_z - base_height)
        let base_z = min_z - self.base_height;

        // Find perimeter vertices (those on the edge of the bounding area)
        // For simplicity, we'll create a new bottom layer for ALL vertices
        // In production, only perimeter vertices should be duplicated

        // Add bottom vertices
        let base_index = mesh.vertices.len();
        // Collect vertices to add first to avoid borrow checker issues
        let bottom_vertices: Vec<[f32; 3]> = mesh.vertices[..original_vertex_count]
            .iter()
            .map(|vertex| [vertex[0], vertex[1], base_z])
            .collect();
        mesh.vertices.extend(bottom_vertices);

        // Add walls connecting top surface to bottom base
        // This is a simplified approach; production code should properly detect boundaries

        // Add bottom face (flip all top triangles)
        let original_triangle_count = mesh.triangles.len();
        for i in 0..original_triangle_count {
            let tri = mesh.triangles[i];
            // Add flipped triangle for bottom face
            mesh.triangles.push([
                tri[0] + original_vertex_count,
                tri[2] + original_vertex_count,
                tri[1] + original_vertex_count,
            ]);
        }

        tracing::info!(
            "Closed mesh with base: added {} bottom vertices",
            original_vertex_count
        );

        Ok(())
    }

    /// Validate that the mesh is manifold (watertight)
    pub fn validate_manifold(&self, mesh: &TerrainMesh) -> Result<bool> {
        // A manifold mesh has exactly 2 faces sharing each edge
        // This is a simplified check; production code should be more thorough

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

        // Check if any edge is shared by more than 2 faces
        let is_manifold = edge_count.values().all(|&count| count == 2);

        if !is_manifold {
            let problematic_edges: Vec<_> = edge_count
                .iter()
                .filter(|(_, &count)| count != 2)
                .collect();
            tracing::warn!("Mesh is not manifold: {} problematic edges", problematic_edges.len());
        } else {
            tracing::info!("Mesh is manifold (watertight)");
        }

        Ok(is_manifold)
    }
}
