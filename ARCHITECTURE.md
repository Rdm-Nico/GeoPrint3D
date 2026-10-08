# GeoPrint3D Architecture

## System Overview

GeoPrint3D follows a modern client-server architecture with clear separation of concerns.

## Architecture Diagram

```
┌──────────────────────────────────────────────────────────────┐
│                         Frontend (React)                      │
│  ┌────────────┐  ┌────────────┐  ┌──────────────────────┐   │
│  │ MapSelector│  │ Preview3D  │  │   Control Panel      │   │
│  │ (Leaflet)  │  │(Three.js)  │  │  (Settings/Actions)  │   │
│  └─────┬──────┘  └─────┬──────┘  └──────────┬───────────┘   │
│        └─────────────┬──────────────────────┘                │
│                      │                                        │
│                 ┌────▼────┐                                   │
│                 │ API     │                                   │
│                 │ Client  │                                   │
│                 └────┬────┘                                   │
└──────────────────────┼───────────────────────────────────────┘
                       │ HTTP/JSON
                       │ (Axios)
┌──────────────────────▼───────────────────────────────────────┐
│                      Backend (Rust/Axum)                      │
│  ┌────────────────────────────────────────────────────────┐  │
│  │              API Layer (handlers.rs)                   │  │
│  │  • POST /api/generate                                  │  │
│  │  • GET /api/health                                     │  │
│  │  • GET /api/download/:filename (TODO)                 │  │
│  └─────────────────┬──────────────────────────────────────┘  │
│                    │                                          │
│  ┌─────────────────▼──────────────────────────────────────┐  │
│  │           Services Layer                              │  │
│  │  ┌─────────────────┐    ┌──────────────────────┐     │  │
│  │  │ElevationService │    │    OsmService        │     │  │
│  │  │ • Open-Elevation│    │  • Overpass API      │     │  │
│  │  │ • Mapbox (TODO) │    │  • Building data     │     │  │
│  │  └────────┬────────┘    └──────────┬───────────┘     │  │
│  └───────────┼──────────────────────────┼────────────────┘  │
│              │                          │                    │
│  ┌───────────▼──────────────────────────▼────────────────┐  │
│  │              Utils Layer                              │  │
│  │  ┌──────────────┐ ┌───────────┐ ┌─────────────────┐  │  │
│  │  │ Projection   │ │  Mesh     │ │   Export        │  │  │
│  │  │ (WGS84→UTM) │ │Generator  │ │  (STL/OBJ)      │  │  │
│  │  └──────────────┘ └───────────┘ └─────────────────┘  │  │
│  └───────────────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────────────────┘
                       │                │
                ┌──────▼────────┐  ┌───▼────────┐
                │ Open-Elevation│  │  Overpass  │
                │     API       │  │    API     │
                └───────────────┘  └────────────┘
```

## Data Flow

### 1. User Selection Flow

```
User clicks map → MapSelector captures bounds
                    ↓
              BBox validation (≤1km²)
                    ↓
              selectedBounds state updated
                    ↓
              Enable "Generate" button
```

### 2. Generation Request Flow

```
User clicks "Generate"
    ↓
Frontend sends POST /api/generate with:
{
  bbox: { min_lat, min_lon, max_lat, max_lon },
  resolution: 100,
  vertical_scale: 1.5,
  base_height: 2.0,
  include_buildings: true
}
    ↓
Backend receives request
    ↓
┌───────────────────────────────────────┐
│  1. Validate area (≤1km²)             │
│  2. Fetch elevation grid              │
│  3. Fetch building footprints (OSM)   │
│  4. Project to UTM coordinates        │
│  5. Generate terrain mesh (Delaunay)  │
│  6. Extrude buildings                 │
│  7. Close mesh with base pedestal     │
│  8. Validate manifold (watertight)    │
│  9. Export to STL                     │
│ 10. Return URLs + stats               │
└───────────────────────────────────────┘
    ↓
Frontend receives response:
{
  preview_url: "...",
  stl_url: "...",
  stats: { vertices, triangles, ... }
}
    ↓
Update Preview3D + enable Download button
```

## Backend Architecture

### Module Organization

