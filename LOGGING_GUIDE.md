# Logging Guide - GeoPrint3D Backend

This guide explains how the logging system works in the GeoPrint3D backend and how to use it to monitor the application.

## Overview

The backend uses the `tracing` and `tracing-subscriber` crates for structured logging. This provides:

- **Hierarchical logging** with different log levels (trace, debug, info, warn, error)
- **Structured output** with timestamps and module names
- **Environment-based configuration** via `RUST_LOG`
- **Performance tracking** with timing information

## Log Levels

The backend uses the following log levels:

| Level | Usage | Example |
|-------|-------|---------|
| `error` | Critical errors that prevent operation | Failed to connect to external API |
| `warn` | Warnings that don't stop execution | Invalid user input, non-manifold mesh |
| `info` | Important operational information | Request started, mesh generated, completion time |
| `debug` | Detailed debugging information | Grid generation, API calls, intermediate steps |
| `trace` | Very verbose debugging | Individual point processing (not used currently) |

## Running with Logging

### Default Logging (Info Level)
```bash
cd backend
cargo run
```

Output:
```
2026-01-18T16:15:03.092456Z  INFO geoprint3d_backend: 🚀 GeoPrint3D Backend starting on 0.0.0.0:8080
```

### Debug Logging (Recommended for Development)
```bash
RUST_LOG=debug cargo run
```

This shows all INFO + DEBUG messages, giving you detailed insight into what's happening.

### Specific Module Logging
```bash
# Only show debug logs from specific modules
RUST_LOG=geoprint3d_backend::services=debug cargo run

# Mix levels for different modules
RUST_LOG=geoprint3d_backend::api=info,geoprint3d_backend::services=debug cargo run

# Show everything (very verbose)
RUST_LOG=trace cargo run
```

### Production Logging
```bash
RUST_LOG=info cargo run --release
```

## Understanding Log Output

### Server Startup
```
2026-01-18T16:15:03.092456Z  INFO geoprint3d_backend: 🚀 GeoPrint3D Backend starting on 0.0.0.0:8080
```

- Timestamp: `2026-01-18T16:15:03.092456Z`
- Level: `INFO`
- Module: `geoprint3d_backend`
- Message: Server is starting

### Request Processing

When a terrain generation request is received, you'll see:

```
2026-01-18T16:15:10.123Z  INFO geoprint3d_backend::api::handlers: === Starting terrain generation ===
2026-01-18T16:15:10.123Z  INFO geoprint3d_backend::api::handlers: Request parameters: bbox=BoundingBox { min_lat: 40.7, min_lon: -74.0, max_lat: 40.71, max_lon: -73.99 }, resolution=Some(100), vertical_scale=Some(1.5), include_buildings=Some(true)
```

### Processing Steps

```
2026-01-18T16:15:10.124Z  INFO geoprint3d_backend::api::handlers: Validated area: 0.0121 km²
2026-01-18T16:15:10.125Z DEBUG geoprint3d_backend::services::elevation: Generating elevation grid with resolution 100x100
2026-01-18T16:15:10.126Z DEBUG geoprint3d_backend::services::elevation: Requesting elevation data for 10201 points from Open-Elevation API
2026-01-18T16:15:15.456Z DEBUG geoprint3d_backend::services::elevation: Received 10201 elevation points from API
2026-01-18T16:15:15.456Z  INFO geoprint3d_backend::services::elevation: Successfully fetched 10201 elevation points
2026-01-18T16:15:15.457Z  INFO geoprint3d_backend::api::handlers: Fetching OSM building data...
2026-01-18T16:15:18.234Z  INFO geoprint3d_backend::services::osm: Fetched 42 buildings from OSM
2026-01-18T16:15:18.235Z  INFO geoprint3d_backend::api::handlers: Projecting coordinates...
2026-01-18T16:15:18.345Z  INFO geoprint3d_backend::utils::projection: Created UTM projection: zone=18, hemisphere=north, origin=(583960.47, 4507523.89)
2026-01-18T16:15:18.346Z  INFO geoprint3d_backend::api::handlers: Generating terrain mesh...
2026-01-18T16:15:18.567Z  INFO geoprint3d_backend::utils::mesh: Generated terrain mesh: 10201 vertices, 20000 triangles
2026-01-18T16:15:18.568Z  INFO geoprint3d_backend::api::handlers: Adding 42 buildings to mesh...
2026-01-18T16:15:18.789Z  INFO geoprint3d_backend::utils::mesh: Added 42 buildings to mesh
2026-01-18T16:15:18.790Z  INFO geoprint3d_backend::api::handlers: Closing mesh with base pedestal...
2026-01-18T16:15:18.891Z  INFO geoprint3d_backend::utils::mesh: Closed mesh with base: added 10201 bottom vertices
2026-01-18T16:15:18.992Z  INFO geoprint3d_backend::utils::mesh: Mesh is manifold (watertight)
```

### Completion

```
2026-01-18T16:15:19.001Z  INFO geoprint3d_backend::api::handlers: === Terrain generation completed successfully ===
  Duration: 8.88s
  Vertices: 20402
  Triangles: 40000
  Buildings: 42
  Elevation range: 5.2m - 45.8m
```

### Error Logging

When errors occur:

```
2026-01-18T16:15:19.001Z  WARN geoprint3d_backend::api::handlers: Bad request: Area 2.50 km² exceeds maximum of 1 km²
```

```
2026-01-18T16:15:19.001Z ERROR geoprint3d_backend::api::handlers: Internal error: Failed to fetch elevation data
```

