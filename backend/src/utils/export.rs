use crate::models::TerrainMesh;
use anyhow::{Context, Result};
use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

/// Exports mesh to STL format for 3D printing
pub struct MeshExporter;

impl MeshExporter {
    /// Export mesh to binary STL file
    pub fn export_stl(mesh: &TerrainMesh, path: &Path) -> Result<()> {
        let file = File::create(path).context("Failed to create STL file")?;
        let mut writer = BufWriter::new(file);

        // Convert to stl_io format
        let mut triangles = Vec::new();

        for tri_indices in &mesh.triangles {
            let v0 = mesh.vertices[tri_indices[0]];
            let v1 = mesh.vertices[tri_indices[1]];
            let v2 = mesh.vertices[tri_indices[2]];

            // Calculate normal vector
            let edge1 = [v1[0] - v0[0], v1[1] - v0[1], v1[2] - v0[2]];
            let edge2 = [v2[0] - v0[0], v2[1] - v0[1], v2[2] - v0[2]];

            let normal = [
                edge1[1] * edge2[2] - edge1[2] * edge2[1],
                edge1[2] * edge2[0] - edge1[0] * edge2[2],
                edge1[0] * edge2[1] - edge1[1] * edge2[0],
            ];

            // Normalize
            let length = (normal[0] * normal[0] + normal[1] * normal[1] + normal[2] * normal[2])
                .sqrt();
            let normal = if length > 0.0 {
                [normal[0] / length, normal[1] / length, normal[2] / length]
            } else {
                [0.0, 0.0, 1.0]
            };

            triangles.push(stl_io::Triangle {
                normal: stl_io::Normal::new(normal),
                vertices: [
                    stl_io::Vertex::new(v0),
                    stl_io::Vertex::new(v1),
                    stl_io::Vertex::new(v2),
                ],
            });
        }

        stl_io::write_stl(&mut writer, triangles.iter())
            .context("Failed to write STL file")?;

        tracing::info!(
            "Exported STL file: {} triangles to {}",
            triangles.len(),
            path.display()
        );

        Ok(())
    }

    /// Export a simplified mesh for preview (reduce triangle count)
    pub fn export_preview_stl(mesh: &TerrainMesh, path: &Path, reduction_factor: usize) -> Result<()> {
        // Simple decimation: take every Nth triangle
        let mut simplified_mesh = TerrainMesh {
            vertices: mesh.vertices.clone(),
            triangles: mesh
                .triangles
                .iter()
                .step_by(reduction_factor)
                .cloned()
                .collect(),
        };

        Self::export_stl(&simplified_mesh, path)
    }

    /// Export mesh to OBJ format (alternative to STL)
    pub fn export_obj(mesh: &TerrainMesh, path: &Path) -> Result<()> {
        use std::io::Write;

        let file = File::create(path).context("Failed to create OBJ file")?;
        let mut writer = BufWriter::new(file);

        // Write vertices
        for vertex in &mesh.vertices {
            writeln!(writer, "v {} {} {}", vertex[0], vertex[1], vertex[2])?;
        }

        // Write faces (OBJ uses 1-based indexing)
        for triangle in &mesh.triangles {
            writeln!(
                writer,
                "f {} {} {}",
                triangle[0] + 1,
                triangle[1] + 1,
                triangle[2] + 1
            )?;
        }

        tracing::info!(
            "Exported OBJ file: {} vertices, {} faces to {}",
            mesh.vertices.len(),
            mesh.triangles.len(),
            path.display()
        );

        Ok(())
    }

    /// Convert mesh to bytes for HTTP response
    pub fn to_stl_bytes(mesh: &TerrainMesh) -> Result<Vec<u8>> {
        let mut buffer = Vec::new();

        let mut triangles = Vec::new();
        for tri_indices in &mesh.triangles {
            let v0 = mesh.vertices[tri_indices[0]];
            let v1 = mesh.vertices[tri_indices[1]];
            let v2 = mesh.vertices[tri_indices[2]];

            let edge1 = [v1[0] - v0[0], v1[1] - v0[1], v1[2] - v0[2]];
            let edge2 = [v2[0] - v0[0], v2[1] - v0[1], v2[2] - v0[2]];

            let normal = [
                edge1[1] * edge2[2] - edge1[2] * edge2[1],
                edge1[2] * edge2[0] - edge1[0] * edge2[2],
                edge1[0] * edge2[1] - edge1[1] * edge2[0],
            ];

            let length = (normal[0] * normal[0] + normal[1] * normal[1] + normal[2] * normal[2])
                .sqrt();
            let normal = if length > 0.0 {
                [normal[0] / length, normal[1] / length, normal[2] / length]
            } else {
                [0.0, 0.0, 1.0]
            };

            triangles.push(stl_io::Triangle {
                normal: stl_io::Normal::new(normal),
                vertices: [
                    stl_io::Vertex::new(v0),
                    stl_io::Vertex::new(v1),
                    stl_io::Vertex::new(v2),
                ],
            });
        }

        stl_io::write_stl(&mut buffer, triangles.iter())
            .context("Failed to write STL to buffer")?;

        Ok(buffer)
    }
}
