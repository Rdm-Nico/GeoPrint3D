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

export interface GenerateResponse {
  preview_url: string;
  stl_url: string;
  stats: MeshStats;
}

export interface MapBounds {
  north: number;
  south: number;
  east: number;
  west: number;
}