```rust
backend/src/
├── main.rs              // Server setup, routing
├── api/
│   ├── mod.rs
│   └── handlers.rs      // HTTP request handlers
├── models/
│   └── mod.rs          // Data structures (BBox, Request, Response)
├── services/
│   ├── mod.rs
│   ├── elevation.rs    // Fetch elevation data
│   ├── osm.rs          // Fetch OSM buildings
│   └── height_inference.rs // Estimate missing building heights
└── utils/
    ├── mod.rs
    ├── projection.rs   // Coordinate transformation
    ├── mesh.rs         // Mesh generation algorithms
    └── export.rs       // STL/OBJ file export
```

### Key Algorithms

#### Coordinate Projection
```rust
// Convert WGS84 (lat/lon) to UTM (meters)
// Ensures 1:1 scale accuracy for printing

CoordinateProjector::new(bbox)
  1. Determine UTM zone from longitude
  2. Determine hemisphere from latitude
  3. Create PROJ transformation: EPSG:4326 → UTM
  4. Project all points relative to center (origin)
```

#### Mesh Generation
```rust
MeshGenerator::generate_terrain_mesh(points)
  1. Extract 2D points (x, y)
  2. Perform Delaunay triangulation
  3. Build 3D vertices with scaled elevation (z)
  4. Create triangle indices from triangulation

MeshGenerator::add_buildings(mesh, buildings)
  For each building:
    1. Project footprint coordinates
    2. Create bottom vertices (at base elevation)
    3. Create top vertices (at base + height)
    4. Generate wall triangles (sides)
    5. Generate top face triangles

MeshGenerator::close_mesh_with_base(mesh)
  1. Find minimum elevation
  2. Create base layer (min_z - base_height)
  3. Duplicate all vertices at base height
  4. Generate bottom face (flipped triangles)
  5. Connect perimeter with walls (TODO: proper edge detection)
```

#### Building Heights
Over 90% of OSM buildings in a typical Italian city carry no `height` or
`building:levels` tag, so most heights must be estimated. The pipeline has
three stages:

```
1. Tags (services/osm.rs)
   height / est_height → building:levels × 3.2 m (+ roof:levels) → per-type default

2. Contextual inference (services/height_inference.rs)
   Only for outlines that fell back to the per-type default and have a
   generic type (yes, residential, apartments, commercial, house, …):
     a. Density prior: built-up ratio ρ in a 100 m disk → storeys
        interpolated in a per-type range (generic 2.5–5, apartments 3.5–6),
        sparse at ρ ≤ 0.10, dense core at ρ ≥ 0.60
     b. Tagged neighbours within 150 m (≥ 3 samples): IDW-weighted median,
        blended with the prior as (n·h_nbr + 3·h_prior) / (n + 3)
     c. Block harmonisation: outlines that share OSM nodes (terraced
        fabric / isolati) take the area-weighted median of their block
        within 60 m, pulled toward any tagged block members
     d. ±4% deterministic jitter per OSM id
   A leave-one-out check on tagged buildings is logged on every run
   (Bologna: MAE 6.7 m with type defaults → 5.2 m).

3. Print scaling (api/handlers.rs)
   Tall landmarks above a knee (3 × median height, at least 20 m) are
   soft-compressed. Then one bounded vertical exaggeration is applied to all
   buildings: the scene's median building prints at ~2 mm (vertical_scale = 2),
   exaggeration clamped to 1–3× of the true XY scale. In urban scenes terrain
   is never exaggerated more than the buildings. Ground-level outlines print
   at least 0.6 mm tall.
```

Garages, churches, towers, industrial buildings and `building:part` elements
keep their tagged or per-type heights.

The approach follows the urban-form literature on predicting building heights
from 2D data:

