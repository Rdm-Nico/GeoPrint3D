mod api;
mod models;
mod services;
mod utils;

use api::{generate_terrain, health_check, receive_frontend_logs, AppState};
use axum::{
    routing::{get, post},
    Router,
};
use services::{ElevationService, OsmService};
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};
use utils::{init_logging, FrontendLogWriter};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load backend/.env (if present) so MAPBOX_TOKEN can be set there.
    dotenvy::dotenv().ok();

    // Initialise file + console logging with weekly rotation.
    // The guard must stay alive for the duration of the process.
    let _log_guard = init_logging("logs")?;

    // Initialize services.
    // MAPBOX_TOKEN (optional) enables high-resolution Terrain-RGB elevation;
    // without it the service falls back to the free Open-Elevation API.
    let mapbox_token = std::env::var("MAPBOX_TOKEN").ok();
    let frontend_log_writer = Arc::new(FrontendLogWriter::new("../frontend/logs")?);
    let state = Arc::new(AppState {
        elevation_service: ElevationService::new(mapbox_token),
        osm_service: OsmService::new(),
        frontend_log_writer,
    });

    // Configure CORS
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    // Build router
    let app = Router::new()
        .route("/api/health", get(health_check))
        .route("/api/generate", post(generate_terrain))
        .route("/api/logs/frontend", post(receive_frontend_logs))
        // TODO: Add download endpoint for serving generated STL files
        // .route("/api/download/:filename", get(download_file))
        .with_state(state)
        .layer(cors);

    // Start server
    let addr = "0.0.0.0:8080";
    tracing::info!("🚀 GeoPrint3D Backend starting on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
