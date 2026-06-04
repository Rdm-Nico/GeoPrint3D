use crate::models::{BoundingBox, Building};
use anyhow::{Context, Result};
use serde_json::Value;

pub struct OsmService {
    client: reqwest::Client,
    overpass_url: String,
}

impl OsmService {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            overpass_url: "https://overpass-api.de/api/interpreter".to_string(),
        }
    }

    /// Fetch building footprints and heights from OpenStreetMap
    pub async fn fetch_buildings(&self, bbox: &BoundingBox) -> Result<Vec<Building>> {
        tracing::info!("   ┌─ OpenStreetMap Service");
        tracing::info!("   │  API: Overpass API (OSM query endpoint)");
        tracing::info!("   │  URL: {}", self.overpass_url);

        let query = format!(
            r#"
            [out:json][timeout:25];
            (
              way["building"]({},{},{},{});
              relation["building"]({},{},{},{});
            );
            out body;
            >;
            out skel qt;
            "#,
            bbox.min_lat, bbox.min_lon, bbox.max_lat, bbox.max_lon,
            bbox.min_lat, bbox.min_lon, bbox.max_lat, bbox.max_lon
        );

        tracing::debug!("   │  Overpass Query: way[\"building\"] and relation[\"building\"]");
        tracing::info!("   │  Querying buildings in bbox: ({:.4}, {:.4}) to ({:.4}, {:.4})",
            bbox.min_lat, bbox.min_lon, bbox.max_lat, bbox.max_lon);

        let request_start = std::time::Instant::now();

        let response = self
            .client
            .post(&self.overpass_url)
            .body(query)
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await
            .context("Failed to connect to Overpass API - server may be overloaded")?;

        let status = response.status();
        let request_time = request_start.elapsed();

        tracing::info!("   │  Response status: {} (took {:.2}s)", status, request_time.as_secs_f64());

        if !status.is_success() {
            let error_body = response.text().await.unwrap_or_else(|_| "Unable to read response".to_string());
            tracing::error!("   │  Overpass API Error: {}", error_body);
            anyhow::bail!("Overpass API returned error status {}", status);
        }

        let data: Value = response
            .json()
            .await
            .context("Failed to parse OSM JSON response")?;

        let mut buildings = Vec::new();
        let elements = data["elements"].as_array().context("Invalid OSM response: missing 'elements' array")?;

        tracing::info!("   │  Received {} OSM elements to process", elements.len());

        // Create a map of node IDs to coordinates
        let mut nodes = std::collections::HashMap::new();
        let mut node_count = 0;
        let mut way_count = 0;

        for element in elements {
            if element["type"] == "node" {
                if let (Some(id), Some(lat), Some(lon)) = (
                    element["id"].as_u64(),
                    element["lat"].as_f64(),
                    element["lon"].as_f64()
                ) {
                    nodes.insert(id, (lon, lat));
                    node_count += 1;
                }
            }
        }

        tracing::debug!("   │  Indexed {} coordinate nodes", node_count);

        // Process ways (buildings)
        let mut height_from_tag = 0;
        let mut height_from_levels = 0;
        let mut height_default = 0;
        let mut skipped_invalid = 0;

        for element in elements {
            if element["type"] == "way" && element["tags"]["building"].is_string() {
                way_count += 1;
                let tags = &element["tags"];

                // Get building height (default to 10m if not specified)
                let height = if let Some(h) = tags["height"].as_str() {
                    height_from_tag += 1;
                    h.trim_end_matches('m').parse::<f32>().unwrap_or(10.0)
                } else if let Some(levels) = tags["building:levels"].as_str() {
                    height_from_levels += 1;
                    levels.parse::<f32>().unwrap_or(3.0) * 3.5 // Assume 3.5m per level
                } else {
                    height_default += 1;
                    10.0 // Default height
                };

                // Get footprint coordinates
                if let Some(node_refs) = element["nodes"].as_array() {
                    let mut footprint = Vec::new();
                    for node_ref in node_refs {
                        if let Some(node_id) = node_ref.as_u64() {
                            if let Some(&coords) = nodes.get(&node_id) {
                                footprint.push(coords);
                            }
                        }
                    }

                    if footprint.len() >= 3 {
                        buildings.push(Building {
                            footprint,
                            height,
                        });
                    } else {
                        skipped_invalid += 1;
                    }
                }
            }
        }

        if buildings.is_empty() {
            tracing::info!("   │  ⚠ No valid buildings found in this area");
        } else {
            // extract heights
            let heights: Vec<f32> = buildings.iter().map(|b| b.height).collect();
            // find min and max heights of the buildings by confront each elements in a secure way 
            let min_h = heights.iter().cloned().min_by(|a, b| a.partial_cmp(b).unwrap()).unwrap_or(0.0);
            let max_h = heights.iter().cloned().max_by(|a, b| a.partial_cmp(b).unwrap()).unwrap_or(0.0);
            let avg_h: f32 = heights.iter().sum::<f32>() / heights.len() as f32;

            tracing::info!("   │  ✓ Found {} valid buildings (from {} ways)", buildings.len(), way_count);
            tracing::info!("   │  Height sources:");
            tracing::info!("   │    ├─ From 'height' tag:        {} buildings", height_from_tag);
            tracing::info!("   │    ├─ From 'building:levels':   {} buildings", height_from_levels);
            tracing::info!("   │    └─ Default (10m):            {} buildings", height_default);
            tracing::info!("   │  Height stats: min={:.1}m, max={:.1}m, avg={:.1}m", min_h, max_h, avg_h);

            if skipped_invalid > 0 {
                tracing::debug!("   │  Skipped {} invalid buildings (< 3 vertices)", skipped_invalid);
            }
        }

        tracing::info!("   └─ Total OSM processing time: {:.2}s", request_time.as_secs_f64());

        Ok(buildings)
    }
}

impl Default for OsmService {
    fn default() -> Self {
        Self::new()
    }
}
