# GeoPrint3D

Transform real-world terrain and buildings into 3D-printable STL files.

## Overview

GeoPrint3D is a full-stack web application that allows users to select any location on Earth (up to 1km²) and generate a 3D-printable model of the terrain, complete with buildings. The application fetches real elevation data and building information to create accurate, watertight STL files optimized for 3D printing.

## Features

- **Interactive Map Selection**: Select any area on Earth using an intuitive map interface
- **Area Validation**: Automatic enforcement of 1km² maximum area constraint
- **Real Terrain Data**: Fetches elevation data from Open-Elevation API (SRTM)
- **Building Integration**: Includes building footprints and heights from OpenStreetMap
- **Accurate Projection**: Converts lat/lon to UTM for 1:1 scale accuracy
- **Watertight Meshes**: Generates manifold meshes with base pedestals for reliable 3D printing
- **3D Preview**: Interactive preview using Three.js before download
- **Responsive Design**: Works on desktop, tablet, and mobile devices
- **Customizable**: Adjust resolution, vertical scale, and base height

## Tech Stack

### Backend (Rust)
- **Framework**: Axum (high-performance async web framework)
- **Geospatial**: `geo`, `geojson`, `proj` (coordinate projection)
- **3D Processing**: `nalgebra`, `delaunator` (Delaunay triangulation)
- **STL Export**: `stl_io`
- **Data Sources**:
  - Open-Elevation API for terrain
  - Overpass API for OpenStreetMap buildings

### Frontend (React + TypeScript)
- **Framework**: React 18 with TypeScript
- **UI**: Tailwind CSS (responsive, mobile-first)
- **Map**: React-Leaflet (interactive world map)
- **3D Preview**: React-Three-Fiber + Three.js
- **Build Tool**: Vite
- **API Client**: Axios

## Architecture

```
┌─────────────┐         HTTP/JSON          ┌──────────────┐
│   Frontend  │ ◄─────────────────────────► │   Backend    │
│   (React)   │                             │   (Rust)     │
│             │                             │              │
│ • Map UI    │                             │ • API Routes │
│ • 3D View   │                             │ • Services   │
│ • Controls  │                             │ • Mesh Gen   │
└─────────────┘                             └──────┬───────┘
                                                   │
                                                   ├─► Open-Elevation API
                                                   └─► Overpass API (OSM)
```

## Project Structure

```
3D-World-Printer/
├── backend/                 # Rust backend
│   ├── src/
│   │   ├── api/            # HTTP handlers
│   │   ├── models/         # Data structures
│   │   ├── services/       # External API clients
│   │   │   ├── elevation.rs
│   │   │   └── osm.rs
│   │   ├── utils/          # Core algorithms
│   │   │   ├── projection.rs  # Lat/Lon → UTM
│   │   │   ├── mesh.rs        # Mesh generation
│   │   │   └── export.rs      # STL export
│   │   └── main.rs
│   └── Cargo.toml
│
└── frontend/               # React frontend
    ├── src/
    │   ├── components/    # UI components
    │   │   ├── MapSelector.tsx
    │   │   └── Preview3D.tsx
    │   ├── services/      # API client
    │   ├── types/         # TypeScript types
    │   ├── App.tsx
    │   └── main.tsx
    ├── package.json
    └── vite.config.ts
```

## Setup & Installation

### Prerequisites

