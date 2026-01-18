use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BoundingBox {
    pub min_lat: f64,
    pub min_lon: f64,
    pub max_lat: f64,
    pub max_lon: f64,
}

impl BoundingBox {
    /// Validates that the bounding box is <= 1km²
    /// Returns the area in km²
    pub fn validate_area(&self) -> Result<f64, String> {
        use geo::{Area, Polygon, Coord};

        // Create polygon from bbox (approximate, should use proper projection)
        let coords = vec![
            Coord { x: self.min_lon, y: self.min_lat },
            Coord { x: self.max_lon, y: self.min_lat },
            Coord { x: self.max_lon, y: self.max_lat },
            Coord { x: self.min_lon, y: self.max_lat },
            Coord { x: self.min_lon, y: self.min_lat },
        ];

        let poly = Polygon::new(coords.into(), vec![]);

        // Rough approximation: convert degrees to km at equator
        // For production, use proper geodesic calculations
        let lat_km = (self.max_lat - self.min_lat) * 111.0;
        let lon_km = (self.max_lon - self.min_lon) * 111.0 * self.min_lat.to_radians().cos();
        let area_km2 = lat_km * lon_km;

        if area_km2 > 1.0 {
            return Err(format!("Area {:.2} km² exceeds maximum of 1 km²", area_km2));
        }

        Ok(area_km2)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GenerateRequest {
    pub bbox: BoundingBox,
    pub resolution: Option<u32>,  // Grid resolution (default: 100x100)
    pub vertical_scale: Option<f32>,  // Exaggeration factor (default: 1.5)
    pub base_height: Option<f32>,  // Base pedestal height in mm (default: 2.0)
    pub include_buildings: Option<bool>,  // Include OSM buildings (default: true)
}

#[derive(Debug, Serialize)]
pub struct GenerateResponse {
    pub preview_url: String,  // URL to low-poly GLB preview
    pub stl_url: String,      // URL to full-resolution STL
    pub stats: MeshStats,
}

#[derive(Debug, Serialize)]
pub struct MeshStats {
    pub vertices: usize,
    pub triangles: usize,
    pub area_km2: f64,
    pub min_elevation: f32,
    pub max_elevation: f32,
    pub buildings_count: usize,
}

#[derive(Debug, Clone)]
pub struct ElevationPoint {
    pub lat: f64,
    pub lon: f64,
    pub elevation: f32,  // in meters
}

#[derive(Debug, Clone)]
pub struct Building {
    pub footprint: Vec<(f64, f64)>,  // (lon, lat) coordinates
    pub height: f32,  // in meters
}

#[derive(Debug)]
pub struct TerrainMesh {
    pub vertices: Vec<[f32; 3]>,  // (x, y, z) in meters
    pub triangles: Vec<[usize; 3]>,  // indices into vertices
}

#[derive(Debug)]
pub struct ProjectedPoint {
    pub x: f64,  // meters in UTM
    pub y: f64,  // meters in UTM
    pub z: f32,  // elevation in meters
}
