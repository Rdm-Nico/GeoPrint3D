use crate::models::{GenerateRequest, GenerateResponse, MeshStats};
use crate::services::{ElevationService, OsmService};
use crate::utils::{CoordinateProjector, MeshExporter, MeshGenerator};
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use std::sync::Arc;

pub struct AppState {
    pub elevation_service: ElevationService,
    pub osm_service: OsmService,
}

/// Error type for API responses
pub enum ApiError {
    BadRequest(String),
    InternalError(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            ApiError::BadRequest(msg) => {
                tracing::warn!("Bad request: {}", msg);
                (StatusCode::BAD_REQUEST, msg.clone())
            }
            ApiError::InternalError(msg) => {
                tracing::error!("Internal error: {}", msg);
                (StatusCode::INTERNAL_SERVER_ERROR, msg.clone())
            }
        };

        (status, Json(serde_json::json!({ "error": message }))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        ApiError::InternalError(err.to_string())
    }
}

/// Main handler for generating 3D terrain models
pub async fn generate_terrain(
    State(state): State<Arc<AppState>>,
    Json(request): Json<GenerateRequest>,
) -> Result<Json<GenerateResponse>, ApiError> {
    let start_time = std::time::Instant::now();
    tracing::info!("=== Starting terrain generation ===");
    tracing::info!("Request parameters: bbox={:?}, resolution={:?}, vertical_scale={:?}, include_buildings={:?}",
        request.bbox, request.resolution, request.vertical_scale, request.include_buildings);

    // 1. Validate bounding box area
    let area_km2 = request
        .bbox
        .validate_area()
        .map_err(|e| ApiError::BadRequest(e))?;

    tracing::info!("Validated area: {:.4} km²", area_km2);

    // 2. Set defaults
    let resolution = request.resolution.unwrap_or(100);
    let vertical_scale = request.vertical_scale.unwrap_or(1.5);
    let base_height = request.base_height.unwrap_or(2.0);
    let include_buildings = request.include_buildings.unwrap_or(true);

    // 3. Fetch elevation data
    tracing::info!("Fetching elevation data...");
    let elevation_points = state
        .elevation_service
        .fetch_elevation_grid(&request.bbox, resolution)
        .await?;

    if elevation_points.is_empty() {
        return Err(ApiError::InternalError(
            "No elevation data received".to_string(),
        ));
    }

    // 4. Fetch building data (if requested)
    let buildings = if include_buildings {
        tracing::info!("Fetching OSM building data...");
        state.osm_service.fetch_buildings(&request.bbox).await?
    } else {
        Vec::new()
    };

    // 5. Project coordinates to UTM
    tracing::info!("Projecting coordinates...");
    let projector = CoordinateProjector::new(&request.bbox)?;
    let projected_points = projector.project_points(&elevation_points)?;

    // 6. Generate mesh
    tracing::info!("Generating terrain mesh...");
    let mesh_generator = MeshGenerator::new(projector, vertical_scale, base_height);
    let mut mesh = mesh_generator.generate_terrain_mesh(&projected_points)?;

    // 7. Add buildings
    if !buildings.is_empty() {
        tracing::info!("Adding {} buildings to mesh...", buildings.len());
        mesh_generator.add_buildings(&mut mesh, &buildings)?;
    }

    // 8. Close mesh with base pedestal (CRITICAL for 3D printing)
    tracing::info!("Closing mesh with base pedestal...");
    mesh_generator.close_mesh_with_base(&mut mesh)?;

    // 9. Validate mesh is watertight
    let is_manifold = mesh_generator.validate_manifold(&mesh)?;
    if !is_manifold {
        tracing::warn!("Generated mesh is not perfectly manifold");
    }

    // 10. Calculate statistics
    let min_elevation = elevation_points
        .iter()
        .map(|p| p.elevation)
        .min_by(|a, b| a.partial_cmp(b).unwrap())
        .unwrap_or(0.0);

    let max_elevation = elevation_points
        .iter()
        .map(|p| p.elevation)
        .max_by(|a, b| a.partial_cmp(b).unwrap())
        .unwrap_or(0.0);

    let stats = MeshStats {
        vertices: mesh.vertices.len(),
        triangles: mesh.triangles.len(),
        area_km2,
        min_elevation,
        max_elevation,
        buildings_count: buildings.len(),
    };

    // 11. Export STL (in production, save to disk and return URLs)
    // For now, we'll generate file paths that the frontend can download
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let stl_filename = format!("terrain_{}.stl", timestamp);
    let preview_filename = format!("terrain_{}_preview.stl", timestamp);

    // In production: save files to disk or object storage
    // MeshExporter::export_stl(&mesh, Path::new(&format!("output/{}", stl_filename)))?;
    // MeshExporter::export_preview_stl(&mesh, Path::new(&format!("output/{}", preview_filename)), 10)?;

    let elapsed = start_time.elapsed();
    tracing::info!(
        "=== Terrain generation completed successfully ===\n  \
        Duration: {:.2}s\n  \
        Vertices: {}\n  \
        Triangles: {}\n  \
        Buildings: {}\n  \
        Elevation range: {:.1}m - {:.1}m",
        elapsed.as_secs_f64(),
        stats.vertices,
        stats.triangles,
        stats.buildings_count,
        stats.min_elevation,
        stats.max_elevation
    );

    Ok(Json(GenerateResponse {
        preview_url: format!("/api/download/{}", preview_filename),
        stl_url: format!("/api/download/{}", stl_filename),
        stats,
    }))
}

/// Health check endpoint
pub async fn health_check() -> impl IntoResponse {
    Json(serde_json::json!({
        "status": "healthy",
        "service": "GeoPrint3D",
        "version": "0.1.0"
    }))
}