- F. Biljecki, H. Ledoux, J. Stoter (2017), *Generating 3D city models without
  elevation data*, Computers, Environment and Urban Systems 64.
  [link](https://3d.bk.tudelft.nl/news/2017/01/17/inferring-heights-ceus-paper.html)
- N. Milojevic-Dupont et al. (2020), *Learning from urban form to predict
  building heights*, PLOS ONE.
  [link](https://depositonce.tu-berlin.de/items/6a651826-232d-4e7b-9ecb-fceeb025e432/full)

Candidate external height sources for a future phase:

- X. X. Zhu et al. (2025), *GlobalBuildingAtlas: an open global and complete
  dataset of building polygons, heights and LoD1 3D models*, Earth System
  Science Data 17. [link](https://essd.copernicus.org/articles/17/6647/2025/)
- EUBUCCO (Milojevic-Dupont et al. 2023, Scientific Data): EU building stock
  with heights. [link](https://ual.sg/publication/2023-sd-eubucco/)

#### Manifold Validation
```rust
validate_manifold(mesh)
  1. Build edge → face map
  2. For each edge: count sharing faces
  3. Valid manifold: every edge shared by exactly 2 faces
  4. Report problematic edges if any
```

## Frontend Architecture

### Component Hierarchy

```
App.tsx (root)
├── Header
├── MapSelector
│   └── BoundingBoxSelector (inner component)
└── Preview & Controls Panel
    ├── Preview3D
    │   ├── Canvas (Three.js)
    │   ├── TerrainPlaceholder
    │   └── Stats Overlay
    └── Control Panel
        ├── Settings (collapsible)
        ├── Generate Button
        └── Download Button
```

### State Management

All state is managed at the App level (no Redux needed for MVP):

```typescript
// Selection state
selectedBounds: BoundingBox | null

// Generation state
generating: boolean
result: GenerateResponse | null
error: string | null

// Settings state
resolution: number
verticalScale: number
baseHeight: number
includeBuildings: boolean
```

### API Client

Simple Axios wrapper in `services/api.ts`:

```typescript
api.generateTerrain(request) → Promise<GenerateResponse>
api.downloadSTL(url) → void (opens in new tab)
api.healthCheck() → Promise<any>
```

## Async Communication

### Frontend → Backend
- **Protocol**: HTTP/JSON
- **Timeout**: 120 seconds (terrain generation is CPU-intensive)
- **Error Handling**: Axios interceptors catch network errors

### Backend → External APIs
- **Elevation**: Open-Elevation (batch POST with locations array)
- **Buildings**: Overpass API (custom Overpass QL query)
- **Retry Logic**: TODO - add exponential backoff

## Performance Considerations

### Backend Optimizations

1. **Parallel Processing** (TODO):
   - Fetch elevation and OSM data concurrently
   - Use `tokio::join!` for async operations

2. **Caching** (TODO):
   - Cache elevation tiles locally
   - Cache OSM building data by bounding box

3. **Streaming** (TODO):
   - Stream STL file directly instead of saving to disk
   - Use chunked transfer encoding

### Frontend Optimizations

1. **Code Splitting**:
   - Three.js loaded lazily (via `Suspense`)
   - Heavy components split by route

2. **Debouncing**:
   - Map events debounced to prevent excessive renders

3. **Progressive Enhancement**:
   - Show low-poly preview immediately
   - Load full STL on demand

## Security Considerations

### Input Validation
- **Backend**: Validate bbox area ≤1km² server-side
- **Backend**: Validate resolution range (50-200)
- **Backend**: Sanitize file paths for download endpoint

### Rate Limiting (TODO)
- Implement per-IP rate limiting
- Prevent abuse of external APIs

### CORS
- Currently allows all origins (development)
- **Production**: Restrict to frontend domain

## Scalability

### Current Limitations
- Single-threaded mesh generation
- In-memory file storage
- Synchronous API calls

### Future Improvements
- **Queue System**: Use message queue (Redis/RabbitMQ) for long-running tasks
- **Object Storage**: Store STL files in S3/MinIO
- **Database**: Track requests, cache results
- **CDN**: Serve static STL files via CDN
- **Microservices**: Split elevation/OSM/mesh services

## Testing Strategy

### Backend Tests
```rust
// Unit tests for each module
#[cfg(test)]
mod tests {
    // Test coordinate projection
    // Test mesh generation
    // Test manifold validation
}
```

### Frontend Tests
```typescript
// Component tests with React Testing Library
// API client tests with MSW (Mock Service Worker)
// E2E tests with Playwright (TODO)
```

## Deployment

### Development
```bash
# Terminal 1: Backend
cd backend && cargo run

# Terminal 2: Frontend
cd frontend && npm run dev
```

### Production

**Backend**:
```bash
cargo build --release
./target/release/geoprint3d-backend
```

**Frontend**:
```bash
npm run build
# Serve dist/ with nginx or similar
```

**Docker** (TODO):
```dockerfile
# Multi-stage build
FROM rust:1.75 as backend-builder
# ... build backend

FROM node:20 as frontend-builder
# ... build frontend

FROM debian:bookworm-slim
# ... copy binaries and serve
```

## Monitoring & Observability

### Logging
- **Backend**: `tracing` crate with JSON output
- **Frontend**: Browser console + Sentry (TODO)

### Metrics (TODO)
- Request duration
- Mesh generation time
- External API latency
- Error rates

### Tracing (TODO)
- Distributed tracing with OpenTelemetry
- Trace requests across services
