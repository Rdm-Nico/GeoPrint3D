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
    pub include_water: Option<bool>,  // Include sea / lakes / rivers (default: true)
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
    pub terrain_triangle_count: usize,   // triangles[0..this] are terrain surface (water included)
    pub building_triangle_count: usize,  // triangles[terrain_tc..terrain_tc+this] are buildings
    /// The LAST `water_triangle_count` triangles of the terrain range,
    /// triangles[terrain_tc - this..terrain_tc], are water surface. Clients
    /// that ignore the field simply render water as terrain.
    pub water_triangle_count: usize,
}

#[derive(Debug, Serialize)]
pub struct MeshStats {
    pub vertices: usize,
    pub triangles: usize,
    pub area_km2: f64,
    pub min_elevation: f32,
    pub max_elevation: f32,
    pub buildings_count: usize,
    pub water_features_count: usize,
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

/// Where a building's `height` came from. Tag/Levels are measured OSM data;
/// Block/Neighbors/Density are contextual estimates (services::height_inference);
/// TypeDefault is the plain per-type fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HeightSource {
    Tag,
    Levels,
    Block,
    Neighbors,
    Density,
    TypeDefault,
}

impl HeightSource {
    /// True for heights backed by OSM data (usable as evidence for neighbours).
    pub fn is_measured(&self) -> bool {
        matches!(self, HeightSource::Tag | HeightSource::Levels)
    }

    pub fn name(&self) -> &'static str {
        match self {
            HeightSource::Tag => "tag",
            HeightSource::Levels => "levels",
            HeightSource::Block => "block",
            HeightSource::Neighbors => "neighbors",
            HeightSource::Density => "density",
            HeightSource::TypeDefault => "type_default",
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
    pub kind: String,                     // `building=` (or `building:part=`) value
    pub height_source: HeightSource,
}

/// Class of an OSM water element. Drives the level model in `utils::hydro`:
/// sea and lakes are level water bodies, rivers / canals / streams flow
/// (level bank-to-bank, never rising downstream).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WaterKind {
    Sea,
    Lake,
    River,
    Canal,
    Stream,
}

impl WaterKind {
    pub fn name(&self) -> &'static str {
        match self {
            WaterKind::Sea => "sea",
            WaterKind::Lake => "lake",
            WaterKind::River => "river",
            WaterKind::Canal => "canal",
            WaterKind::Stream => "stream",
        }
    }

    pub fn is_flowing(&self) -> bool {
        matches!(self, WaterKind::River | WaterKind::Canal | WaterKind::Stream)
    }
}

/// A water polygon (`natural=water`, legacy `waterway=riverbank` /
/// `landuse=reservoir`, or sea assembled from coastlines). NOT clipped to the
/// request bbox: `utils::hydro` clips it in metres against the terrain hull.
#[derive(Debug, Clone)]
pub struct WaterArea {
    pub id: u64,
    pub kind: WaterKind,
    pub outer: Vec<(f64, f64)>,      // (lon, lat), no closing duplicate
    pub holes: Vec<Vec<(f64, f64)>>, // islands
}

/// A linear waterway (`waterway=river|canal|stream`), drawn in flow direction.
#[derive(Debug, Clone)]
pub struct Waterway {
    pub id: u64,
    pub kind: WaterKind,
    pub line: Vec<(f64, f64)>, // (lon, lat)
    pub width_m: f32,          // `width` tag or a per-kind default
}

/// Raw water elements of one request, as fetched from OSM.
#[derive(Debug, Clone, Default)]
pub struct WaterFeatures {
    pub areas: Vec<WaterArea>,
    pub waterways: Vec<Waterway>,
    /// `natural=coastline` ways in node order (land on the LEFT).
    pub coastlines: Vec<(u64, Vec<(f64, f64)>)>,
}

impl WaterFeatures {
    pub fn is_empty(&self) -> bool {
        self.areas.is_empty() && self.waterways.is_empty() && self.coastlines.is_empty()
    }
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
