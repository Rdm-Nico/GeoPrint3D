//! High-resolution elevation sampling from Mapbox Terrain-RGB tiles.
//!
//! Strategy:
//!   1. Pick the highest web-mercator zoom (≤ MAX_ZOOM) whose tile coverage of the
//!      bbox stays within MAX_TILES — higher zoom = finer DEM, bounded fetch cost.
//!   2. Fetch the covering tiles in parallel as `@2x.pngraw` (512px) PNGs.
//!   3. Decode each pixel to metres: elev = -10000 + (R*65536 + G*256 + B) * 0.1.
//!   4. Stitch into one raster and bilinearly sample the (res+1)² lat/lon grid.
//!
//! Returns the same `Vec<ElevationPoint>` (row-major, lat-outer/lon-inner) that the
//! Open-Elevation path produces, so it is a drop-in replacement downstream.

use crate::models::{BoundingBox, ElevationPoint};
use anyhow::{Context, Result};
use std::f64::consts::PI;

/// Terrain tileset. `mapbox.terrain-rgb` serves native tiles up to zoom 15.
/// (The newer `mapbox.mapbox-terrain-dem-v1` only goes to zoom 14 on the /v4/
/// raster endpoint — requesting z15 there returns 404 — so it would need
/// MAX_ZOOM = 14 if swapped in.)
const TILESET: &str = "mapbox.terrain-rgb";
/// Highest native zoom for TILESET; the /v4/ API does not over-zoom (404 above).
const MAX_ZOOM: u32 = 15;
/// Hard cap on tiles fetched per request (memory + rate-limit guard).
const MAX_TILES: u32 = 24;
/// `@2x` tiles are 512×512.
const TILE_PX: usize = 512;

/// A decoded raster stitched from one or more tiles, with the geographic
/// transform needed to map lon/lat → pixel coordinates.
struct DemRaster {
    elev: Vec<f32>, // row-major, height_px rows × width_px cols
    width_px: usize,
    height_px: usize,
    zoom: u32,
    tile_x_min: u32,
    tile_y_min: u32,
}

/// Fetch a Mapbox terrain grid for the bbox at the given sampling resolution.
pub async fn fetch_mapbox_grid(
    client: &reqwest::Client,
    token: &str,
    bbox: &BoundingBox,
    resolution: u32,
) -> Result<Vec<ElevationPoint>> {
    let center_lat = (bbox.min_lat + bbox.max_lat) / 2.0;

    tracing::info!("   ┌─ Elevation Service (Mapbox Terrain-RGB)");
    tracing::info!("   │  Dataset: {} (@2x .pngraw)", TILESET);

    let zoom = pick_zoom(bbox);
    let raster = fetch_dem_raster(client, token, bbox, zoom).await?;

    let mpp = ground_resolution_m(center_lat, zoom) / 2.0; // /2 for @2x tiles
    tracing::info!(
        "   │  Zoom {} → {}×{} tiles, {}×{} px raster (~{:.1} m/px at center)",
        zoom,
        raster.width_px / TILE_PX,
        raster.height_px / TILE_PX,
        raster.width_px,
        raster.height_px,
        mpp
    );

    // Sample the (res+1)² grid, row-major: i over latitude, j over longitude.
    // Matches the Open-Elevation ordering so projection/meshing are unchanged.
    let lat_step = (bbox.max_lat - bbox.min_lat) / resolution as f64;
    let lon_step = (bbox.max_lon - bbox.min_lon) / resolution as f64;

    let mut points = Vec::with_capacity(((resolution + 1) * (resolution + 1)) as usize);
    for i in 0..=resolution {
        for j in 0..=resolution {
            let lat = bbox.min_lat + i as f64 * lat_step;
            let lon = bbox.min_lon + j as f64 * lon_step;
            let elevation = raster.sample(lon, lat);
            points.push(ElevationPoint { lat, lon, elevation });
        }
    }

    let (min_e, max_e, avg_e) = stats(&points);
    tracing::info!("   │  ✓ Sampled {} grid points", points.len());
    tracing::info!("   │  Elevation stats: min={:.1}m, max={:.1}m, avg={:.1}m", min_e, max_e, avg_e);
    tracing::info!("   └─ Source: Mapbox {}", TILESET);

    Ok(points)
}

