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
    // Initialise file + console logging with weekly rotation.
    // The guard must stay alive for the duration of the process.
    let _log_guard = init_logging("logs")?;

    // Initialize services
    let frontend_log_writer = Arc::new(FrontendLogWriter::new("../frontend/logs")?);
    let state = Arc::new(AppState {
        elevation_service: ElevationService::new(),
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
