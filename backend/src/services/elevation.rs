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
        tracing::debug!("Generating elevation grid with resolution {}x{}", resolution, resolution);
        let mut points = Vec::new();

        // Generate grid of lat/lon points
        let lat_step = (bbox.max_lat - bbox.min_lat) / resolution as f64;
        let lon_step = (bbox.max_lon - bbox.min_lon) / resolution as f64;

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

        tracing::debug!("Requesting elevation data for {} points from Open-Elevation API", locations.len());

        // Batch request to Open-Elevation API
        // Note: For production, consider using Mapbox Terrain-RGB tiles for better performance
        let response = self
            .client
            .post("https://api.open-elevation.com/api/v1/lookup")
            .json(&serde_json::json!({ "locations": locations }))
            .send()
            .await
            .context("Failed to fetch elevation data")?;

        let data: OpenTopoResponse = response
            .json()
            .await
            .context("Failed to parse elevation response")?;

        tracing::debug!("Received {} elevation points from API", data.results.len());

        for result in data.results {
            points.push(ElevationPoint {
                lat: result.latitude,
                lon: result.longitude,
                elevation: result.elevation,
            });
        }

        tracing::info!("Successfully fetched {} elevation points", points.len());
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
