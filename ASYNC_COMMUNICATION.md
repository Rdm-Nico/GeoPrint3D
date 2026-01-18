# Async Communication Architecture

This document explains how GeoPrint3D handles asynchronous communication between the UI and the heavy geometry processing in Rust.

## Overview

The application uses a **request-response** pattern with HTTP/JSON, where the frontend sends a generation request and waits for the complete response. While this is simple, it handles the heavy processing efficiently through Rust's async runtime (Tokio).

## Communication Flow

```
┌─────────────┐                                 ┌──────────────┐
│  Frontend   │                                 │   Backend    │
│   (React)   │                                 │   (Rust)     │
└──────┬──────┘                                 └──────┬───────┘
       │                                               │
       │ 1. POST /api/generate                        │
       │    (bbox, settings)                           │
       ├──────────────────────────────────────────────>│
       │                                               │
       │                                               │ 2. Validate area
       │                                               │
       │                                               │ 3. Spawn async tasks
       │                                               │    ┌─────────────────┐
       │                                               │───>│ Fetch Elevation │
       │                                               │    └─────────────────┘
       │                                               │    ┌─────────────────┐
       │                                               │───>│ Fetch Buildings │
       │                                               │    └─────────────────┘
       │                                               │
       │                                               │ 4. Await both tasks
       │                                               │
       │                                               │ 5. Process in order:
       │                                               │    - Project coords
       │                                               │    - Generate mesh
       │                                               │    - Add buildings
       │                                               │    - Close mesh
       │                                               │    - Validate
       │                                               │    - Export STL
       │                                               │
       │ 6. Response                                   │
       │    (preview_url, stl_url, stats)              │
       │<──────────────────────────────────────────────┤
       │                                               │
       │ 7. GET /api/download/file.stl                 │
       ├──────────────────────────────────────────────>│
       │                                               │
       │ 8. STL file (binary)                          │
       │<──────────────────────────────────────────────┤
       │                                               │
```

## Backend Async Design

### Axum Handler (Async Function)

```rust
pub async fn generate_terrain(
    State(state): State<Arc<AppState>>,
    Json(request): Json<GenerateRequest>,
) -> Result<Json<GenerateResponse>, ApiError> {
    // This entire function is async and non-blocking

    // 1. Validate (sync, fast)
    request.bbox.validate_area()?;

    // 2. Fetch external data (async, parallel)
    let (elevation_points, buildings) = tokio::join!(
        state.elevation_service.fetch_elevation_grid(&request.bbox, resolution),
        state.osm_service.fetch_buildings(&request.bbox)
    );

    // 3. Process data (sync, CPU-bound)
    // Note: This could be moved to a blocking task pool for better throughput
    let projector = CoordinateProjector::new(&request.bbox)?;
    let projected_points = projector.project_points(&elevation_points)?;

    let mesh_generator = MeshGenerator::new(projector, vertical_scale, base_height);
    let mut mesh = mesh_generator.generate_terrain_mesh(&projected_points)?;
    mesh_generator.add_buildings(&mut mesh, &buildings)?;
    mesh_generator.close_mesh_with_base(&mut mesh)?;

    // 4. Return response
    Ok(Json(GenerateResponse { /* ... */ }))
}
```

### Key Async Components

#### 1. External API Calls (Network I/O)
```rust
// services/elevation.rs
impl ElevationService {
    pub async fn fetch_elevation_grid(&self, /* ... */) -> Result<Vec<ElevationPoint>> {
        // reqwest::Client is async
        let response = self.client
            .post("https://api.open-elevation.com/api/v1/lookup")
            .json(&locations)
            .send()
            .await?;  // Non-blocking wait

        let data = response.json().await?;
        Ok(data)
    }
}
```

#### 2. Parallel Task Execution
```rust
// Execute multiple async operations concurrently
let (elevation_result, osm_result) = tokio::join!(
    fetch_elevation_task,
    fetch_osm_task
);

// Or with error handling:
use futures::try_join;
let (elevation, buildings) = try_join!(
    elevation_service.fetch_elevation_grid(&bbox, resolution),
    osm_service.fetch_buildings(&bbox)
)?;
```