- **Rust** (1.75+): [Install Rust](https://rustup.rs/)
- **Node.js** (20+): [Install Node](https://nodejs.org/)
- **PROJ Library**: Required for coordinate projection
  - macOS: `brew install proj`
  - Ubuntu: `sudo apt-get install libproj-dev`
  - Windows: Download from [PROJ website](https://proj.org/)

### Backend Setup

```bash
cd backend

# Install dependencies (handled by Cargo)
cargo build

# Run the backend server
cargo run

# Backend will start on http://localhost:8080
```

### Frontend Setup

```bash
cd frontend

# Install dependencies
npm install

# Run development server
npm run dev

# Frontend will start on http://localhost:3000
```

### Production Build

```bash
# Backend
cd backend
cargo build --release
./target/release/geoprint3d-backend

# Frontend
cd frontend
npm run build
# Serve the dist/ folder with your preferred web server
```

## Build Status

✅ **Build Verified**: Backend compiles successfully and server starts without errors.

See `BUILD_STATUS.md` for detailed build verification and fixes applied.

## Logging

The backend includes comprehensive logging for monitoring and debugging. Use:

```bash
# Detailed logging for development
RUST_LOG=debug cargo run

# Production logging
RUST_LOG=info cargo run --release
```

See `LOGGING_GUIDE.md` for complete logging documentation.

## Usage

1. **Open the application** in your browser (http://localhost:3000)

2. **Navigate the map** to your desired location

3. **Select an area**:
   - Click and hold to start selection
   - Drag to define bounding box
   - Release to confirm (max 1 km²)

4. **Configure settings** (optional):
   - Resolution: Grid density (50-200)
   - Vertical Scale: Terrain exaggeration (0.5-3x)
   - Base Height: Pedestal thickness (1-5mm)
   - Include Buildings: Toggle OSM buildings

5. **Generate** the 3D model

6. **Preview** the model in the interactive 3D viewer

7. **Download** the STL file for 3D printing

## API Endpoints

### `GET /api/health`
Health check endpoint.

**Response:**
```json
{
  "status": "healthy",
  "service": "GeoPrint3D",
  "version": "0.1.0"
}
```

### `POST /api/generate`
Generate 3D terrain model from bounding box.

**Request:**
```json
{
  "bbox": {
    "min_lat": 40.7,
    "min_lon": -74.0,
    "max_lat": 40.71,
    "max_lon": -73.99
  },
  "resolution": 100,
  "vertical_scale": 1.5,
  "base_height": 2.0,
  "include_buildings": true
}
```

**Response:**
```json
{
  "preview_url": "/api/download/terrain_123_preview.stl",
  "stl_url": "/api/download/terrain_123.stl",
  "stats": {
    "vertices": 10201,
    "triangles": 20000,
    "area_km2": 0.0121,
    "min_elevation": 5.2,
    "max_elevation": 45.8,
    "buildings_count": 23
  }
}
```

## Key Implementation Details

### 1. Coordinate Projection
Converts WGS84 lat/lon to UTM (Universal Transverse Mercator) for accurate metric measurements:

```rust
// Automatically determines correct UTM zone
let projector = CoordinateProjector::new(&bbox)?;
let projected = projector.project(lon, lat, elevation)?;
```

### 2. Mesh Generation
Uses Delaunay triangulation for optimal terrain mesh:

```rust
let mesh_generator = MeshGenerator::new(projector, vertical_scale, base_height);
let mut mesh = mesh_generator.generate_terrain_mesh(&projected_points)?;
```

### 3. Watertight Mesh (Critical for 3D Printing)
Adds base pedestal to close the mesh:

```rust
mesh_generator.close_mesh_with_base(&mut mesh)?;
mesh_generator.validate_manifold(&mesh)?; // Ensures printability
```

### 4. Building Extrusion
Extrudes building footprints with real heights:

```rust
mesh_generator.add_buildings(&mut mesh, &buildings)?;
```

## Data Sources

### Elevation Data
- **Current**: Open-Elevation API (free, SRTM-based)
- **Alternative**: Mapbox Terrain-RGB (requires API key, higher quality)
- **Alternative**: Local SRTM tiles (best performance)

### Building Data
- **Source**: OpenStreetMap via Overpass API
- **Includes**: Building footprints, heights, levels
- **Fallback**: Default 10m height if not specified

## Troubleshooting

### Backend fails to compile
- Ensure PROJ library is installed: `brew install proj` (macOS) or `apt-get install libproj-dev` (Ubuntu)
- Run `cargo clean && cargo build`

### Frontend map not loading
- Check that port 3000 is not in use
- Verify backend is running on port 3001
- Check browser console for errors

### STL file won't print
- Ensure mesh is watertight (backend validates automatically)
- Check slicer settings (base pedestal should be visible)
- Scale appropriately in your slicer software

### No elevation data received
- Check internet connection
- Open-Elevation API may be rate-limited (try again later)
- Consider implementing Mapbox Terrain-RGB alternative

## Future Enhancements

- [ ] Implement Mapbox Terrain-RGB for higher resolution
- [ ] Add SRTM tile caching for offline operation
- [ ] Support for custom elevation data upload
- [ ] Texture mapping from satellite imagery
- [ ] Multi-material STL export (terrain vs buildings)
- [ ] Real-time collaboration (share selections)
- [ ] Export to other formats (OBJ, glTF)
- [ ] Advanced mesh decimation algorithms
- [ ] Water bodies and vegetation layers
- [ ] Historical terrain comparison

## Contributing

Contributions are welcome! Please feel free to submit issues or pull requests.

## License

MIT License - See LICENSE file for details

## Acknowledgments

- Elevation data: SRTM / Open-Elevation
- Building data: OpenStreetMap contributors
- Map tiles: OpenStreetMap
- Coordinate projection: PROJ library
