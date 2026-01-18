use crate::models::{BoundingBox, ElevationPoint, ProjectedPoint};
use anyhow::{Context, Result};
use proj::Proj;

/// Projects geographic coordinates (lat/lon) to a local UTM coordinate system
/// This ensures accurate metric measurements for 3D printing
pub struct CoordinateProjector {
    proj: Proj,
    origin_x: f64,
    origin_y: f64,
}

impl CoordinateProjector {
    /// Create a new projector for the given bounding box
    /// Automatically determines the appropriate UTM zone
    pub fn new(bbox: &BoundingBox) -> Result<Self> {
        let center_lon = (bbox.min_lon + bbox.max_lon) / 2.0;
        let center_lat = (bbox.min_lat + bbox.max_lat) / 2.0;

        // Determine UTM zone from longitude
        let zone = ((center_lon + 180.0) / 6.0).floor() as i32 + 1;

        // Determine hemisphere
        let hemisphere = if center_lat >= 0.0 { "north" } else { "south" };

        // Create PROJ string for WGS84 -> UTM transformation
        let proj_string = format!(
            "+proj=utm +zone={} +{} +datum=WGS84 +units=m +no_defs",
            zone, hemisphere
        );

        let proj = Proj::new_known_crs("EPSG:4326", &proj_string, None)
            .context("Failed to create coordinate projection")?;

        // Project the bbox center to use as origin
        let (origin_x, origin_y) = proj
            .convert((center_lon, center_lat))
            .context("Failed to project origin")?;

        tracing::info!(
            "Created UTM projection: zone={}, hemisphere={}, origin=({:.2}, {:.2})",
            zone,
            hemisphere,
            origin_x,
            origin_y
        );

        Ok(Self {
            proj,
            origin_x,
            origin_y,
        })
    }

    /// Project a single lat/lon point to local coordinates relative to origin
    pub fn project(&self, lon: f64, lat: f64, elevation: f32) -> Result<ProjectedPoint> {
        let (x, y) = self
            .proj
            .convert((lon, lat))
            .context("Failed to project coordinate")?;

        Ok(ProjectedPoint {
            x: x - self.origin_x,
            y: y - self.origin_y,
            z: elevation,
        })
    }

    /// Project multiple elevation points
    pub fn project_points(&self, points: &[ElevationPoint]) -> Result<Vec<ProjectedPoint>> {
        points
            .iter()
            .map(|p| self.project(p.lon, p.lat, p.elevation))
            .collect()
    }

    /// Get the local coordinate bounds in meters
    pub fn get_local_bounds(&self, bbox: &BoundingBox) -> Result<(f64, f64, f64, f64)> {
        let min_point = self.project(bbox.min_lon, bbox.min_lat, 0.0)?;
        let max_point = self.project(bbox.max_lon, bbox.max_lat, 0.0)?;

        Ok((min_point.x, min_point.y, max_point.x, max_point.y))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_projection() {
        let bbox = BoundingBox {
            min_lat: 40.7,
            min_lon: -74.0,
            max_lat: 40.71,
            max_lon: -73.99,
        };

        let projector = CoordinateProjector::new(&bbox).unwrap();
        let point = projector.project(-74.0, 40.7, 10.0).unwrap();

        // Should be close to origin
        assert!(point.x.abs() < 1000.0);
        assert!(point.y.abs() < 1000.0);
        assert_eq!(point.z, 10.0);
    }
}