#### 3. CPU-Bound Operations
```rust
// Option 1: Run in current async context (blocks the async task)
let mesh = mesh_generator.generate_terrain_mesh(&points)?;

// Option 2: Spawn blocking task (better for throughput)
let mesh = tokio::task::spawn_blocking(move || {
    mesh_generator.generate_terrain_mesh(&points)
}).await??;
```

## Frontend Async Design

### Axios Request with Loading State

```typescript
const handleGenerate = async () => {
  // 1. Set loading state
  setGenerating(true);
  setError(null);

  try {
    // 2. Await async API call
    const response = await api.generateTerrain({
      bbox: selectedBounds,
      resolution,
      vertical_scale: verticalScale,
      // ...
    });

    // 3. Update UI with result
    setResult(response);
  } catch (err: any) {
    // 4. Handle errors
    setError(err.response?.data?.error || err.message);
  } finally {
    // 5. Clear loading state
    setGenerating(false);
  }
};
```

### API Client Configuration

```typescript
// services/api.ts
export const api = {
  generateTerrain: async (request: GenerateRequest): Promise<GenerateResponse> => {
    const response = await axios.post<GenerateResponse>(
      `${API_BASE_URL}/generate`,
      request,
      {
        timeout: 120000, // 2 minutes - allows for heavy processing
        // Axios automatically handles JSON serialization/deserialization
      }
    );
    return response.data;
  },
};
```

## Handling Long-Running Tasks

### Current Approach: Synchronous Response

**Pros**:
- Simple implementation
- No additional infrastructure
- Suitable for processing times < 2 minutes

**Cons**:
- Client must wait for entire processing
- No progress updates
- Timeout risk for large areas

### Future Enhancement: Job Queue with Polling

For production at scale, consider:

#### 1. Submit Job
```typescript
// Frontend
const jobId = await api.submitJob(request);
setJobId(jobId);
```

```rust
// Backend
pub async fn submit_job(
    Json(request): Json<GenerateRequest>,
) -> Result<Json<JobResponse>, ApiError> {
    let job_id = Uuid::new_v4().to_string();

    // Queue the job
    job_queue.push(Job {
        id: job_id.clone(),
        request,
        status: JobStatus::Pending,
    });

    Ok(Json(JobResponse { job_id }))
}
```

#### 2. Poll for Status
```typescript
// Frontend - poll every 2 seconds
const pollJob = async (jobId: string) => {
  const interval = setInterval(async () => {
    const status = await api.getJobStatus(jobId);

    if (status.state === 'completed') {
      clearInterval(interval);
      setResult(status.result);
    } else if (status.state === 'failed') {
      clearInterval(interval);
      setError(status.error);
    } else {
      // Update progress
      setProgress(status.progress);
    }
  }, 2000);
};
```

```rust
// Backend
pub async fn get_job_status(
    Path(job_id): Path<String>,
) -> Result<Json<JobStatus>, ApiError> {
    let status = job_store.get(&job_id)?;
    Ok(Json(status))
}
```

#### 3. Process in Background Worker
```rust
// Separate Tokio task
tokio::spawn(async move {
    while let Some(job) = job_queue.pop().await {
        job_store.update(job.id, JobStatus::Processing);

        match process_job(job.request).await {
            Ok(result) => {
                job_store.update(job.id, JobStatus::Completed { result });
            }
            Err(e) => {
                job_store.update(job.id, JobStatus::Failed { error: e.to_string() });
            }
        }
    }
});
```

### Alternative: Server-Sent Events (SSE)

Real-time progress updates:

```typescript
// Frontend
const eventSource = new EventSource(`/api/generate-stream?bbox=...`);

eventSource.onmessage = (event) => {
  const data = JSON.parse(event.data);

  if (data.type === 'progress') {
    setProgress(data.progress);
    setStatus(data.message);
  } else if (data.type === 'complete') {
    setResult(data.result);
    eventSource.close();
  }
};
```

