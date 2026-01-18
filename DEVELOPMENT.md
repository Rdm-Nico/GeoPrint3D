# Development Guide

This guide helps you set up a development environment and understand how to work with the GeoPrint3D codebase.

## Quick Start

### 1. Install Prerequisites

#### Rust
```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup update
```

#### Node.js
```bash
# Using nvm (recommended)
curl -o- https://raw.githubusercontent.com/nvm-sh/nvm/v0.39.0/install.sh | bash
nvm install 20
nvm use 20
```

#### PROJ Library (required for coordinate projection)

**macOS**:
```bash
brew install proj
```

**Ubuntu/Debian**:
```bash
sudo apt-get update
sudo apt-get install libproj-dev pkg-config
```

**Windows**:
Download and install from [PROJ website](https://proj.org/install.html)

### 2. Clone and Setup

```bash
cd backend
cargo build
# This will download and compile all Rust dependencies

cd ../frontend
npm install
# This will install all Node.js dependencies
```

### 3. Run Development Servers

**Terminal 1 - Backend**:
```bash
cd backend
cargo run
# Runs on http://localhost:8080
```

**Terminal 2 - Frontend**:
```bash
cd frontend
npm run dev
# Runs on http://localhost:3000
# Auto-proxies /api requests to backend
```

### 4. Access the App

Open http://localhost:3000 in your browser.

## Development Workflow

### Backend Development

#### File Watcher (auto-rebuild)
```bash
cargo install cargo-watch
cargo watch -x run
```

#### Run Tests
```bash
cargo test
cargo test -- --nocapture  # Show println! output
```

#### Check Code Quality
```bash
cargo clippy  # Linter
cargo fmt     # Formatter
```

#### Add Dependencies
```bash
cargo add <crate-name>
# Example: cargo add serde --features derive
```

### Frontend Development

#### Type Checking
```bash
npm run tsc --noEmit  # Check types without building
```

#### Linting
```bash
npm run lint
```

#### Build for Production
```bash
npm run build
npm run preview  # Preview production build
```

#### Add Dependencies
```bash
npm install <package-name>
# Example: npm install axios
```

## Project Structure Deep Dive

### Backend (`backend/src/`)

```
├── main.rs              # Entry point, server setup
│   └── Sets up Axum router and CORS
│
├── api/
│   └── handlers.rs      # HTTP request handlers
│       ├── generate_terrain() # Main endpoint
│       └── health_check()
│
├── models/
│   └── mod.rs          # Data structures
│       ├── BoundingBox
│       ├── GenerateRequest
│       ├── GenerateResponse
│       ├── ElevationPoint
│       ├── Building
│       └── TerrainMesh
│
├── services/
│   ├── elevation.rs    # Fetch elevation data
│   │   ├── fetch_elevation_grid() # Open-Elevation
│   │   ├── fetch_mapbox_terrain() # TODO
│   │   └── fetch_srtm_data()      # TODO
│   │
│   └── osm.rs          # Fetch OpenStreetMap buildings
│       └── fetch_buildings()
│
└── utils/
    ├── projection.rs   # Coordinate transformation
    │   └── CoordinateProjector
    │       ├── new()           # Auto-detect UTM zone
    │       ├── project()       # Single point
    │       └── project_points() # Batch
    │
    ├── mesh.rs         # Mesh generation
    │   └── MeshGenerator
    │       ├── generate_terrain_mesh()   # Delaunay
    │       ├── add_buildings()           # Extrude
    │       ├── close_mesh_with_base()    # Watertight
    │       └── validate_manifold()       # Check
    │
    └── export.rs       # File export
        └── MeshExporter
            ├── export_stl()         # Full resolution
            ├── export_preview_stl() # Decimated
            └── export_obj()         # Alternative format
```

### Frontend (`frontend/src/`)

```
├── main.tsx            # Entry point
│   └── Renders App into DOM
│
├── App.tsx             # Root component
│   ├── State management
│   ├── Layout (header + panels)
│   └── Event handling
│
├── components/
│   ├── MapSelector.tsx # Leaflet map + selection
│   │   └── BoundingBoxSelector # Inner component
│   │       ├── Mouse event handling
│   │       ├── Area calculation
│   │       └── Validation (≤1km²)
│   │
│   └── Preview3D.tsx   # Three.js 3D preview
│       ├── Canvas setup
│       ├── Camera + controls
│       ├── Lighting
│       └── TerrainPlaceholder
│
├── services/
│   └── api.ts          # Backend API client
│       ├── generateTerrain()
│       ├── downloadSTL()
│       └── healthCheck()
│
└── types/
    └── index.ts        # TypeScript interfaces
        ├── BoundingBox
        ├── GenerateRequest
        ├── GenerateResponse
        └── MeshStats
```

## Common Development Tasks

### Adding a New Backend Endpoint

1. **Define handler in `api/handlers.rs`**:
```rust
pub async fn my_new_endpoint(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<MyRequest>,
) -> Result<Json<MyResponse>, ApiError> {
    // Implementation
    Ok(Json(MyResponse { /* ... */ }))
}
```

2. **Add route in `main.rs`**:
```rust
let app = Router::new()
    .route("/api/my-endpoint", post(my_new_endpoint))
    // ... other routes
```

3. **Add types in `frontend/src/types/index.ts`**:
```typescript
export interface MyRequest { /* ... */ }
export interface MyResponse { /* ... */ }
```

4. **Add API client method in `frontend/src/services/api.ts`**:
```typescript
myNewEndpoint: async (request: MyRequest) => {
  const response = await axios.post('/api/my-endpoint', request);
  return response.data;
}
```

### Adding a New Frontend Component

1. **Create component file**:
```bash
touch frontend/src/components/MyComponent.tsx
```

2. **Implement component**:
```typescript
import { useState } from 'react';

interface MyComponentProps {
  data: string;
  onAction: () => void;
}

export default function MyComponent({ data, onAction }: MyComponentProps) {
  return (
    <div className="p-4 bg-white rounded">
      {/* Implementation */}
    </div>
  );
}
```

3. **Import and use in App.tsx**:
```typescript
import MyComponent from './components/MyComponent';

// In App component:
<MyComponent data={someData} onAction={handleAction} />
```

### Modifying the Mesh Generation Algorithm

**File**: `backend/src/utils/mesh.rs`

Key areas to modify:

1. **Terrain resolution**: Adjust Delaunay triangulation parameters
2. **Building extrusion**: Modify `add_building_to_mesh()`
3. **Base pedestal**: Adjust `close_mesh_with_base()`
4. **Manifold validation**: Enhance `validate_manifold()`

Example - Change base pedestal logic:
```rust
pub fn close_mesh_with_base(&self, mesh: &mut TerrainMesh) -> Result<()> {
    // Original logic...

    // Your modifications:
    // - Add beveled edges
    // - Create stepped base
    // - Add support structures

    Ok(())
}
```

### Switching Elevation Data Source

**File**: `backend/src/services/elevation.rs`

Current: Open-Elevation API (free, slower)
Alternative: Mapbox Terrain-RGB (faster, requires API key)

To switch:

1. **Get Mapbox API key**: https://account.mapbox.com/

2. **Implement `fetch_mapbox_terrain()`**:
```rust
pub async fn fetch_mapbox_terrain(
    &self,
    bbox: &BoundingBox,
    resolution: u32,
    mapbox_token: &str,
) -> Result<Vec<ElevationPoint>> {
    // 1. Calculate appropriate zoom level
    // 2. Fetch RGB tiles for bbox
    // 3. Decode RGB to elevation:
    //    height = -10000 + ((R * 256² + G * 256 + B) * 0.1)
    // 4. Resample to desired resolution
}
```

3. **Update handler to use new method**:
```rust
// In api/handlers.rs
let elevation_points = state
    .elevation_service
    .fetch_mapbox_terrain(&request.bbox, resolution, "YOUR_TOKEN")
    .await?;
```

## Debugging

### Backend Debugging

#### Enable Debug Logs
```bash
RUST_LOG=debug cargo run
# Or for specific modules:
RUST_LOG=geoprint3d_backend=debug,axum=info cargo run
```

#### Use `dbg!` Macro
```rust
dbg!(&elevation_points);  // Prints debug info
```

#### Attach Debugger (VSCode)
Add to `.vscode/launch.json`:
```json
{
  "type": "lldb",
  "request": "launch",
  "name": "Debug Backend",
  "cargo": {
    "args": ["build", "--bin=geoprint3d-backend"]
  }
}
```

### Frontend Debugging

#### React DevTools
Install browser extension: [React DevTools](https://react.dev/learn/react-developer-tools)

#### Network Debugging
```typescript
// In services/api.ts, add interceptor:
axios.interceptors.request.use(request => {
  console.log('Starting Request', request);
  return request;
});

axios.interceptors.response.use(response => {
  console.log('Response:', response);
  return response;
});
```

#### Three.js Debugging
Add to `Preview3D.tsx`:
```typescript
import { Stats } from '@react-three/drei';

// In Canvas:
<Stats />
```

## Testing

### Backend Tests

#### Unit Tests
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bbox_validation() {
        let bbox = BoundingBox {
            min_lat: 40.0,
            min_lon: -74.0,
            max_lat: 40.01,
            max_lon: -73.99,
        };
        assert!(bbox.validate_area().is_ok());
    }

    #[tokio::test]
    async fn test_elevation_fetch() {
        let service = ElevationService::new();
        // Test implementation
    }
}
```

Run tests:
```bash
cargo test
cargo test test_bbox_validation  # Run specific test
cargo test -- --test-threads=1   # Serial execution
```

### Frontend Tests

#### Component Tests
```bash
npm install --save-dev @testing-library/react @testing-library/jest-dom vitest
```

```typescript
// MapSelector.test.tsx
import { render, screen } from '@testing-library/react';
import MapSelector from './MapSelector';

test('renders map selector', () => {
  render(<MapSelector onBoundsChange={() => {}} />);
  expect(screen.getByText(/How to Select Area/i)).toBeInTheDocument();
});
```

## Performance Profiling

### Backend Profiling
```bash
cargo install flamegraph
cargo flamegraph
# Open flamegraph.svg
```

### Frontend Profiling
Use React DevTools Profiler tab to identify slow renders.

## Environment Variables

### Backend
Create `backend/.env`:
```env
RUST_LOG=debug
MAPBOX_TOKEN=your_token_here
SRTM_CACHE_DIR=/path/to/cache
```

Load in `main.rs`:
```rust
use dotenv::dotenv;
dotenv().ok();
```

### Frontend
Create `frontend/.env.local`:
```env
VITE_API_BASE_URL=http://localhost:8080
VITE_MAPBOX_TOKEN=your_token_here
```

Access in code:
```typescript
const apiUrl = import.meta.env.VITE_API_BASE_URL;
```

## Common Issues

### `PROJ` library not found
```
error: failed to run custom build command for `proj-sys`
```

**Solution**: Install PROJ library (see Prerequisites section)

### Port already in use
```
Error: Address already in use (os error 48)
```

**Solution**:
```bash
# Find process using port
lsof -ti:8080
# Kill process
kill -9 <PID>
```

### Leaflet CSS not loading
Map tiles show as broken images.

**Solution**: Ensure CSS import in component:
```typescript
import 'leaflet/dist/leaflet.css';
```

### CORS errors
```
Access to XMLHttpRequest blocked by CORS policy
```

**Solution**: Check backend CORS config in `main.rs`:
```rust
let cors = CorsLayer::new()
    .allow_origin(Any)  // Development only!
    .allow_methods(Any)
    .allow_headers(Any);
```

## Code Style

### Rust
- Follow official Rust style guide
- Run `cargo fmt` before commits
- Run `cargo clippy` and fix warnings
- Use `?` operator for error propagation
- Prefer `&str` over `String` for function parameters

### TypeScript/React
- Use functional components with hooks
- Prefer `interface` over `type` for object shapes
- Use TypeScript strict mode
- Follow React best practices (keys, effect dependencies)

## Git Workflow

1. Create feature branch:
```bash
git checkout -b feature/my-feature
```

2. Make changes and commit:
```bash
git add .
git commit -m "feat: add my feature"
```

3. Push and create PR:
```bash
git push origin feature/my-feature
```

## Resources

- [Rust Book](https://doc.rust-lang.org/book/)
- [Axum Documentation](https://docs.rs/axum)
- [React Documentation](https://react.dev)
- [Three.js Documentation](https://threejs.org/docs/)
- [Leaflet Documentation](https://leafletjs.com/)
- [PROJ Documentation](https://proj.org/)
- [STL Format Specification](https://en.wikipedia.org/wiki/STL_(file_format))
