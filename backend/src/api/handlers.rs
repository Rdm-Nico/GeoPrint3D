use crate::models::{GenerateRequest, GenerateResponse, MeshData, MeshStats};
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
    let total_start = std::time::Instant::now();

    tracing::info!("╔══════════════════════════════════════════════════════════════╗");
    tracing::info!("║           STARTING 3D TERRAIN GENERATION                     ║");
    tracing::info!("╚══════════════════════════════════════════════════════════════╝");

    tracing::info!("📍 Input bounding box:");
    tracing::info!("   └─ SW corner: ({:.6}°, {:.6}°)", request.bbox.min_lat, request.bbox.min_lon);
    tracing::info!("   └─ NE corner: ({:.6}°, {:.6}°)", request.bbox.max_lat, request.bbox.max_lon);
    tracing::info!("📊 Requested parameters:");
    tracing::info!("   └─ Resolution: {:?} (grid points per axis)", request.resolution);
    tracing::info!("   └─ Vertical scale: {:?}x", request.vertical_scale);
    tracing::info!("   └─ Base height: {:?} mm", request.base_height);
    tracing::info!("   └─ Include buildings: {:?}", request.include_buildings);

    // ═══════════════════════════════════════════════════════════════
    // STEP 1: Validate bounding box area
    // ═══════════════════════════════════════════════════════════════
    tracing::info!("─────────────────────────────────────────────────────────────────");
    tracing::info!("📋 STEP 1/8: Validating bounding box area");
    let step_start = std::time::Instant::now();

    let area_km2 = request
        .bbox
        .validate_area()
        .map_err(|e| ApiError::BadRequest(e))?;

    tracing::info!("   ✓ Area validated: {:.4} km² (limit: 1.0 km²)", area_km2);
    tracing::info!("   ⏱ Step completed in {:.2}ms", step_start.elapsed().as_secs_f64() * 1000.0);

    // ═══════════════════════════════════════════════════════════════
    // STEP 2: Set default parameters
    // ═══════════════════════════════════════════════════════════════
    tracing::info!("─────────────────────────────────────────────────────────────────");
    tracing::info!("⚙️  STEP 2/8: Applying default parameters");

    let resolution = request.resolution.unwrap_or(100);
    let vertical_scale = request.vertical_scale.unwrap_or(1.5);
    let base_height = request.base_height.unwrap_or(2.0);
    let include_buildings = request.include_buildings.unwrap_or(true);

    tracing::info!("   └─ Resolution: {} ({}x{} = {} grid points)",
        resolution, resolution, resolution, resolution * resolution);
    tracing::info!("   └─ Vertical scale: {}x (elevation exaggeration for visibility)", vertical_scale);
    tracing::info!("   └─ Base height: {} mm (solid base for 3D printing)", base_height);
    tracing::info!("   └─ Include buildings: {}", include_buildings);

    // ═══════════════════════════════════════════════════════════════
    // STEP 3: Fetch elevation data from external API
    // ═══════════════════════════════════════════════════════════════
    tracing::info!("─────────────────────────────────────────────────────────────────");
    tracing::info!("🌍 STEP 3/8: Fetching elevation data from Open-Elevation API");
    tracing::info!("   └─ This step makes an HTTP request to external API...");
    let step_start = std::time::Instant::now();

    let elevation_points = state
        .elevation_service
        .fetch_elevation_grid(&request.bbox, resolution)
        .await
        .map_err(|e| {
            tracing::error!("   ✗ FAILED to fetch elevation data: {}", e);
            tracing::error!("   └─ Possible causes:");
            tracing::error!("      • Open-Elevation API is down or unreachable");
            tracing::error!("      • Network connectivity issues");
            tracing::error!("      • Request timeout (too many points)");
            ApiError::InternalError(format!("Failed to fetch elevation data: {}", e))
        })?;

    if elevation_points.is_empty() {
        tracing::error!("   ✗ API returned empty elevation data");
        return Err(ApiError::InternalError(
            "No elevation data received from API".to_string(),
        ));
    }

    let elev_min = elevation_points.iter().map(|p| p.elevation).min_by(|a, b| a.partial_cmp(b).unwrap()).unwrap_or(0.0);
    let elev_max = elevation_points.iter().map(|p| p.elevation).max_by(|a, b| a.partial_cmp(b).unwrap()).unwrap_or(0.0);

    tracing::info!("   ✓ Received {} elevation points", elevation_points.len());
    tracing::info!("   └─ Elevation range: {:.1}m to {:.1}m (Δ{:.1}m)", elev_min, elev_max, elev_max - elev_min);
    tracing::info!("   ⏱ Step completed in {:.2}s", step_start.elapsed().as_secs_f64());

    // ═══════════════════════════════════════════════════════════════
    // STEP 4: Fetch building data from OpenStreetMap
    // ═══════════════════════════════════════════════════════════════
    tracing::info!("─────────────────────────────────────────────────────────────────");
    let step_start = std::time::Instant::now();
    let buildings = if include_buildings {
        tracing::info!("🏢 STEP 4/8: Fetching building data from OpenStreetMap (Overpass API)");
        tracing::info!("   └─ This step makes an HTTP request to Overpass API...");

        match state.osm_service.fetch_buildings(&request.bbox).await {
            Ok(b) => {
                if b.is_empty() {
                    tracing::info!("   ⚠ No buildings found in this area");
                } else {
                    let total_height: f32 = b.iter().map(|bld| bld.height).sum();
                    let avg_height = total_height / b.len() as f32;
                    tracing::info!("   ✓ Found {} buildings", b.len());
                    tracing::info!("   └─ Average building height: {:.1}m", avg_height);
                }
                tracing::info!("   ⏱ Step completed in {:.2}s", step_start.elapsed().as_secs_f64());
                b
            }
            Err(e) => {
                tracing::warn!("   ⚠ Failed to fetch buildings (continuing without): {}", e);
                tracing::warn!("   └─ Possible causes:");
                tracing::warn!("      • Overpass API is overloaded or down");
                tracing::warn!("      • Request timeout");
                Vec::new()
            }
        }
    } else {
        tracing::info!("🏢 STEP 4/8: Skipping building data (disabled by user)");
        Vec::new()
    };

    // ═══════════════════════════════════════════════════════════════
    // STEP 5: Project coordinates from WGS84 to UTM (meters)
    // ═══════════════════════════════════════════════════════════════
    tracing::info!("─────────────────────────────────────────────────────────────────");
    tracing::info!("🗺️  STEP 5/8: Projecting coordinates from WGS84 (lat/lon) to UTM (meters)");
    tracing::info!("   └─ This converts geographic coordinates to metric for accurate 3D printing");
    let step_start = std::time::Instant::now();

    let projector = CoordinateProjector::new(&request.bbox)
        .map_err(|e| {
            tracing::error!("   ✗ FAILED to create coordinate projector: {}", e);
            ApiError::InternalError(format!("Coordinate projection failed: {}", e))
        })?;

    let projected_points = projector.project_points(&elevation_points)
        .map_err(|e| {
            tracing::error!("   ✗ FAILED to project elevation points: {}", e);
            ApiError::InternalError(format!("Point projection failed: {}", e))
        })?;

    tracing::info!("   ✓ Projected {} points to local UTM coordinates", projected_points.len());
    tracing::info!("   ⏱ Step completed in {:.2}ms", step_start.elapsed().as_secs_f64() * 1000.0);

    // ═══════════════════════════════════════════════════════════════
    // STEP 6: Generate 3D terrain mesh using Delaunay triangulation
    // ═══════════════════════════════════════════════════════════════
    tracing::info!("─────────────────────────────────────────────────────────────────");
    tracing::info!("📐 STEP 6/8: Generating 3D terrain mesh");
    tracing::info!("   └─ Using Delaunay triangulation to create triangle mesh from elevation points");
    let step_start = std::time::Instant::now();

    let mesh_generator = MeshGenerator::new(projector, vertical_scale, base_height);
    let mut mesh = mesh_generator.generate_terrain_mesh(&projected_points)
        .map_err(|e| {
            tracing::error!("   ✗ FAILED to generate terrain mesh: {}", e);
            ApiError::InternalError(format!("Mesh generation failed: {}", e))
        })?;

    tracing::info!("   ⏱ Step completed in {:.2}ms", step_start.elapsed().as_secs_f64() * 1000.0);

    // ═══════════════════════════════════════════════════════════════
    // STEP 7: Add building extrusions to mesh
    // ═══════════════════════════════════════════════════════════════
    tracing::info!("─────────────────────────────────────────────────────────────────");
    let step_start = std::time::Instant::now();
    if !buildings.is_empty() {
        tracing::info!("🏗️  STEP 7/8: Adding {} building extrusions to mesh", buildings.len());
        tracing::info!("   └─ Each building footprint is extruded to its height");

        let vertices_before = mesh.vertices.len();
        let triangles_before = mesh.triangles.len();

        mesh_generator.add_buildings(&mut mesh, &buildings)
            .map_err(|e| {
                tracing::error!("   ✗ FAILED to add buildings: {}", e);
                ApiError::InternalError(format!("Building extrusion failed: {}", e))
            })?;

        tracing::info!("   ✓ Added {} vertices and {} triangles for buildings",
            mesh.vertices.len() - vertices_before,
            mesh.triangles.len() - triangles_before);
        tracing::info!("   ⏱ Step completed in {:.2}ms", step_start.elapsed().as_secs_f64() * 1000.0);
    } else {
        tracing::info!("🏗️  STEP 7/8: Skipping building extrusions (no buildings)");
    }

    // ═══════════════════════════════════════════════════════════════
    // STEP 8: Close mesh with base pedestal (CRITICAL for 3D printing)
    // ═══════════════════════════════════════════════════════════════
    tracing::info!("─────────────────────────────────────────────────────────────────");
    tracing::info!("🔒 STEP 8/8: Closing mesh with base pedestal");
    tracing::info!("   └─ CRITICAL: 3D printing requires a watertight (manifold) mesh");
    tracing::info!("   └─ Adding solid base {} mm thick", base_height);
    let step_start = std::time::Instant::now();

    let vertices_before = mesh.vertices.len();
    let triangles_before = mesh.triangles.len();

    mesh_generator.close_mesh_with_base(&mut mesh)
        .map_err(|e| {
            tracing::error!("   ✗ FAILED to close mesh: {}", e);
            ApiError::InternalError(format!("Mesh closure failed: {}", e))
        })?;

    tracing::info!("   ✓ Added {} bottom vertices and {} bottom triangles",
        mesh.vertices.len() - vertices_before,
        mesh.triangles.len() - triangles_before);
    tracing::info!("   ⏱ Step completed in {:.2}ms", step_start.elapsed().as_secs_f64() * 1000.0);

    // ═══════════════════════════════════════════════════════════════
    // Validate mesh is watertight
    // ═══════════════════════════════════════════════════════════════
    tracing::info!("─────────────────────────────────────────────────────────────────");
    tracing::info!("🔍 Validating mesh integrity (manifold check)");

    let is_manifold = mesh_generator.validate_manifold(&mesh)
        .map_err(|e| {
            tracing::error!("   ✗ Mesh validation failed: {}", e);
            ApiError::InternalError(format!("Mesh validation failed: {}", e))
        })?;

    if !is_manifold {
        tracing::warn!("   ⚠ Mesh is NOT perfectly manifold - may have issues with some 3D printers");
    } else {
        tracing::info!("   ✓ Mesh is valid and watertight");
    }

    // ═══════════════════════════════════════════════════════════════
    // Calculate final statistics
    // ═══════════════════════════════════════════════════════════════
    let stats = MeshStats {
        vertices: mesh.vertices.len(),
        triangles: mesh.triangles.len(),
        area_km2,
        min_elevation: elev_min,
        max_elevation: elev_max,
        buildings_count: buildings.len(),
    };

    // Prepare mesh data for frontend rendering
    let mesh_data = MeshData {
        vertices: mesh.vertices.clone(),
        triangles: mesh.triangles.clone(),
    };

    let total_elapsed = total_start.elapsed();

    tracing::info!("╔══════════════════════════════════════════════════════════════╗");
    tracing::info!("║           3D TERRAIN GENERATION COMPLETED                    ║");
    tracing::info!("╚══════════════════════════════════════════════════════════════╝");
    tracing::info!("📊 Final Statistics:");
    tracing::info!("   ├─ Total vertices:     {}", stats.vertices);
    tracing::info!("   ├─ Total triangles:    {}", stats.triangles);
    tracing::info!("   ├─ Buildings included: {}", stats.buildings_count);
    tracing::info!("   ├─ Area covered:       {:.4} km²", stats.area_km2);
    tracing::info!("   ├─ Elevation range:    {:.1}m to {:.1}m", stats.min_elevation, stats.max_elevation);
    tracing::info!("   └─ Mesh watertight:    {}", if is_manifold { "Yes ✓" } else { "No ⚠" });
    tracing::info!("⏱ Total processing time: {:.2}s", total_elapsed.as_secs_f64());
    tracing::info!("📤 Sending mesh data to frontend ({} vertices, {} triangles)",
        mesh_data.vertices.len(), mesh_data.triangles.len());
    tracing::info!("═════════════════════════════════════════════════════════════════");

    Ok(Json(GenerateResponse {
        stats,
        mesh_data,
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