```rust
// Backend (using axum-streams or similar)
pub async fn generate_stream(
    Query(params): Query<GenerateParams>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = async_stream::stream! {
        yield Event::default().data("Starting generation...");

        // Fetch elevation
        yield Event::default().json_data(Progress { step: 1, total: 5 }).unwrap();
        let elevation = fetch_elevation().await;

        // Fetch buildings
        yield Event::default().json_data(Progress { step: 2, total: 5 }).unwrap();
        let buildings = fetch_buildings().await;

        // ... continue with progress updates

        yield Event::default().json_data(CompleteMessage { result }).unwrap();
    };

    Sse::new(stream)
}
```

## Error Handling Across Async Boundary

### Backend Error Types

```rust
pub enum ApiError {
    BadRequest(String),
    InternalError(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            ApiError::InternalError(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        ApiError::InternalError(err.to_string())
    }
}
```

### Frontend Error Handling

```typescript
try {
  const response = await api.generateTerrain(request);
  setResult(response);
} catch (err: any) {
  if (err.response) {
    // Backend returned error response
    setError(err.response.data.error);
  } else if (err.request) {
    // Request sent but no response (timeout, network error)
    setError('Network error. Please check your connection.');
  } else {
    // Error in request setup
    setError(err.message);
  }
}
```

## Performance Optimizations

### 1. Connection Pooling

Reuse HTTP connections:

```rust
// In services
pub struct ElevationService {
    client: reqwest::Client,  // Maintains connection pool
}

impl ElevationService {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .pool_max_idle_per_host(10)
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap(),
        }
    }
}
```

### 2. Parallel Processing

```rust
// Process multiple regions in parallel
let futures: Vec<_> = regions
    .iter()
    .map(|region| fetch_elevation(region))
    .collect();

let results = futures::future::join_all(futures).await;
```

### 3. Cancellation

Handle request cancellation:

```rust
use tokio::select;

pub async fn generate_terrain(/* ... */) -> Result<Json<GenerateResponse>, ApiError> {
    select! {
        result = actual_processing() => {
            result
        }
        _ = tokio::signal::ctrl_c() => {
            Err(ApiError::InternalError("Cancelled".to_string()))
        }
    }
}
```

Frontend cancellation:

```typescript
const controller = new AbortController();

const response = await axios.post('/api/generate', request, {
  signal: controller.signal,
});

// Cancel on unmount
useEffect(() => {
  return () => controller.abort();
}, []);
```

## Monitoring Async Operations

### Backend Tracing

```rust
use tracing::{info, instrument};

#[instrument(skip(state))]
pub async fn generate_terrain(/* ... */) -> Result</* ... */> {
    info!("Starting terrain generation for bbox: {:?}", request.bbox);

    let start = Instant::now();
    let result = do_work().await?;

    info!("Terrain generation completed in {:?}", start.elapsed());
    Ok(result)
}
```

### Frontend Performance Monitoring

```typescript
const startTime = performance.now();

try {
  const response = await api.generateTerrain(request);
  const duration = performance.now() - startTime;

  console.log(`Generation took ${duration}ms`);

  // Send to analytics
  analytics.track('terrain_generated', {
    duration,
    area: response.stats.area_km2,
    vertices: response.stats.vertices,
  });
} catch (err) {
  // Track errors
  analytics.track('generation_failed', { error: err.message });
}
```

## Summary

The current architecture uses a simple request-response pattern that works well for the MVP:

1. **Frontend** sends async request via Axios with loading state
2. **Backend** processes async using Tokio runtime
3. **External APIs** called in parallel using `tokio::join!`
4. **CPU-bound work** runs in async context (can be optimized later)
5. **Response** sent back to frontend when complete

For production at scale, consider:
- Job queue with polling for long-running tasks
- Server-Sent Events for real-time progress
- WebSocket for bidirectional communication
- Caching frequently requested areas
- Rate limiting per user
