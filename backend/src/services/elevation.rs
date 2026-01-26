use crate::models::{BoundingBox, ElevationPoint};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
struct OpenTopoResponse {
    results: Vec<ElevationResult>,
}

#[derive(Debug, Deserialize)]
struct ElevationResult {
    latitude: f64,
    longitude: f64,
    elevation: f32,
}

pub struct ElevationService {
    client: reqwest::Client,
}

impl ElevationService {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }

    /// Fetch elevation data for a bounding box
    /// Uses Open-Elevation API (free) or can be swapped for Mapbox Terrain-RGB
    pub async fn fetch_elevation_grid(
        &self,
        bbox: &BoundingBox,
        resolution: u32,
    ) -> Result<Vec<ElevationPoint>> {
        let api_url = "https://api.open-elevation.com/api/v1/lookup";

        tracing::info!("   ┌─ Elevation Service");
        tracing::info!("   │  API: Open-Elevation (free, public)");
        tracing::info!("   │  URL: {}", api_url);
        tracing::info!("   │  Grid: {}x{} = {} total points", resolution + 1, resolution + 1, (resolution + 1) * (resolution + 1));

        let mut points = Vec::new();

        // Generate grid of lat/lon points
        let lat_step = (bbox.max_lat - bbox.min_lat) / resolution as f64;
        let lon_step = (bbox.max_lon - bbox.min_lon) / resolution as f64;

        tracing::debug!("   │  Lat step: {:.6}° ({:.2}m approx)", lat_step, lat_step * 111_000.0);
        tracing::debug!("   │  Lon step: {:.6}° ({:.2}m approx)", lon_step, lon_step * 111_000.0 * (bbox.min_lat.to_radians().cos()));

        let mut locations = Vec::new();
        for i in 0..=resolution {
            for j in 0..=resolution {
                let lat = bbox.min_lat + (i as f64 * lat_step);
                let lon = bbox.min_lon + (j as f64 * lon_step);
                locations.push(serde_json::json!({
                    "latitude": lat,
                    "longitude": lon
                }));
            }
        }

        tracing::info!("   │  Sending POST request with {} location points...", locations.len());
        let request_start = std::time::Instant::now();

        // Batch request to Open-Elevation API
        let response = self
            .client
            .post(api_url)
            .json(&serde_json::json!({ "locations": locations }))
            .timeout(std::time::Duration::from_secs(60))
            .send()
            .await
            .context("Failed to connect to Open-Elevation API - check network connectivity")?;

        let status = response.status();
        let request_time = request_start.elapsed();

        tracing::info!("   │  Response status: {} (took {:.2}s)", status, request_time.as_secs_f64());

        if !status.is_success() {
            let error_body = response.text().await.unwrap_or_else(|_| "Unable to read response body".to_string());
            tracing::error!("   │  API Error Response: {}", error_body);
            anyhow::bail!("Open-Elevation API returned error status {}: {}", status, error_body);
        }

        let parse_start = std::time::Instant::now();
        let data: OpenTopoResponse = response
            .json()
            .await
            .context("Failed to parse elevation JSON response - API may have returned unexpected format")?;

        tracing::debug!("   │  JSON parsing took {:.2}ms", parse_start.elapsed().as_secs_f64() * 1000.0);

        if data.results.is_empty() {
            tracing::warn!("   │  ⚠ API returned empty results array");
            anyhow::bail!("Open-Elevation API returned no elevation data");
        }

        for result in data.results {
            points.push(ElevationPoint {
                lat: result.latitude,
                lon: result.longitude,
                elevation: result.elevation,
            });
        }

        // Calculate some statistics
        let elevations: Vec<f32> = points.iter().map(|p| p.elevation).collect();
        let min_elev = elevations.iter().cloned().min_by(|a, b| a.partial_cmp(b).unwrap()).unwrap_or(0.0);
        let max_elev = elevations.iter().cloned().max_by(|a, b| a.partial_cmp(b).unwrap()).unwrap_or(0.0);
        let avg_elev: f32 = elevations.iter().sum::<f32>() / elevations.len() as f32;

        tracing::info!("   │  ✓ Successfully received {} elevation points", points.len());
        tracing::info!("   │  Elevation stats: min={:.1}m, max={:.1}m, avg={:.1}m", min_elev, max_elev, avg_elev);
        tracing::info!("   └─ Total API call time: {:.2}s", request_time.as_secs_f64());

        Ok(points)
    }

    /// Alternative: Fetch from Mapbox Terrain-RGB tiles
    /// This is more efficient for production use
    pub async fn fetch_mapbox_terrain(
        &self,
        bbox: &BoundingBox,
        resolution: u32,
        mapbox_token: &str,
    ) -> Result<Vec<ElevationPoint>> {
        // TODO: Implement Mapbox Terrain-RGB tile fetching
        // 1. Convert bbox to tile coordinates at appropriate zoom level
        // 2. Fetch RGB tiles
        // 3. Decode RGB values to elevation: height = -10000 + ((R * 256 * 256 + G * 256 + B) * 0.1)
        // 4. Resample to desired resolution

        unimplemented!("Mapbox Terrain-RGB implementation pending")
    }

    /// Alternative: Fetch from SRTM data
    /// Requires local SRTM tile cache or API
    pub async fn fetch_srtm_data(
        &self,
        bbox: &BoundingBox,
        resolution: u32,
    ) -> Result<Vec<ElevationPoint>> {
        // TODO: Implement SRTM data fetching
        // Option 1: Use AWS Open Data SRTM tiles
        // Option 2: Use local SRTM cache
        // Option 3: Use third-party SRTM API

        unimplemented!("SRTM implementation pending")
    }
}

impl Default for ElevationService {
    fn default() -> Self {
        Self::new()
    }
}
