use crate::models::{BoundingBox, ElevationPoint};
use anyhow::{Context, Result};
use serde::Deserialize;

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
    /// Mapbox access token. When present, high-resolution Terrain-RGB tiles are
    /// used; otherwise the service falls back to the free Open-Elevation API.
    mapbox_token: Option<String>,
}

impl ElevationService {
    pub fn new(mapbox_token: Option<String>) -> Self {
        match &mapbox_token {
            Some(t) if !t.trim().is_empty() => {
                tracing::info!("ElevationService: Mapbox Terrain-RGB enabled (token …{})",
                    &t[t.len().saturating_sub(4)..]);
            }
            _ => tracing::warn!(
                "ElevationService: MAPBOX_TOKEN not set — using Open-Elevation fallback (coarse SRTM)"
            ),
        }
        Self {
            client: reqwest::Client::new(),
            mapbox_token: mapbox_token.filter(|t| !t.trim().is_empty()),
        }
    }

    /// Fetch an elevation grid for a bounding box.
    ///
    /// Prefers Mapbox Terrain-RGB (high resolution); on missing token or any
    /// Mapbox failure, transparently falls back to the Open-Elevation API so the
    /// app keeps working without a token.
    pub async fn fetch_elevation_grid(
        &self,
        bbox: &BoundingBox,
        resolution: u32,
    ) -> Result<Vec<ElevationPoint>> {
        if let Some(token) = &self.mapbox_token {
            match super::elevation_mapbox::fetch_mapbox_grid(&self.client, token, bbox, resolution)
                .await
            {
                Ok(points) => return Ok(points),
                Err(e) => {
                    tracing::warn!(
                        "   ⚠ Mapbox elevation failed ({}) — falling back to Open-Elevation",
                        e
                    );
                }
            }
        }
        self.fetch_open_elevation_grid(bbox, resolution).await
    }

    /// Fetch elevation data from the free Open-Elevation API (coarse SRTM).
    async fn fetch_open_elevation_grid(
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

        // Open-Elevation rejects very large POST bodies (HTTP 413). Split the grid
        // into batches that stay well under the limit; results keep input order
        // within each batch, and batches are processed sequentially, so the final
        // ordering matches the row-major grid the rest of the pipeline expects.
        const BATCH_SIZE: usize = 8000;
        let total = locations.len();
        tracing::info!(
            "   │  Sending {} location points in {} batch(es) of ≤{}...",
            total,
            (total + BATCH_SIZE - 1) / BATCH_SIZE,
            BATCH_SIZE
        );
        let request_start = std::time::Instant::now();

        const MAX_RETRIES: u32 = 3;
        for (b, chunk) in locations.chunks(BATCH_SIZE).enumerate() {
            let mut attempt = 0;
            let data = loop {
                attempt += 1;
                let result = self
                    .client
                    .post(api_url)
                    .json(&serde_json::json!({ "locations": chunk }))
                    .timeout(std::time::Duration::from_secs(60))
                    .send()
                    .await;

                match result {
                    Ok(resp) if resp.status().is_success() => {
                        break resp
                            .json::<OpenTopoResponse>()
                            .await
                            .context("Failed to parse elevation JSON response")?;
                    }
                    // Transient server-side failures (429/5xx) — retry with backoff.
                    Ok(resp) if resp.status().as_u16() == 429 || resp.status().is_server_error() => {
                        let status = resp.status();
                        if attempt >= MAX_RETRIES {
                            let body = resp.text().await.unwrap_or_default();
                            anyhow::bail!("Open-Elevation API error {} after {} attempts: {}", status, attempt, body.chars().take(160).collect::<String>());
                        }
                        tracing::warn!("   │  Batch {} got {} (attempt {}/{}), retrying…", b + 1, status, attempt, MAX_RETRIES);
                        tokio::time::sleep(std::time::Duration::from_secs(2 * attempt as u64)).await;
                    }
                    Ok(resp) => {
                        let status = resp.status();
                        let body = resp.text().await.unwrap_or_default();
                        anyhow::bail!("Open-Elevation API returned status {}: {}", status, body.chars().take(160).collect::<String>());
                    }
                    Err(e) if attempt < MAX_RETRIES => {
                        tracing::warn!("   │  Batch {} connection error (attempt {}/{}): {} — retrying…", b + 1, attempt, MAX_RETRIES, e);
                        tokio::time::sleep(std::time::Duration::from_secs(2 * attempt as u64)).await;
                    }
                    Err(e) => return Err(anyhow::Error::new(e).context("Failed to connect to Open-Elevation API")),
                }
            };

            for result in data.results {
                points.push(ElevationPoint {
                    lat: result.latitude,
                    lon: result.longitude,
                    elevation: result.elevation,
                });
            }
            tracing::debug!("   │  Batch {} ok ({} points so far)", b + 1, points.len());
        }

        let request_time = request_start.elapsed();
        if points.is_empty() {
            tracing::warn!("   │  ⚠ API returned no elevation points");
            anyhow::bail!("Open-Elevation API returned no elevation data");
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
}

/// Light separable [1,2,1] smoothing over a row-major elevation grid.
///
/// Removes DEM quantisation terracing (integer-metre stair-steps from coarse
/// sources) without flattening genuine relief. `width`/`height` are the grid
/// dimensions (resolution+1 each). `passes` controls strength (1 = gentle).
pub fn smooth_elevation_grid(points: &mut [ElevationPoint], width: usize, height: usize, passes: u32) {
    if width < 3 || height < 3 || points.len() != width * height {
        return;
    }
    let mut buf: Vec<f32> = points.iter().map(|p| p.elevation).collect();
    let mut tmp = buf.clone();

    for _ in 0..passes {
        // Horizontal pass
        for y in 0..height {
            for x in 0..width {
                let c = buf[y * width + x];
                let l = if x > 0 { buf[y * width + x - 1] } else { c };
                let r = if x + 1 < width { buf[y * width + x + 1] } else { c };
                tmp[y * width + x] = (l + 2.0 * c + r) * 0.25;
            }
        }
        // Vertical pass
        for y in 0..height {
            for x in 0..width {
                let c = tmp[y * width + x];
                let u = if y > 0 { tmp[(y - 1) * width + x] } else { c };
                let d = if y + 1 < height { tmp[(y + 1) * width + x] } else { c };
                buf[y * width + x] = (u + 2.0 * c + d) * 0.25;
            }
        }
    }

    for (p, &e) in points.iter_mut().zip(buf.iter()) {
        p.elevation = e;
    }
}

impl Default for ElevationService {
    fn default() -> Self {
        Self::new(None)
    }
}