/// Choose the highest zoom (≤ MAX_ZOOM) whose tile span stays within MAX_TILES.
/// Smaller areas get full z15 detail; large areas step down to bound fetch cost.
fn pick_zoom(bbox: &BoundingBox) -> u32 {
    for z in (1..=MAX_ZOOM).rev() {
        let (x0, y0, x1, y1) = tile_span(bbox, z);
        let tiles = (x1 - x0 + 1) * (y1 - y0 + 1);
        if tiles <= MAX_TILES {
            return z;
        }
    }
    1
}

/// Integer tile range (inclusive) covering the bbox at zoom `z`.
fn tile_span(bbox: &BoundingBox, z: u32) -> (u32, u32, u32, u32) {
    let n = 2f64.powi(z as i32);
    let x0 = lon_to_tile_x(bbox.min_lon, z).floor().clamp(0.0, n - 1.0) as u32;
    let x1 = lon_to_tile_x(bbox.max_lon, z).floor().clamp(0.0, n - 1.0) as u32;
    // Note: tile-y grows southward, so max_lat → smaller y.
    let y0 = lat_to_tile_y(bbox.max_lat, z).floor().clamp(0.0, n - 1.0) as u32;
    let y1 = lat_to_tile_y(bbox.min_lat, z).floor().clamp(0.0, n - 1.0) as u32;
    (x0, y0, x1, y1)
}

/// Fetch and stitch all covering tiles into a single raster.
async fn fetch_dem_raster(
    client: &reqwest::Client,
    token: &str,
    bbox: &BoundingBox,
    zoom: u32,
) -> Result<DemRaster> {
    let (x0, y0, x1, y1) = tile_span(bbox, zoom);
    let cols = (x1 - x0 + 1) as usize;
    let rows = (y1 - y0 + 1) as usize;
    let width_px = cols * TILE_PX;
    let height_px = rows * TILE_PX;

    // Fetch every tile concurrently.
    let mut futures = Vec::new();
    for ty in y0..=y1 {
        for tx in x0..=x1 {
            futures.push(fetch_tile(client, token, zoom, tx, ty));
        }
    }
    let results = futures::future::join_all(futures).await;

    let mut elev = vec![f32::NAN; width_px * height_px];
    let mut placed = 0;
    let mut idx = 0;
    for ty in y0..=y1 {
        for tx in x0..=x1 {
            let tile = results[idx].as_ref().map_err(|e| {
                anyhow::anyhow!("Mapbox tile {}/{}/{} failed: {}", zoom, tx, ty, e)
            })?;
            let off_x = (tx - x0) as usize * TILE_PX;
            let off_y = (ty - y0) as usize * TILE_PX;
            for py in 0..TILE_PX {
                let dst = (off_y + py) * width_px + off_x;
                let src = py * TILE_PX;
                elev[dst..dst + TILE_PX].copy_from_slice(&tile[src..src + TILE_PX]);
            }
            placed += 1;
            idx += 1;
        }
    }
    tracing::info!("   │  Fetched {} tiles", placed);

    Ok(DemRaster {
        elev,
        width_px,
        height_px,
        zoom,
        tile_x_min: x0,
        tile_y_min: y0,
    })
}

/// Fetch one tile and decode it to a TILE_PX×TILE_PX elevation buffer (metres).
async fn fetch_tile(
    client: &reqwest::Client,
    token: &str,
    z: u32,
    x: u32,
    y: u32,
) -> Result<Vec<f32>> {
    let url = format!(
        "https://api.mapbox.com/v4/{}/{}/{}/{}@2x.pngraw?access_token={}",
        TILESET, z, x, y, token
    );
    let resp = client
        .get(&url)
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .context("tile request failed")?;
    if !resp.status().is_success() {
        anyhow::bail!("HTTP {}", resp.status());
    }
    let bytes = resp.bytes().await.context("reading tile bytes")?;
    decode_terrain_png(&bytes)
}

/// Decode a Mapbox Terrain-RGB PNG into per-pixel elevation in metres.
fn decode_terrain_png(bytes: &[u8]) -> Result<Vec<f32>> {
    let img = image::load_from_memory_with_format(bytes, image::ImageFormat::Png)
        .context("decoding terrain PNG")?
        .to_rgb8();
    if img.width() as usize != TILE_PX || img.height() as usize != TILE_PX {
        anyhow::bail!("unexpected tile size {}×{}", img.width(), img.height());
    }
    let mut out = Vec::with_capacity(TILE_PX * TILE_PX);
    for px in img.pixels() {
        out.push(rgb_to_elevation(px.0[0], px.0[1], px.0[2]));
    }
    Ok(out)
}