## Key Log Points in the Code

### 1. Server Startup (main.rs)
```rust
tracing::info!("🚀 GeoPrint3D Backend starting on {}", addr);
```

### 2. Request Received (handlers.rs)
```rust
tracing::info!("=== Starting terrain generation ===");
tracing::info!("Request parameters: bbox={:?}, resolution={:?}, ...", ...);
```

### 3. Area Validation
```rust
tracing::info!("Validated area: {:.4} km²", area_km2);
```

### 4. Elevation Fetching (elevation.rs)
```rust
tracing::debug!("Generating elevation grid with resolution {}x{}", resolution, resolution);
tracing::debug!("Requesting elevation data for {} points from Open-Elevation API", locations.len());
tracing::info!("Successfully fetched {} elevation points", points.len());
```

### 5. Coordinate Projection (projection.rs)
```rust
tracing::info!(
    "Created UTM projection: zone={}, hemisphere={}, origin=({:.2}, {:.2})",
    zone, hemisphere, origin_x, origin_y
);
```

### 6. Mesh Generation (mesh.rs)
```rust
tracing::info!("Generated terrain mesh: {} vertices, {} triangles", vertices.len(), triangles.len());
tracing::info!("Added {} buildings to mesh", buildings.len());
tracing::info!("Closed mesh with base: added {} bottom vertices", original_vertex_count);
```

### 7. Manifold Validation
```rust
tracing::info!("Mesh is manifold (watertight)");
// OR
tracing::warn!("Mesh is not manifold: {} problematic edges", problematic_edges.len());
```

### 8. Completion (handlers.rs)
```rust
tracing::info!(
    "=== Terrain generation completed successfully ===\n  \
    Duration: {:.2}s\n  \
    Vertices: {}\n  \
    Triangles: {}\n  \
    Buildings: {}\n  \
    Elevation range: {:.1}m - {:.1}m",
    elapsed.as_secs_f64(), ...
);
```

### 9. Error Logging
```rust
tracing::warn!("Bad request: {}", msg);
tracing::error!("Internal error: {}", msg);
```

## Monitoring in Production

### 1. Log to File
```bash
RUST_LOG=info cargo run --release 2>&1 | tee logs/geoprint3d.log
```

### 2. JSON Logging (for log aggregation tools)

Add to `Cargo.toml`:
```toml
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }
```

Update `main.rs`:
```rust
tracing_subscriber::fmt()
    .json()
    .with_env_filter(EnvFilter::from_default_env())
    .init();
```

### 3. External Monitoring Tools

For production, consider integrating:
- **Datadog**: Collect and analyze logs
- **Sentry**: Error tracking and alerting
- **OpenTelemetry**: Distributed tracing
- **ELK Stack**: Log aggregation and search

## Performance Analysis

Use the logs to identify bottlenecks:

1. **Elevation API call time**: Check the duration between "Requesting elevation data" and "Successfully fetched"
2. **OSM API call time**: Check the duration for OSM data fetching
3. **Mesh generation time**: Compare start and end of mesh generation
4. **Total request time**: See the "Duration" in the completion message

Example analysis:
```
Total: 8.88s
├── Elevation fetch: ~5.3s (60%)
├── OSM fetch: ~2.8s (32%)
├── Projection: ~0.11s (1%)
├── Mesh generation: ~0.22s (2%)
├── Building extrusion: ~0.22s (2%)
└── Mesh closing: ~0.25s (3%)
```

## Debugging Common Issues

### 1. No Elevation Data
```
ERROR: No elevation data received
```
**Check**:
- Internet connection
- Open-Elevation API status
- Increase timeout in elevation.rs

### 2. Area Too Large
```
WARN: Bad request: Area 2.50 km² exceeds maximum of 1 km²
```
**Solution**: Frontend should validate before sending, but backend catches it

### 3. Non-Manifold Mesh
```
WARN: Mesh is not manifold: 42 problematic edges
```
**Investigation**: Check mesh closing algorithm, may need edge detection improvement

### 4. Slow Performance
**Enable DEBUG logging**:
```bash
RUST_LOG=debug cargo run
```
Check which step takes the most time and optimize accordingly.

## Log Rotation (Production)

For long-running production servers:

```bash
# Using logrotate (Linux)
# Create /etc/logrotate.d/geoprint3d
/var/log/geoprint3d/*.log {
    daily
    rotate 7
    compress
    delaycompress
    missingok
    notifempty
}
```

## Best Practices

1. **Use appropriate log levels**:
   - Don't log at ERROR unless it's truly an error
   - Use DEBUG for information useful during development
   - Use INFO for important operational events

2. **Include context**:
   - Request IDs (add to future implementation)
   - User IDs (when auth is added)
   - Relevant parameters

3. **Avoid logging sensitive data**:
   - Don't log API keys
   - Don't log user credentials
   - Be careful with PII

4. **Monitor log volume**:
   - Too much DEBUG logging in production can impact performance
   - Use INFO as the default production level

5. **Structured logging**:
   - Use consistent message formats
   - Include timestamps automatically (handled by tracing)

## Summary

The GeoPrint3D backend now has comprehensive logging that allows you to:

✅ Track every request from start to finish
✅ Monitor performance of each processing step
✅ Debug issues with detailed information
✅ Identify bottlenecks for optimization
✅ Monitor production deployments effectively

Run with `RUST_LOG=debug cargo run` for the best development experience!
