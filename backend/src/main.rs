mod api;
mod models;
mod services;
mod utils;

use api::{generate_terrain, health_check, AppState};
use axum::{
    routing::{get, post},
    Router,
};
use services::{ElevationService, OsmService};
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "geoprint3d_backend=debug,tower_http=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Initialize services
    let state = Arc::new(AppState {
        elevation_service: ElevationService::new(),
        osm_service: OsmService::new(),
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