/// Mapbox Terrain-RGB decode: height = -10000 + (R*65536 + G*256 + B) * 0.1
#[inline]
fn rgb_to_elevation(r: u8, g: u8, b: u8) -> f32 {
    -10000.0 + (r as f32 * 65536.0 + g as f32 * 256.0 + b as f32) * 0.1
}

impl DemRaster {
    /// Bilinearly sample elevation at a geographic coordinate.
    fn sample(&self, lon: f64, lat: f64) -> f32 {
        let fx = lon_to_tile_x(lon, self.zoom) - self.tile_x_min as f64;
        let fy = lat_to_tile_y(lat, self.zoom) - self.tile_y_min as f64;
        let px = (fx * TILE_PX as f64).clamp(0.0, self.width_px as f64 - 1.0);
        let py = (fy * TILE_PX as f64).clamp(0.0, self.height_px as f64 - 1.0);

        let x0 = px.floor() as usize;
        let y0 = py.floor() as usize;
        let x1 = (x0 + 1).min(self.width_px - 1);
        let y1 = (y0 + 1).min(self.height_px - 1);
        let tx = (px - x0 as f64) as f32;
        let ty = (py - y0 as f64) as f32;

        let v00 = self.elev[y0 * self.width_px + x0];
        let v10 = self.elev[y0 * self.width_px + x1];
        let v01 = self.elev[y1 * self.width_px + x0];
        let v11 = self.elev[y1 * self.width_px + x1];

        let top = v00 + (v10 - v00) * tx;
        let bot = v01 + (v11 - v01) * tx;
        top + (bot - top) * ty
    }
}

// ── Web-mercator / slippy-tile helpers ──────────────────────────────────────

fn lon_to_tile_x(lon: f64, z: u32) -> f64 {
    (lon + 180.0) / 360.0 * 2f64.powi(z as i32)
}

fn lat_to_tile_y(lat: f64, z: u32) -> f64 {
    let lat_rad = lat.to_radians();
    (1.0 - (lat_rad.tan() + 1.0 / lat_rad.cos()).ln() / PI) / 2.0 * 2f64.powi(z as i32)
}

/// Ground resolution (m/px) for 256px tiles at the given latitude and zoom.
fn ground_resolution_m(lat: f64, z: u32) -> f64 {
    156_543.03 * lat.to_radians().cos() / 2f64.powi(z as i32)
}

fn stats(points: &[ElevationPoint]) -> (f32, f32, f32) {
    let mut min = f32::INFINITY;
    let mut max = f32::NEG_INFINITY;
    let mut sum = 0.0f32;
    for p in points {
        min = min.min(p.elevation);
        max = max.max(p.elevation);
        sum += p.elevation;
    }
    (min, max, sum / points.len().max(1) as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_matches_mapbox_formula() {
        // Mapbox reference: RGB(1,134,160) decodes to exactly 0 m (sea level).
        let zero = rgb_to_elevation(1, 134, 160);
        assert!(zero.abs() < 0.001, "got {zero}");
        // RGB(1,138,136) → -10000 + (65536+35328+136)*0.1 = 100.0 m
        let hundred = rgb_to_elevation(1, 138, 136);
        assert!((hundred - 100.0).abs() < 0.001, "got {hundred}");
        // Minimum encodable value RGB(0,0,0) → -10000 m.
        let floor = rgb_to_elevation(0, 0, 0);
        assert!((floor + 10000.0).abs() < 0.001, "got {floor}");
    }

    #[test]
    fn zoom_stays_within_tile_budget() {
        let bbox = BoundingBox {
            min_lat: 44.46,
            min_lon: 11.00,
            max_lat: 44.47,
            max_lon: 11.02,
        };
        let z = pick_zoom(&bbox);
        let (x0, y0, x1, y1) = tile_span(&bbox, z);
        assert!((x1 - x0 + 1) * (y1 - y0 + 1) <= MAX_TILES);
        assert!(z <= MAX_ZOOM);
    }
}
