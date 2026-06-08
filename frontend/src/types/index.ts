export interface BoundingBox {
  min_lat: number;
  min_lon: number;
  max_lat: number;
  max_lon: number;
}

export interface GenerateRequest {
  bbox: BoundingBox;
  resolution?: number;
  vertical_scale?: number;
  base_height?: number;
  include_buildings?: boolean;
}

export interface MeshStats {
  vertices: number;
  triangles: number;
  area_km2: number;
  min_elevation: number;
  max_elevation: number;
  buildings_count: number;
}

export interface MeshData {
  vertices: [number, number, number][];  // [x, y, z] positions
  triangles: [number, number, number][]; // Triangle indices into vertices
  terrain_triangle_count: number;        // triangles[0..this] are terrain
  building_triangle_count: number;       // triangles[terrain_tc..terrain_tc+this] are buildings
}

export interface GenerateResponse {
  stats: MeshStats;
  mesh_data: MeshData;  // The actual 3D mesh data for rendering
}

export interface MapBounds {
  north: number;
  south: number;
  east: number;
  west: number;
}
