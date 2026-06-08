use crate::models::{BoundingBox, Building};
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;

// Community-run Overpass mirrors — tried first (compact out geom response).
const OVERPASS_ENDPOINTS: &[&str] = &[
    "https://overpass-api.de/api/interpreter",
    "https://overpass.kumi.systems/api/interpreter",
    "https://overpass.private.coffee/api/interpreter",
];

// Buildings below this footprint (m²) are sub-pixel at print scale and skipped.
const MIN_BUILDING_AREA_M2: f64 = 10.0;

// Above this area, buildings would be < 1 mm on a 100 mm print — skip the query.
pub const MAX_AREA_FOR_BUILDINGS_KM2: f64 = 5.0;

pub struct OsmService {
    client: reqwest::Client,
}

impl OsmService {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .user_agent("geoprint3d-backend/0.1.0")
            .build()
            .unwrap_or_default();
        Self { client }
    }

    /// Fetch building footprints and heights from OSM.
    ///
    /// Strategy (each step is tried only if the previous one fails):
    ///   1. Overpass mirrors 1-3 — compact `out geom` query, no recurse step.
    ///   2. OSM direct API    — `api.openstreetmap.org/api/0.6/map.json`, OSMF-maintained,
    ///                          much more available than community Overpass instances.
    pub async fn fetch_buildings(&self, bbox: &BoundingBox) -> Result<Vec<Building>> {
        tracing::info!("   ┌─ OpenStreetMap Building Service");
        tracing::info!(
            "   │  Bbox: ({:.4}, {:.4}) → ({:.4}, {:.4})",
            bbox.min_lat, bbox.min_lon, bbox.max_lat, bbox.max_lon
        );

        let request_start = std::time::Instant::now();

        // ── Step 1: Overpass mirrors ──────────────────────────────────────────
        // `out geom qt` returns coordinates inline — ~10× smaller than
        // `out body;>;out skel qt` because no separate node objects are emitted.
        let overpass_query = format!(
            "[out:json][timeout:20];way[\"building\"][\"building\"!=\"no\"]({},{},{},{});out geom qt;",
            bbox.min_lat, bbox.min_lon, bbox.max_lat, bbox.max_lon
        );

        for (i, &endpoint) in OVERPASS_ENDPOINTS.iter().enumerate() {
            tracing::info!("   │  Overpass attempt {}/{}: {}", i + 1, OVERPASS_ENDPOINTS.len(), endpoint);

            let resp = match self
                .client
                .post(endpoint)
                .form(&[("data", &overpass_query)])
                .timeout(Duration::from_secs(25))
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("   │  Connection failed: {}", e);
                    continue;
                }
            };

            let status = resp.status();
            tracing::info!("   │  Status: {} ({:.2}s)", status, request_start.elapsed().as_secs_f64());

            match status.as_u16() {
                200..=299 => {
                    let data: Value = resp.json().await.context("Failed to parse Overpass JSON")?;
                    let elapsed = request_start.elapsed();
                    tracing::info!("   │  Source: Overpass (out geom)");
                    return self.parse_geom_response(data, elapsed);
                }
                429 | 406 => {
                    tracing::warn!("   │  Rate-limited / rejected — trying next mirror");
                    continue;
                }
                _ => {
                    let body = resp.text().await.unwrap_or_default();
                    tracing::warn!("   │  Error {}: {}", status, body.chars().take(120).collect::<String>());
                    continue;
                }
            }
        }

        // ── Step 2: OSM direct API (fallback) ────────────────────────────────
        // Returns all OSM elements in the bbox as JSON. More available than
        // Overpass because it is the primary OSMF-maintained endpoint.
        // Note: bbox order is west,south,east,north (lon,lat,lon,lat).
        tracing::info!("   │  All Overpass mirrors failed — falling back to OSM direct API");
        self.fetch_buildings_osm_api(bbox, request_start).await
    }

    async fn fetch_buildings_osm_api(
        &self,
        bbox: &BoundingBox,
        request_start: std::time::Instant,
    ) -> Result<Vec<Building>> {
        let url = format!(
            "https://api.openstreetmap.org/api/0.6/map.json?bbox={},{},{},{}",
            bbox.min_lon, bbox.min_lat, bbox.max_lon, bbox.max_lat
        );
        tracing::info!("   │  OSM API: {}", url);

        let resp = self
            .client
            .get(&url)
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .context("Failed to connect to OSM direct API")?;

        let status = resp.status();
        tracing::info!("   │  Status: {} ({:.2}s)", status, request_start.elapsed().as_secs_f64());

        if status.as_u16() == 509 {
            anyhow::bail!("OSM API: bounding box too large (> 50 000 nodes) — area exceeds service limit");
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("OSM API error {}: {}", status, body.chars().take(200).collect::<String>());
        }

        let data: Value = resp.json().await.context("Failed to parse OSM API JSON")?;
        let elapsed = request_start.elapsed();
        tracing::info!("   │  Source: OSM direct API");
        self.parse_noderefs_response(data, elapsed)
    }

    // ── Parsers ───────────────────────────────────────────────────────────────

    /// Parse an Overpass `out geom` response.
    /// Each way carries a `geometry` array with inline lat/lon objects.
    fn parse_geom_response(&self, data: Value, elapsed: Duration) -> Result<Vec<Building>> {
        let elements = data["elements"]
            .as_array()
            .context("Missing 'elements' in Overpass response")?;

        tracing::info!("   │  Received {} way elements", elements.len());
        let mut buildings = Vec::new();
        let (mut tag, mut levels, mut default, mut skipped) = (0, 0, 0, 0);

        for el in elements.iter() {
            let tags = &el["tags"];
            let height = parse_height(tags, &mut tag, &mut levels, &mut default);

            let Some(geometry) = el["geometry"].as_array() else {
                skipped += 1;
                continue;
            };

            let footprint: Vec<(f64, f64)> = geometry
                .iter()
                .filter_map(|g| Some((g["lon"].as_f64()?, g["lat"].as_f64()?)))
                .collect();

            if footprint.len() >= 3 && footprint_area_m2(&footprint) >= MIN_BUILDING_AREA_M2 {
                buildings.push(Building { footprint, height });
            } else {
                skipped += 1;
            }
        }

        log_building_stats(&buildings, tag, levels, default, skipped, elapsed);
        Ok(buildings)
    }

    /// Parse an OSM direct API response (node-ref format).
    /// Ways reference node IDs; coordinates are looked up from the node list.
    fn parse_noderefs_response(&self, data: Value, elapsed: Duration) -> Result<Vec<Building>> {
        let elements = data["elements"]
            .as_array()
            .context("Missing 'elements' in OSM API response")?;

        tracing::info!("   │  Received {} total OSM elements", elements.len());

        // Build node-id → (lon, lat) map
        let mut nodes: HashMap<u64, (f64, f64)> = HashMap::new();
        for el in elements.iter() {
            if el["type"] == "node" {
                if let (Some(id), Some(lat), Some(lon)) =
                    (el["id"].as_u64(), el["lat"].as_f64(), el["lon"].as_f64())
                {
                    nodes.insert(id, (lon, lat));
                }
            }
        }

        let mut buildings = Vec::new();
        let (mut tag, mut levels, mut default, mut skipped) = (0, 0, 0, 0);

        for el in elements.iter() {
            if el["type"] != "way" {
                continue;
            }
            let tags = &el["tags"];
            if !tags["building"].is_string() || tags["building"] == "no" {
                continue;
            }

            let height = parse_height(tags, &mut tag, &mut levels, &mut default);

            let Some(node_refs) = el["nodes"].as_array() else {
                skipped += 1;
                continue;
            };

            let footprint: Vec<(f64, f64)> = node_refs
                .iter()
                .filter_map(|r| r.as_u64())
                .filter_map(|id| nodes.get(&id).copied())
                .collect();

            if footprint.len() >= 3 && footprint_area_m2(&footprint) >= MIN_BUILDING_AREA_M2 {
                buildings.push(Building { footprint, height });
            } else {
                skipped += 1;
            }
        }

        log_building_stats(&buildings, tag, levels, default, skipped, elapsed);
        Ok(buildings)
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn parse_height(tags: &Value, tag: &mut usize, levels: &mut usize, default: &mut usize) -> f32 {
    if let Some(h) = tags["height"].as_str() {
        *tag += 1;
        h.trim_end_matches(|c: char| c == 'm' || c == ' ')
            .parse::<f32>()
            .unwrap_or(10.0)
    } else if let Some(l) = tags["building:levels"].as_str() {
        *levels += 1;
        l.parse::<f32>().unwrap_or(3.0) * 3.5
    } else {
        *default += 1;
        10.0
    }
}

fn log_building_stats(
    buildings: &[Building],
    tag: usize,
    levels: usize,
    default: usize,
    skipped: usize,
    elapsed: Duration,
) {
    if buildings.is_empty() {
        tracing::info!("   │  ⚠ No printable buildings found");
    } else {
        let heights: Vec<f32> = buildings.iter().map(|b| b.height).collect();
        let min_h = heights.iter().cloned().fold(f32::INFINITY, f32::min);
        let max_h = heights.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        let avg_h: f32 = heights.iter().sum::<f32>() / heights.len() as f32;
        tracing::info!("   │  ✓ {} printable buildings", buildings.len());
        tracing::info!("   │  Height sources: tag={} levels={} default={}", tag, levels, default);
        tracing::info!("   │  Heights: min={:.0}m max={:.0}m avg={:.0}m", min_h, max_h, avg_h);
    }
    if skipped > 0 {
        tracing::debug!("   │  Skipped {} elements (no geometry / too small)", skipped);
    }
    tracing::info!("   └─ OSM processing time: {:.2}s", elapsed.as_secs_f64());
}

/// Shoelace formula for polygon area in m².
/// Accurate enough for filtering; not for precise measurements.
fn footprint_area_m2(pts: &[(f64, f64)]) -> f64 {
    let n = pts.len();
    if n < 3 {
        return 0.0;
    }
    let mut area = 0.0_f64;
    for i in 0..n {
        let (x0, y0) = pts[i];
        let (x1, y1) = pts[(i + 1) % n];
        area += x0 * y1 - x1 * y0;
    }
    (area.abs() / 2.0) * 111_320.0 * 111_320.0
}

impl Default for OsmService {
    fn default() -> Self {
        Self::new()
    }
}
