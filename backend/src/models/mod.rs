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

        if area_km2 > 20.0 {
            return Err(format!("Area {:.2} km² exceeds maximum of 20 km²", area_km2));
        }

        Ok(area_km2)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GenerateRequest {
    pub bbox: BoundingBox,
    pub resolution: Option<u32>,  // Grid resolution (default: 150x150)
    pub vertical_scale: Option<f32>,  // Exaggeration factor (default: 2.0)
    pub base_height: Option<f32>,  // Base pedestal height in mm (default: 2.0)
    pub include_buildings: Option<bool>,  // Include OSM buildings (default: true)
    pub print_size: Option<f32>,  // Largest XY print dimension in mm (default: 180.0)
}

#[derive(Debug, Serialize)]
pub struct GenerateResponse {
    pub stats: MeshStats,
    pub mesh_data: MeshData,  // The actual 3D mesh data for rendering
}

#[derive(Debug, Serialize)]
pub struct MeshData {
    pub vertices: Vec<[f32; 3]>,         // [x, y, z] positions
    pub triangles: Vec<[usize; 3]>,      // Triangle indices into vertices
    pub terrain_triangle_count: usize,   // triangles[0..this] are terrain surface
    pub building_triangle_count: usize,  // triangles[terrain_tc..terrain_tc+this] are buildings
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

/// Roof geometry classes derived from the OSM `roof:shape` tag.
/// Unrecognised values fall back to `Flat` (the safe, always-watertight cap).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoofShape {
    Flat,
    Gabled,
    Hipped,
    Pyramidal,
    Dome,
    Skillion,
}

impl RoofShape {
    pub fn from_tag(tag: &str) -> Self {
        match tag.trim().to_ascii_lowercase().as_str() {
            "gabled" | "gable" | "gambrel" | "saltbox" | "round" => RoofShape::Gabled,
            "hipped" | "hip" | "half-hipped" | "mansard" => RoofShape::Hipped,
            "pyramidal" | "pyramid" => RoofShape::Pyramidal,
            "dome" | "onion" | "cone" | "conical" => RoofShape::Dome,
            "skillion" | "lean_to" | "shed" | "sloped" => RoofShape::Skillion,
            _ => RoofShape::Flat,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            RoofShape::Flat => "flat",
            RoofShape::Gabled => "gabled",
            RoofShape::Hipped => "hipped",
            RoofShape::Pyramidal => "pyramidal",
            RoofShape::Dome => "dome",
            RoofShape::Skillion => "skillion",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Building {
    pub id: u64,                          // OSM element id (way or relation)
    pub footprint: Vec<(f64, f64)>,       // outer ring, (lon, lat)
    pub holes: Vec<Vec<(f64, f64)>>,      // inner rings (courtyards), (lon, lat)
    pub height: f32,                      // total height in meters, roof included
    pub min_height: f32,                  // bottom of the solid (building:part stacking)
    pub roof_shape: RoofShape,
    pub roof_height: f32,                 // meters of `height` taken by the roof; 0 = auto
    pub is_part: bool,                    // true for building:part elements
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
