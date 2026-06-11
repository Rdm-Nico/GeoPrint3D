use crate::models::{
    BoundingBox, Building, GenerateRequest, GenerateResponse, MeshData, MeshStats, TerrainMesh,
};
use crate::services::{ElevationService, OsmService};
use crate::utils::{
    CoordinateProjector, FrontendLogBatch, FrontendLogWriter, MeshExporter, MeshGenerator,
};
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
    pub frontend_log_writer: Arc<FrontendLogWriter>,
}

/// Receives a batch of frontend log entries and appends them to `frontend/logs/frontend.log`.
pub async fn receive_frontend_logs(
    State(state): State<Arc<AppState>>,
    Json(batch): Json<FrontendLogBatch>,
) -> StatusCode {
    state.frontend_log_writer.write_batch(&batch);
    StatusCode::NO_CONTENT
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

    tracing::info!("   ✓ Area validated: {:.4} km² (limit: 20.0 km²)", area_km2);
    tracing::info!("   ⏱ Step completed in {:.2}ms", step_start.elapsed().as_secs_f64() * 1000.0);

    // ═══════════════════════════════════════════════════════════════
    // STEP 2: Set default parameters
    // ═══════════════════════════════════════════════════════════════
    tracing::info!("─────────────────────────────────────────────────────────────────");
    tracing::info!("⚙️  STEP 2/8: Applying default parameters");

    let resolution = request.resolution.unwrap_or(150);
    // vertical_scale is the user's exaggeration preference, neutral at 2.0 (see
    // the saturating terrain-scale model below).
    let vertical_scale = request.vertical_scale.unwrap_or(2.0);
    let base_height = request.base_height.unwrap_or(2.0);
    let include_buildings = request.include_buildings.unwrap_or(true);
    // Largest XY dimension of the printed model in mm.
    let print_size_mm = request.print_size.unwrap_or(180.0);

    tracing::info!("   └─ Resolution: {} ({}x{} = {} grid points)",
        resolution, resolution, resolution, resolution * resolution);
    tracing::info!("   └─ Vertical scale: {}x (elevation exaggeration for visibility)", vertical_scale);
    tracing::info!("   └─ Base height: {} mm (solid base for 3D printing)", base_height);
    tracing::info!("   └─ Print size: {} mm (largest XY dimension)", print_size_mm);
    tracing::info!("   └─ Include buildings: {}", include_buildings);

    // ═══════════════════════════════════════════════════════════════
    // STEP 3: Fetch elevation data from external API
    // ═══════════════════════════════════════════════════════════════
    tracing::info!("─────────────────────────────────────────────────────────────────");
    tracing::info!("🌍 STEP 3/8: Fetching elevation data from Open-Elevation API");
    tracing::info!("   └─ This step makes an HTTP request to external API...");
    let step_start = std::time::Instant::now();

    let mut elevation_points = state
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

    // Light low-pass over the regular grid to remove DEM quantisation terracing
    // (integer-metre stair-steps) without flattening genuine relief.
    let grid_dim = (resolution + 1) as usize;
    if elevation_points.len() == grid_dim * grid_dim {
        crate::services::elevation::smooth_elevation_grid(&mut elevation_points, grid_dim, grid_dim, 1);
        tracing::info!("   └─ Applied light grid smoothing (1 pass, {}×{})", grid_dim, grid_dim);
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
    // Print-resolution-aware footprint filter: drop buildings whose printed
    // footprint would be smaller than MIN_PRINTED_FOOTPRINT_MM2. On a small
    // bbox this stays at the 10 m² floor (everything prints); on a 20 km² city
    // it rises to ~100+ m², keeping blocks and landmarks while shedding the
    // sub-millimetre houses that would only bloat the mesh.
    let min_building_area_m2 = {
        const MIN_PRINTED_FOOTPRINT_MM2: f64 = 0.2;
        let center_lat = ((request.bbox.min_lat + request.bbox.max_lat) / 2.0).to_radians();
        let lat_extent_m = (request.bbox.max_lat - request.bbox.min_lat) * 111_320.0;
        let lon_extent_m = (request.bbox.max_lon - request.bbox.min_lon) * 111_320.0 * center_lat.cos();
        let mm_per_m = print_size_mm as f64 / lat_extent_m.max(lon_extent_m).max(1.0);
        MIN_PRINTED_FOOTPRINT_MM2 / (mm_per_m * mm_per_m)
    };

    let mut buildings = if !include_buildings {
        tracing::info!("🏢 STEP 4/8: Skipping building data (disabled by user)");
        Vec::new()
    } else {
        tracing::info!("🏢 STEP 4/8: Fetching building data from OpenStreetMap (Overpass API)");
        tracing::info!("   └─ This step makes an HTTP request to Overpass API...");

        match state.osm_service.fetch_buildings(&request.bbox, min_building_area_m2).await {
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
                Vec::new()
            }
        }
    };

    // Soft-knee compression of outlier-tall buildings.
    // Building height scaling is linear, so a real 100 m+ landmark (St Peter's,
    // Asinelli Tower) towers absurdly over a 10 m city. Buildings up to a
    // scene-aware knee (3× the median height, floored at 20 m) stay untouched —
    // preserving the proportions that already look right — while only the extreme
    // outliers above the knee are compressed, so landmarks stay tallest without
    // blowing out the scale.
    if buildings.len() >= 8 {
        const KNEE_MULT: f32 = 3.0;
        const KNEE_FLOOR_M: f32 = 20.0;
        const SLOPE: f32 = 0.30;

        // Median over outlines only: a decomposed landmark (St Peter's has dozens
        // of building:part elements, many of them tall) would otherwise drag the
        // knee upward and escape compression.
        let mut sorted: Vec<f32> = buildings.iter().filter(|b| !b.is_part).map(|b| b.height).collect();
        if sorted.is_empty() {
            sorted = buildings.iter().map(|b| b.height).collect();
        }
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median = sorted[sorted.len() / 2];
        let knee = (median * KNEE_MULT).max(KNEE_FLOOR_M);
        let max_before = buildings.iter().map(|b| b.height).fold(0.0f32, f32::max);

        // One monotone map applied to every vertical coordinate (top, eave,
        // min_height) keeps stacked building:part solids aligned: the eave of a
        // drum still meets the min_height of the dome above it.
        let compress = |h: f32| if h > knee { knee + (h - knee) * SLOPE } else { h };

        let mut compressed = 0usize;
        let mut max_after = 0.0f32;
        for b in &mut buildings {
            let new_top = compress(b.height);
            if new_top < b.height {
                compressed += 1;
                if b.roof_height > 0.0 {
                    let eave = (b.height - b.roof_height).max(0.0);
                    b.roof_height = (new_top - compress(eave)).max(0.0);
                }
                b.height = new_top;
            }
            b.min_height = compress(b.min_height);
            max_after = max_after.max(b.height);
        }
        if compressed > 0 {
            tracing::info!(
                "   └─ Height compression: median {:.0}m, knee {:.0}m → compressed {} element(s); max {:.0}m → {:.0}m",
                median, knee, compressed, max_before, max_after
            );
        }
    }

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
    // Compute scaling (saturating, relief-proportional)
    // ═══════════════════════════════════════════════════════════════
    // Unlike a constant-relief target (which amplifies flat terrain hardest and
    // turns DEM noise into fake hills), this maps real relief through a saturating
    // curve: flat terrain stays flat, real relief is visible, big mountains compress.
    // In urban scenes terrain is further capped so buildings remain the dominant
    // feature. Terrain and building scales are independent.
    let xy_range_m = {
        let x_vals: Vec<f64> = projected_points.iter().map(|p| p.x).collect();
        let y_vals: Vec<f64> = projected_points.iter().map(|p| p.y).collect();
        let x_range = x_vals.iter().cloned().max_by(|a, b| a.partial_cmp(b).unwrap()).unwrap_or(0.0)
            - x_vals.iter().cloned().min_by(|a, b| a.partial_cmp(b).unwrap()).unwrap_or(0.0);
        let y_range = y_vals.iter().cloned().max_by(|a, b| a.partial_cmp(b).unwrap()).unwrap_or(0.0)
            - y_vals.iter().cloned().min_by(|a, b| a.partial_cmp(b).unwrap()).unwrap_or(0.0);
        (x_range.max(y_range) as f32).max(1.0)
    };
    let elevation_range = (elev_max - elev_min).max(1.0_f32);

    // mm per real metre after normalization (matches normalize_to_print_size).
    let mm_per_m = print_size_mm / xy_range_m;
    let relief_m = elevation_range; // already clamped to ≥ 1.0 above

    const MAX_TERRAIN_FRAC: f32 = 0.28; // terrain relief never exceeds this × print size
    const HALF_RELIEF_M: f32 = 120.0;   // relief at which we reach half of the cap
    const NOISE_FLOOR_M: f32 = 3.0;     // below this, terrain is treated as flat
    let max_terrain_mm = MAX_TERRAIN_FRAC * print_size_mm;
    let has_buildings = !buildings.is_empty();

    // Saturating target relief, then user knob (neutral at 2.0).
    let mut target_terrain_mm = if relief_m < NOISE_FLOOR_M {
        1.0 // a thin lip only — keep flat cities flat
    } else {
        max_terrain_mm * relief_m / (relief_m + HALF_RELIEF_M)
    };
    target_terrain_mm *= vertical_scale / 2.0;

    let scene_mode = if relief_m < NOISE_FLOOR_M {
        "flat"
    } else if has_buildings {
        // Keep terrain from drowning the architecture in cities.
        target_terrain_mm = target_terrain_mm.min(12.0 * (vertical_scale / 2.0));
        "urban"
    } else {
        "natural"
    };
    target_terrain_mm = target_terrain_mm.clamp(0.0, max_terrain_mm);

    let effective_terrain_scale = if relief_m > 0.5 {
        target_terrain_mm / (relief_m * mm_per_m)
    } else {
        0.0
    };

    // Buildings: a REFERENCE_BUILDING_M building reaches target_building_mm at vs=2,
    // independent of area size. height_mm = height_m × scale × mm_per_m.
    const REFERENCE_BUILDING_M: f32 = 15.0;
    const TARGET_BUILDING_MM_PER_VS: f32 = 6.0; // → 12mm at vs=2 for a 15m building
    let target_building_mm = TARGET_BUILDING_MM_PER_VS * vertical_scale;
    let effective_building_scale = target_building_mm / (REFERENCE_BUILDING_M * mm_per_m);

    tracing::info!("─────────────────────────────────────────────────────────────────");
    tracing::info!("📐 Scaling model ({} scene):", scene_mode);
    tracing::info!("   ├─ XY extent:        {:.1} m  (→ {:.1}mm print, {:.4} mm/m)", xy_range_m, print_size_mm, mm_per_m);
    tracing::info!("   ├─ Relief:           {:.1} m", relief_m);
    tracing::info!("   ├─ Terrain target:   {:.1} mm  (scale {:.2}x)", target_terrain_mm, effective_terrain_scale);
    tracing::info!("   └─ Building target:  {:.1} mm for {:.0}m ref  (scale {:.2}x)", target_building_mm, REFERENCE_BUILDING_M, effective_building_scale);

    // ═══════════════════════════════════════════════════════════════
    // STEP 6: Generate 3D terrain mesh using Delaunay triangulation
    // ═══════════════════════════════════════════════════════════════
    tracing::info!("─────────────────────────────────────────────────────────────────");
    tracing::info!("📐 STEP 6/8: Generating 3D terrain mesh");
    tracing::info!("   └─ Using Delaunay triangulation to create triangle mesh from elevation points");
    let step_start = std::time::Instant::now();

    let mesh_generator = MeshGenerator::new(projector, effective_terrain_scale, effective_building_scale, base_height);
    let mut mesh = mesh_generator.generate_terrain_mesh(&projected_points)
        .map_err(|e| {
            tracing::error!("   ✗ FAILED to generate terrain mesh: {}", e);
            ApiError::InternalError(format!("Mesh generation failed: {}", e))
        })?;

    // Record terrain vertex/triangle counts before buildings are added.
    // terrain_vertex_count lets add_buildings sample the ground elevation beneath each footprint.
    // terrain_triangle_count is sent to the frontend so it can colour terrain and buildings separately.
    let terrain_vertex_count = mesh.vertices.len();
    let terrain_triangle_count = mesh.triangles.len();
    tracing::info!("   ⏱ Step completed in {:.2}ms", step_start.elapsed().as_secs_f64() * 1000.0);

    // ═══════════════════════════════════════════════════════════════
    // STEP 7: Add building extrusions to mesh
    // ═══════════════════════════════════════════════════════════════
    tracing::info!("─────────────────────────────────────────────────────────────────");
    let step_start = std::time::Instant::now();
    let mut placements = Vec::new();
    if !buildings.is_empty() {
        tracing::info!("🏗️  STEP 7/8: Adding {} building extrusions to mesh", buildings.len());
        tracing::info!("   └─ Buildings placed on terrain surface (not at sea level)");

        let vertices_before = mesh.vertices.len();
        let triangles_before = mesh.triangles.len();

        placements = mesh_generator.add_buildings(&mut mesh, &buildings, terrain_vertex_count)
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
    let building_triangle_count = mesh.triangles.len() - terrain_triangle_count;

    // ═══════════════════════════════════════════════════════════════
    // STEP 7b: Normalize mesh to print dimensions (100mm × 100mm)
    // ═══════════════════════════════════════════════════════════════
    // Must happen BEFORE close_mesh_with_base so that base_height (in mm)
    // is correctly applied after all coordinates are in mm units.
    tracing::info!("─────────────────────────────────────────────────────────────────");
    tracing::info!("📏 STEP 7b: Normalizing mesh to {}mm print dimensions", print_size_mm);
    tracing::info!("   └─ Converts scaled-meter coordinates to mm for 3D printing");
    let step_start = std::time::Instant::now();

    mesh_generator.normalize_to_print_size(&mut mesh, print_size_mm);

    tracing::info!("   ⏱ Step completed in {:.2}ms", step_start.elapsed().as_secs_f64() * 1000.0);

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
    // Debug artifacts: geometry report (always) + STL dump (env-gated)
    // ═══════════════════════════════════════════════════════════════
    write_geometry_report(
        &request.bbox,
        &buildings,
        &placements,
        &mesh,
        terrain_triangle_count,
        building_triangle_count,
        is_manifold,
    );
    if std::env::var("DEBUG_DUMP_STL").map(|v| v != "0").unwrap_or(false) {
        let path = std::path::Path::new("logs/last_mesh.stl");
        match MeshExporter::export_stl(&mesh, path) {
            Ok(_) => tracing::info!("🧪 Debug STL dumped to {}", path.display()),
            Err(e) => tracing::warn!("Failed to dump debug STL: {}", e),
        }
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

    // Prepare mesh data for frontend rendering.
    // terrain_triangle_count / building_triangle_count tell the renderer which triangles
    // to colour as terrain (green) vs buildings (grey), ignoring the base-closure triangles.
    let mesh_data = MeshData {
        vertices: mesh.vertices.clone(),
        triangles: mesh.triangles.clone(),
        terrain_triangle_count,
        building_triangle_count,
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

/// Write a per-generation geometry report to `logs/last_geometry.json` so the
/// building geometry can be evaluated without a 3D viewer: roof-shape histogram,
/// building:part / courtyard counts, the tallest elements, and mesh integrity.
fn write_geometry_report(
    bbox: &BoundingBox,
    buildings: &[Building],
    placements: &[crate::utils::mesh::BuildingPlacement],
    mesh: &TerrainMesh,
    terrain_triangles: usize,
    building_triangles: usize,
    is_manifold: bool,
) {
    use std::collections::BTreeMap;

    let mut roof_shapes: BTreeMap<&'static str, usize> = BTreeMap::new();
    for b in buildings {
        *roof_shapes.entry(b.roof_shape.name()).or_insert(0) += 1;
    }

    // placements is index-aligned with buildings (one record per input element).
    let building_json = |(i, b): (usize, &Building)| {
        let mut v = serde_json::json!({
            "id": b.id,
            "is_part": b.is_part,
            "height_m": b.height,
            "min_height_m": b.min_height,
            "roof": b.roof_shape.name(),
            "roof_height_m": b.roof_height,
            "holes": b.holes.len(),
            "footprint_vertices": b.footprint.len(),
        });
        if let Some(p) = placements.get(i) {
            v["placement"] = serde_json::json!({
                "base_elevation_z": p.base_elevation,
                "bottom_z": p.bottom_z,
                "eave_z": p.eave_z,
                "top_z": p.top_z,
                "roof_z": p.roof_z,
                "vertices_added": p.vertices_added,
                "skip_reason": p.skip_reason,
            });
        }
        v
    };

    let mut by_height: Vec<(usize, &Building)> = buildings.iter().enumerate().collect();
    by_height.sort_by(|a, b| b.1.height.partial_cmp(&a.1.height).unwrap_or(std::cmp::Ordering::Equal));
    let tallest: Vec<serde_json::Value> = by_height.iter().take(15).map(|&e| building_json(e)).collect();

    let report = serde_json::json!({
        "generated_at": chrono::Local::now().to_rfc3339(),
        "bbox": bbox,
        "buildings": {
            "total": buildings.len(),
            "parts": buildings.iter().filter(|b| b.is_part).count(),
            "with_courtyards": buildings.iter().filter(|b| !b.holes.is_empty()).count(),
            "with_min_height": buildings.iter().filter(|b| b.min_height > 0.0).count(),
            "roof_shapes": roof_shapes,
        },
        "mesh": {
            "vertices": mesh.vertices.len(),
            "triangles": mesh.triangles.len(),
            "terrain_triangles": terrain_triangles,
            "building_triangles": building_triangles,
            "manifold": is_manifold,
        },
        "tallest_elements": tallest,
        "elements": buildings.iter().enumerate().map(building_json).collect::<Vec<_>>(),
    });

    match serde_json::to_string_pretty(&report) {
        Ok(json) => {
            if let Err(e) = std::fs::write("logs/last_geometry.json", json) {
                tracing::warn!("Failed to write geometry report: {}", e);
            } else {
                tracing::info!("📝 Geometry report written to logs/last_geometry.json");
            }
        }
        Err(e) => tracing::warn!("Failed to serialise geometry report: {}", e),
    }
}

/// Health check endpoint
pub async fn health_check() -> impl IntoResponse {
    Json(serde_json::json!({
        "status": "healthy",
        "service": "GeoPrint3D",
        "version": "0.1.0"
    }))
}
