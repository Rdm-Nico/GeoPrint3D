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

        let response = self
            .client
            .post(&self.overpass_url)
            .body(query)
            .send()
            .await
            .context("Failed to fetch OSM data")?;

        let data: Value = response
            .json()
            .await
            .context("Failed to parse OSM response")?;

        let mut buildings = Vec::new();
        let elements = data["elements"].as_array().context("Invalid OSM response")?;

        // Create a map of node IDs to coordinates
        let mut nodes = std::collections::HashMap::new();
        for element in elements {
            if element["type"] == "node" {
                let id = element["id"].as_u64().unwrap();
                let lat = element["lat"].as_f64().unwrap();
                let lon = element["lon"].as_f64().unwrap();
                nodes.insert(id, (lon, lat));
            }
        }

        // Process ways (buildings)
        for element in elements {
            if element["type"] == "way" && element["tags"]["building"].is_string() {
                let tags = &element["tags"];

                // Get building height (default to 10m if not specified)
                let height = if let Some(h) = tags["height"].as_str() {
                    h.trim_end_matches('m').parse::<f32>().unwrap_or(10.0)
                } else if let Some(levels) = tags["building:levels"].as_str() {
                    levels.parse::<f32>().unwrap_or(3.0) * 3.5 // Assume 3.5m per level
                } else {
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
                    }
                }
            }
        }

        tracing::info!("Fetched {} buildings from OSM", buildings.len());
        Ok(buildings)
    }
}

impl Default for OsmService {
    fn default() -> Self {
        Self::new()
    }
}
