use crate::models::{
    BoundingBox, Building, HeightSource, RoofShape, WaterArea, WaterFeatures, WaterKind, Waterway,
};
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

// Community-run Overpass mirrors — tried first (compact out geom response).
const OVERPASS_ENDPOINTS: &[&str] = &[
    "https://overpass-api.de/api/interpreter",
    "https://overpass.kumi.systems/api/interpreter",
    "https://overpass.private.coffee/api/interpreter",
];

// Hard floor for the footprint filter (m²): below this a building is sub-pixel
// on ANY print. The effective threshold grows with the mapped area (the handler
// passes a print-resolution-aware value).
const MIN_BUILDING_AREA_M2: f64 = 10.0;

// A single Overpass / OSM-API query never covers more than this area: dense-city
// responses above it time out or exceed the OSM API 50k-node limit. Larger
// requests are split into a tile grid and merged.
const TILE_MAX_KM2: f64 = 4.0;

// A building outline is suppressed when ground-level building:part elements
// cover at least this fraction of its footprint (Simple 3D Buildings: parts
// replace the outline, which only describes the ground-plan envelope).
const PART_COVERAGE_SUPPRESS_FRAC: f64 = 0.55;

/// Counters for where building heights came from (diagnostics).
#[derive(Default)]
struct HeightSources {
    tag: usize,
    levels: usize,
    default: usize,
    skipped: usize,
}

/// Accumulated diagnostics across all tiles of one fetch.
#[derive(Default)]
struct FetchStats {
    src: HeightSources,
    relations: usize,
    /// Water elements dropped (underground / culverted / open rings).
    water_skipped: usize,
}

/// Which feature classes one fetch asks OSM for. Buildings and water travel
/// in the SAME per-tile query, so enabling water costs no extra Overpass
/// round trips (and no extra pressure on the mirrors' rate limits).
#[derive(Clone, Copy, Debug)]
pub struct FetchOptions {
    pub buildings: bool,
    pub water: bool,
}

/// Everything one fetch returns.
#[derive(Default)]
pub struct OsmFeatures {
    pub buildings: Vec<Building>,
    pub water: WaterFeatures,
}

// Default widths (m) of linear waterways without a `width` tag.
const RIVER_DEFAULT_WIDTH_M: f32 = 12.0;
const CANAL_DEFAULT_WIDTH_M: f32 = 8.0;
const STREAM_DEFAULT_WIDTH_M: f32 = 2.0;

// A failed tile is split 2×2 and retried, at most this many times
// (4 km² → 1 km² → 0.25 km²). Catches both Overpass stalls on dense tiles and
// the OSM-API 50k-node limit.
const MAX_TILE_SPLIT_DEPTH: u8 = 2;

// ── Concurrency / time budget ─────────────────────────────────────────────────
// Tiles are fetched concurrently, bounded by this many in flight (≈2 first
// attempts per Overpass mirror thanks to per-tile mirror rotation).
const MAX_CONCURRENT_TILES: usize = 6;

// Within a tile the sources are RACED with staggered starts (hedging): mirror 2
// joins HEDGE_DELAY after mirror 1, mirror 3 after that, the OSM direct API
// last. The first success cancels the rest, so a hung mirror costs HEDGE_DELAY
// instead of a full timeout, and a tile's worst case is
// ~(3·HEDGE_DELAY + ATTEMPT_TIMEOUT) instead of the serial sum of every
// fallback timeout (the old N×M blow-up).
const HEDGE_DELAY: Duration = Duration::from_secs(6);
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(25);

// Hard wall-clock budget for the whole building-fetch phase. The frontend
// aborts the request 120 s after submission (frontend/src/services/api.ts), so
// this leaves headroom for elevation, meshing and response transfer. When the
// budget runs out, outstanding tiles fail fast and coverage is reported as
// PARTIAL instead of blowing the frontend deadline.
const OSM_PHASE_BUDGET: Duration = Duration::from_secs(90);

// A failed tile is only split-and-retried when at least this much budget is left.
const MIN_SPLIT_BUDGET: Duration = Duration::from_secs(10);

/// Split a bbox into an `nx` × `ny` grid.
fn split_bbox_grid(bbox: &BoundingBox, nx: usize, ny: usize) -> Vec<BoundingBox> {
    let dlon = (bbox.max_lon - bbox.min_lon) / nx as f64;
    let dlat = (bbox.max_lat - bbox.min_lat) / ny as f64;
    let mut tiles = Vec::with_capacity(nx * ny);
    for iy in 0..ny {
        for ix in 0..nx {
            tiles.push(BoundingBox {
                min_lat: bbox.min_lat + dlat * iy as f64,
                max_lat: bbox.min_lat + dlat * (iy + 1) as f64,
                min_lon: bbox.min_lon + dlon * ix as f64,
                max_lon: bbox.min_lon + dlon * (ix + 1) as f64,
            });
        }
    }
    tiles
}

/// Split a bbox into a grid of tiles no larger than `max_km2` each.
/// Small bboxes come back unchanged (single tile).
fn split_bbox(bbox: &BoundingBox, max_km2: f64) -> Vec<BoundingBox> {
    let lat_km = (bbox.max_lat - bbox.min_lat) * 111.0;
    let lon_km = (bbox.max_lon - bbox.min_lon) * 111.0 * bbox.min_lat.to_radians().cos();
    if lat_km * lon_km <= max_km2 {
        return vec![bbox.clone()];
    }
    let tile_side_km = max_km2.sqrt();
    let nx = (lon_km / tile_side_km).ceil().max(1.0) as usize;
    let ny = (lat_km / tile_side_km).ceil().max(1.0) as usize;
    split_bbox_grid(bbox, nx, ny)
}

/// Stable position of a tile in the fetch grid, surviving split-retries:
/// `top` is the index in the initial grid, `path` the quadrant taken at each
/// split depth (0 = not split, else quadrant+1). The derived `Ord` sorts tiles
/// exactly as the old sequential queue visited them, so merging fetched tiles
/// in key order reproduces the sequential building order — and therefore the
/// same dedupe winner for elements spanning tile borders — no matter in which
/// order the concurrent fetches actually complete.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct TileKey {
    top: usize,
    path: [u8; MAX_TILE_SPLIT_DEPTH as usize],
}

impl TileKey {
    fn root(top: usize) -> Self {
        Self { top, path: [0; MAX_TILE_SPLIT_DEPTH as usize] }
    }

    fn child(mut self, depth: u8, quadrant: usize) -> Self {
        self.path[(depth - 1) as usize] = quadrant as u8 + 1;
        self
    }

    /// Human-readable label, e.g. "3", "3.2", "3.2.4" (1-based).
    fn label(&self) -> String {
        let mut s = (self.top + 1).to_string();
        for &q in self.path.iter().filter(|&&q| q > 0) {
            s.push('.');
            s.push_str(&q.to_string());
        }
        s
    }
}

/// One tile's raw, still-unparsed response plus provenance. Parsing is
/// deferred to the ordered merge so the shared dedupe set stays sequential.
struct FetchedTile {
    source: &'static str,
    element_count: usize,
    data: RawTile,
}

enum RawTile {
    /// Overpass `out geom` response (inline coordinates).
    Geom(Value),
    /// OSM direct API `map.json` response (node-ref format).
    NodeRefs(Value),
}

/// What each concurrent tile task hands back to the coordinator.
type TileTask = (TileKey, u8, BoundingBox, Result<FetchedTile>);

pub struct OsmService {
    client: reqwest::Client,
}

impl OsmService {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .user_agent("geoprint3d-backend/0.1.0")
            .build()
            .unwrap_or_default();
        Self { client }
    }

    /// Fetch building footprints, heights and roof shapes — and/or water
    /// features — from OSM, depending on `opts`.
    ///
    /// Fetched building element classes:
    ///   - `way[building]`                — plain building outlines
    ///   - `way[building:part]`           — Simple 3D Buildings parts (domes, towers,
    ///                                      naves at different heights / min_height)
    ///   - `relation[type=multipolygon]`  — buildings with courtyards (inner rings)
    ///                                      or outlines split across several ways
    ///                                      (e.g. St Peter's colonnade)
    ///
    /// Fetched water element classes (see `classify_element`):
    ///   - `natural=water` ways / multipolygons (+ legacy `waterway=riverbank`,
    ///     `landuse=reservoir`) — lakes, ponds, river and canal areas
    ///   - `waterway=river|canal|stream` ways — linear waterways (flow direction)
    ///   - `natural=coastline` ways — assembled into sea polygons by
    ///     `services::water` (land on the LEFT of the way)
    ///
    /// Strategy: the bbox is split into ≤TILE_MAX_KM2 tiles, ALL fetched
    /// concurrently (bounded by MAX_CONCURRENT_TILES); within a tile the
    /// Overpass mirrors and the OSM direct API are raced with staggered starts
    /// (see HEDGE_DELAY). Raw responses are merged in TileKey order afterwards,
    /// so the result is byte-identical to a sequential fetch. The whole phase
    /// runs under OSM_PHASE_BUDGET.
    /// `min_area_m2` is the print-resolution-aware footprint filter computed by
    /// the handler (floored at MIN_BUILDING_AREA_M2 here).
    pub async fn fetch_features(
        &self,
        bbox: &BoundingBox,
        opts: FetchOptions,
        min_area_m2: f64,
    ) -> Result<OsmFeatures> {
        tracing::info!(
            "   ┌─ OpenStreetMap Feature Service (buildings: {}, water: {})",
            opts.buildings, opts.water
        );
        tracing::info!(
            "   │  Bbox: ({:.4}, {:.4}) → ({:.4}, {:.4})",
            bbox.min_lat, bbox.min_lon, bbox.max_lat, bbox.max_lon
        );
        let min_area_m2 = min_area_m2.max(MIN_BUILDING_AREA_M2);
        tracing::info!("   │  Footprint filter: ≥ {:.0} m² (print-resolution aware)", min_area_m2);

        let request_start = Instant::now();
        let deadline = request_start + OSM_PHASE_BUDGET;
        let tiles = split_bbox(bbox, TILE_MAX_KM2);
        if tiles.len() > 1 {
            tracing::info!(
                "   │  Large area: split into {} tiles of ≤ {} km² (≤{} concurrent, {}s budget)",
                tiles.len(), TILE_MAX_KM2, MAX_CONCURRENT_TILES, OSM_PHASE_BUDGET.as_secs()
            );
        }

        // ── Concurrent fan-out ────────────────────────────────────────────────
        // Every tile is an independent task; a failed tile is quartered and its
        // sub-tiles re-spawned (smaller queries dodge Overpass stalls and the
        // OSM-API node limit) until MAX_TILE_SPLIT_DEPTH or budget exhaustion.
        let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_TILES));
        let mut tasks: JoinSet<TileTask> = JoinSet::new();
        let client = self.client.clone();
        let mut seq = 0usize; // rotates the starting mirror per spawned tile
        let mut spawn_tile = |tasks: &mut JoinSet<TileTask>, key: TileKey, depth: u8, tile: BoundingBox| {
            let client = client.clone();
            let semaphore = semaphore.clone();
            let mirror_seq = seq;
            seq += 1;
            tasks.spawn(async move {
                let _permit = semaphore.acquire_owned().await.expect("semaphore closed");
                let result = fetch_tile_raw(&client, &tile, mirror_seq, deadline, opts).await;
                (key, depth, tile, result)
            });
        };
        for (i, tile) in tiles.into_iter().enumerate() {
            spawn_tile(&mut tasks, TileKey::root(i), 0, tile);
        }

        let mut raw_tiles: Vec<(TileKey, FetchedTile)> = Vec::new();
        let mut failed_tiles = 0usize;
        while let Some(joined) = tasks.join_next().await {
            let Ok((key, depth, tile, result)) = joined else {
                failed_tiles += 1;
                tracing::warn!("   │  Tile task aborted unexpectedly");
                continue;
            };
            match result {
                Ok(fetched) => {
                    tracing::info!(
                        "   │  Tile {} (depth {}): ok via {} — {} raw elements ({:.1}s)",
                        key.label(), depth, fetched.source, fetched.element_count,
                        request_start.elapsed().as_secs_f64()
                    );
                    raw_tiles.push((key, fetched));
                }
                Err(e) => {
                    let time_left = deadline.checked_duration_since(Instant::now());
                    if depth < MAX_TILE_SPLIT_DEPTH && time_left.is_some_and(|t| t >= MIN_SPLIT_BUDGET) {
                        tracing::warn!(
                            "   │  Tile {} (depth {}) failed: {:#} — splitting 2×2 and retrying",
                            key.label(), depth, e
                        );
                        for (q, sub) in split_bbox_grid(&tile, 2, 2).into_iter().enumerate() {
                            spawn_tile(&mut tasks, key.child(depth + 1, q), depth + 1, sub);
                        }
                    } else {
                        failed_tiles += 1;
                        tracing::warn!(
                            "   │  Tile {} (depth {}) FAILED permanently: {:#}",
                            key.label(), depth, e
                        );
                    }
                }
            }
        }

        // ── Ordered merge ─────────────────────────────────────────────────────
        // Elements spanning tile borders are returned by every tile they touch;
        // parsing in TileKey order with one shared (is_relation, id) set makes
        // the dedupe winner deterministic. Geometry is identical in each
        // occurrence anyway, because clipping always uses the FULL request bbox.
        raw_tiles.sort_by_key(|(key, _)| *key);
        let mut seen: HashSet<(bool, u64)> = HashSet::new();
        let mut out = OsmFeatures::default();
        let mut stats = FetchStats::default();
        let mut ok_tiles = 0usize;
        for (key, fetched) in raw_tiles {
            let mut ctx = ParseCtx {
                bbox,
                min_area_m2,
                opts,
                seen: &mut seen,
                out: &mut out,
                stats: &mut stats,
            };
            let parsed = match fetched.data {
                RawTile::Geom(v) => parse_geom_response(v, &mut ctx),
                RawTile::NodeRefs(v) => parse_noderefs_response(v, &mut ctx),
            };
            match parsed {
                Ok(()) => ok_tiles += 1,
                Err(e) => {
                    failed_tiles += 1;
                    tracing::warn!("   │  Tile {} parse FAILED: {:#}", key.label(), e);
                }
            }
        }

        if ok_tiles == 0 && failed_tiles > 0 {
            anyhow::bail!("all OSM tile fetches failed ({} tiles)", failed_tiles);
        }
        if failed_tiles > 0 {
            tracing::warn!(
                "   │  ⚠ {} tile(s) failed permanently — building coverage is PARTIAL",
                failed_tiles
            );
        }

        let mut buildings = std::mem::take(&mut out.buildings);
        if opts.buildings {
            let suppressed = suppress_covered_outlines(&mut buildings);
            log_building_stats(&buildings, &stats.src, stats.relations, suppressed);
        }
        if opts.water {
            log_water_stats(&out.water, stats.water_skipped);
        }
        tracing::info!("   └─ OSM processing time: {:.2}s", request_start.elapsed().as_secs_f64());
        Ok(OsmFeatures { buildings, water: std::mem::take(&mut out.water) })
    }
}

// ── Parsers ───────────────────────────────────────────────────────────────────
// Both parsers route every element through `classify_element`, so the
// Overpass (`out geom`) path and the OSM-direct-API (node-ref) fallback
// accept exactly the same element classes. Keep them in sync.

/// Shared mutable state of the ordered merge (one per fetch).
struct ParseCtx<'a> {
    bbox: &'a BoundingBox,
    min_area_m2: f64,
    opts: FetchOptions,
    seen: &'a mut HashSet<(bool, u64)>,
    out: &'a mut OsmFeatures,
    stats: &'a mut FetchStats,
}

/// What an OSM element contributes to the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ElementClass {
    Building,
    WaterArea(WaterKind),
    Waterway(WaterKind),
    Coastline,
    Skip,
}

/// Route an element by its tags. Buildings win over water tags (a building
/// outline that also carries `natural=water` is still a building). Relations
/// must be multipolygons; waterways and coastlines are ways only.
fn classify_element(tags: &Value, is_relation: bool, opts: FetchOptions) -> ElementClass {
    let tag = |k: &str| tags[k].as_str();
    if is_relation && tag("type") != Some("multipolygon") {
        return ElementClass::Skip;
    }

    let is_building = tag("building").is_some_and(|v| v != "no");
    let is_part = tag("building:part").is_some_and(|v| v != "no");
    if is_building || is_part {
        return if opts.buildings { ElementClass::Building } else { ElementClass::Skip };
    }
    if !opts.water {
        return ElementClass::Skip;
    }

    // Water that is not visible from above (culverts, covered reservoirs,
    // underground channels) is not modelled.
    let hidden = tag("tunnel").is_some_and(|v| v != "no")
        || tag("covered").is_some_and(|v| v != "no")
        || matches!(tag("location"), Some("underground" | "indoor"));

    if !is_relation && tag("natural") == Some("coastline") {
        return ElementClass::Coastline;
    }

    let is_area = tag("natural") == Some("water")
        || tag("waterway") == Some("riverbank")
        || tag("landuse") == Some("reservoir");
    if is_area {
        if hidden {
            return ElementClass::Skip;
        }
        let kind = if tag("waterway") == Some("riverbank") {
            WaterKind::River
        } else {
            match tag("water") {
                Some("river" | "stream" | "rapids") => WaterKind::River,
                Some("canal" | "ditch" | "drain") => WaterKind::Canal,
                _ => WaterKind::Lake, // lake, pond, reservoir, basin, oxbow, lagoon…
            }
        };
        return ElementClass::WaterArea(kind);
    }

    if !is_relation {
        let kind = match tag("waterway") {
            Some("river") => Some(WaterKind::River),
            Some("canal") => Some(WaterKind::Canal),
            Some("stream") => Some(WaterKind::Stream),
            _ => None,
        };
        if let Some(kind) = kind {
            // Seasonal streams are dry most of the year; seasonal rivers keep
            // a visible bed and stay.
            let dry_stream = kind == WaterKind::Stream && tag("intermittent") == Some("yes");
            if hidden || dry_stream {
                return ElementClass::Skip;
            }
            return ElementClass::Waterway(kind);
        }
    }

    ElementClass::Skip
}

/// Parse an Overpass `out geom` response.
/// Ways carry a `geometry` array with inline lat/lon objects; relations carry
/// per-member geometry with `outer`/`inner` roles.
fn parse_geom_response(data: Value, ctx: &mut ParseCtx) -> Result<()> {
    let elements = data["elements"]
        .as_array()
        .context("Missing 'elements' in Overpass response")?;

    tracing::debug!("   │  Parsing {} OSM elements", elements.len());

    let coords_of = |geom: &Vec<Value>| -> Vec<(f64, f64)> {
        geom.iter()
            .filter_map(|g| Some((g["lon"].as_f64()?, g["lat"].as_f64()?)))
            .collect()
    };

    for el in elements.iter() {
        let tags = &el["tags"];
        let id = el["id"].as_u64().unwrap_or(0);

        match el["type"].as_str().unwrap_or("") {
            "way" => {
                let class = classify_element(tags, false, ctx.opts);
                if class == ElementClass::Skip {
                    continue;
                }
                if !ctx.seen.insert((false, id)) {
                    continue; // already parsed via a neighbouring tile
                }
                let Some(geometry) = el["geometry"].as_array() else {
                    ctx.stats.src.skipped += 1;
                    continue;
                };
                push_way(ctx, class, id, coords_of(geometry), tags);
            }
            "relation" => {
                let class = classify_element(tags, true, ctx.opts);
                if class == ElementClass::Skip {
                    continue;
                }
                if !ctx.seen.insert((true, id)) {
                    continue;
                }
                let Some(members) = el["members"].as_array() else {
                    continue;
                };
                let mut outer_segments: Vec<Vec<(f64, f64)>> = Vec::new();
                let mut inner_segments: Vec<Vec<(f64, f64)>> = Vec::new();
                for m in members {
                    let Some(geom) = m["geometry"].as_array() else {
                        continue;
                    };
                    let line = coords_of(geom);
                    if line.len() < 2 {
                        continue;
                    }
                    if m["role"].as_str() == Some("inner") {
                        inner_segments.push(line);
                    } else {
                        outer_segments.push(line);
                    }
                }
                push_multipolygon(ctx, class, id, outer_segments, inner_segments, tags);
            }
            _ => {}
        }
    }

    Ok(())
}

/// Parse an OSM direct API response (node-ref format).
/// Ways reference node IDs; relations reference way IDs. Coordinates are
/// resolved through the node list. Note: the map call does not return the
/// members of a relation that lie outside the bbox, so large water
/// multipolygons can arrive incomplete — their open rings are dropped by
/// `assemble_rings` (partial coverage, never wrong geometry).
fn parse_noderefs_response(data: Value, ctx: &mut ParseCtx) -> Result<()> {
    let elements = data["elements"]
        .as_array()
        .context("Missing 'elements' in OSM API response")?;

    tracing::debug!("   │  Parsing {} total OSM elements", elements.len());

    // Build node-id → (lon, lat) map
    let mut nodes: HashMap<u64, (f64, f64)> = HashMap::new();
    for el in elements.iter() {
        if el["type"] == "node" {
            if let (Some(id), Some(lat), Some(lon)) =
                (el["id"].as_u64(), el["lat"].as_f64(), el["lon"].as_f64())
            {
                nodes.insert(id, (lon, lat));
            }
        }
    }

    // Build way-id → coordinate-list map (all ways: multipolygon members
    // usually carry no building tag themselves).
    let mut ways: HashMap<u64, Vec<(f64, f64)>> = HashMap::new();
    for el in elements.iter() {
        if el["type"] != "way" {
            continue;
        }
        let (Some(id), Some(node_refs)) = (el["id"].as_u64(), el["nodes"].as_array()) else {
            continue;
        };
        let coords: Vec<(f64, f64)> = node_refs
            .iter()
            .filter_map(|r| r.as_u64())
            .filter_map(|nid| nodes.get(&nid).copied())
            .collect();
        ways.insert(id, coords);
    }

    for el in elements.iter() {
        let tags = &el["tags"];
        let id = el["id"].as_u64().unwrap_or(0);

        match el["type"].as_str().unwrap_or("") {
            "way" => {
                let class = classify_element(tags, false, ctx.opts);
                if class == ElementClass::Skip {
                    continue;
                }
                if !ctx.seen.insert((false, id)) {
                    continue;
                }
                let coords = ways.get(&id).cloned().unwrap_or_default();
                if coords.len() < 2 {
                    ctx.stats.src.skipped += 1;
                    continue;
                }
                push_way(ctx, class, id, coords, tags);
            }
            "relation" => {
                let class = classify_element(tags, true, ctx.opts);
                if class == ElementClass::Skip {
                    continue;
                }
                if !ctx.seen.insert((true, id)) {
                    continue;
                }
                let Some(members) = el["members"].as_array() else {
                    continue;
                };
                let mut outer_segments: Vec<Vec<(f64, f64)>> = Vec::new();
                let mut inner_segments: Vec<Vec<(f64, f64)>> = Vec::new();
                for m in members {
                    if m["type"] != "way" {
                        continue;
                    }
                    let Some(coords) = m["ref"].as_u64().and_then(|r| ways.get(&r)).cloned() else {
                        continue;
                    };
                    if coords.len() < 2 {
                        continue;
                    }
                    if m["role"].as_str() == Some("inner") {
                        inner_segments.push(coords);
                    } else {
                        outer_segments.push(coords);
                    }
                }
                push_multipolygon(ctx, class, id, outer_segments, inner_segments, tags);
            }
            _ => {}
        }
    }

    Ok(())
}

/// Dispatch one parsed way by class.
fn push_way(ctx: &mut ParseCtx, class: ElementClass, id: u64, coords: Vec<(f64, f64)>, tags: &Value) {
    match class {
        ElementClass::Building => {
            if coords.len() < 3 {
                ctx.stats.src.skipped += 1;
                return;
            }
            push_building(
                &mut ctx.out.buildings, id, coords, Vec::new(), tags,
                ctx.bbox, ctx.min_area_m2, &mut ctx.stats.src,
            );
        }
        ElementClass::WaterArea(kind) => match closed_ring(coords) {
            Some(outer) => ctx.out.water.areas.push(WaterArea { id, kind, outer, holes: Vec::new() }),
            // Open water ways are usually multipolygon members that also
            // carry tags; the relation provides the area.
            None => ctx.stats.water_skipped += 1,
        },
        ElementClass::Waterway(kind) => {
            let default_width = match kind {
                WaterKind::River => RIVER_DEFAULT_WIDTH_M,
                WaterKind::Canal => CANAL_DEFAULT_WIDTH_M,
                _ => STREAM_DEFAULT_WIDTH_M,
            };
            let width_m = tags["width"]
                .as_str()
                .and_then(parse_meters)
                .filter(|w| *w > 0.0 && *w < 2000.0)
                .unwrap_or(default_width);
            ctx.out.water.waterways.push(Waterway { id, kind, line: coords, width_m });
        }
        ElementClass::Coastline => ctx.out.water.coastlines.push((id, coords)),
        ElementClass::Skip => {}
    }
}

/// Dispatch one parsed multipolygon relation by class.
fn push_multipolygon(
    ctx: &mut ParseCtx,
    class: ElementClass,
    id: u64,
    outer_segments: Vec<Vec<(f64, f64)>>,
    inner_segments: Vec<Vec<(f64, f64)>>,
    tags: &Value,
) {
    match class {
        ElementClass::Building => {
            ctx.stats.relations += 1;
            push_relation(
                &mut ctx.out.buildings, id, outer_segments, inner_segments, tags,
                ctx.bbox, ctx.min_area_m2, &mut ctx.stats.src,
            );
        }
        ElementClass::WaterArea(kind) => {
            // No bbox clipping here: hydro clips in metres against the
            // terrain hull, and a lake's shore outside the bbox is irrelevant.
            let outer_rings = assemble_rings(outer_segments);
            let inner_rings = assemble_rings(inner_segments);
            if outer_rings.is_empty() {
                ctx.stats.water_skipped += 1;
            }
            for outer in outer_rings {
                let holes: Vec<Vec<(f64, f64)>> = inner_rings
                    .iter()
                    .filter(|h| !h.is_empty() && point_in_ring(h[0], &outer))
                    .cloned()
                    .collect();
                ctx.out.water.areas.push(WaterArea { id, kind, outer, holes });
            }
        }
        _ => {}
    }
}

/// A closed way's ring without the repeated closing node; `None` if the way
/// is not closed (or too short to enclose anything).
fn closed_ring(mut coords: Vec<(f64, f64)>) -> Option<Vec<(f64, f64)>> {
    const EPS: f64 = 1e-9;
    if coords.len() < 4 {
        return None;
    }
    let (f, l) = (coords[0], *coords.last().unwrap());
    if (f.0 - l.0).abs() > EPS || (f.1 - l.1).abs() > EPS {
        return None;
    }
    coords.pop();
    Some(coords)
}

// ── Tile fetching (hedged race) ───────────────────────────────────────────────

/// Fetch one tile's raw OSM data, racing the sources with staggered starts:
/// Overpass mirror k joins the race k·HEDGE_DELAY in (a healthy first mirror
/// finishes alone before the second even sends), the OSM direct API joins
/// last. First success wins — dropping the JoinSet aborts the losers, so late
/// hedges that never fired cost nothing. `seq` rotates the mirror order per
/// tile so concurrent tiles spread their first attempts across mirrors.
async fn fetch_tile_raw(
    client: &reqwest::Client,
    tile: &BoundingBox,
    seq: usize,
    deadline: Instant,
    opts: FetchOptions,
) -> Result<FetchedTile> {
    let overpass_query = overpass_query(tile, opts);

    let n_mirrors = OVERPASS_ENDPOINTS.len();
    let mut attempts: JoinSet<Result<FetchedTile>> = JoinSet::new();
    for k in 0..n_mirrors {
        let endpoint = OVERPASS_ENDPOINTS[(seq + k) % n_mirrors];
        attempts.spawn(overpass_attempt(
            client.clone(),
            endpoint,
            overpass_query.clone(),
            HEDGE_DELAY * k as u32,
            deadline,
        ));
    }
    attempts.spawn(direct_api_attempt(
        client.clone(),
        tile.clone(),
        HEDGE_DELAY * n_mirrors as u32,
        deadline,
    ));

    let mut errors: Vec<String> = Vec::new();
    while let Some(joined) = attempts.join_next().await {
        match joined {
            Ok(Ok(fetched)) => return Ok(fetched),
            Ok(Err(e)) => errors.push(format!("{e:#}")),
            Err(e) => errors.push(format!("attempt panicked: {e}")),
        }
    }
    anyhow::bail!("all sources failed: {}", errors.join(" | "))
}

/// Build one tile's Overpass union query for the requested feature classes.
/// `out geom qt` returns coordinates inline — ~10× smaller than
/// `out body;>;out skel qt` because no separate node objects are emitted.
/// Relations include per-member geometry and roles (outer/inner).
fn overpass_query(tile: &BoundingBox, opts: FetchOptions) -> String {
    let bb = format!(
        "{},{},{},{}",
        tile.min_lat, tile.min_lon, tile.max_lat, tile.max_lon
    );
    let mut q = String::from("[out:json][timeout:20];(");
    if opts.buildings {
        q.push_str(&format!(
            "way[\"building\"][\"building\"!=\"no\"]({bb});\
             way[\"building:part\"][\"building:part\"!=\"no\"]({bb});\
             relation[\"type\"=\"multipolygon\"][\"building\"][\"building\"!=\"no\"]({bb});\
             relation[\"type\"=\"multipolygon\"][\"building:part\"][\"building:part\"!=\"no\"]({bb});"
        ));
    }
    if opts.water {
        q.push_str(&format!(
            "way[\"natural\"=\"water\"]({bb});\
             relation[\"type\"=\"multipolygon\"][\"natural\"=\"water\"]({bb});\
             way[\"waterway\"=\"riverbank\"]({bb});\
             relation[\"type\"=\"multipolygon\"][\"waterway\"=\"riverbank\"]({bb});\
             way[\"landuse\"=\"reservoir\"]({bb});\
             relation[\"type\"=\"multipolygon\"][\"landuse\"=\"reservoir\"]({bb});\
             way[\"waterway\"~\"^(river|canal|stream)$\"]({bb});\
             way[\"natural\"=\"coastline\"]({bb});"
        ));
    }
    q.push_str(");out geom qt;");
    q
}

/// One delayed Overpass request (the delay implements the hedge stagger).
/// The HTTP timeout is clamped to the remaining phase budget so no attempt
/// outlives the deadline.
async fn overpass_attempt(
    client: reqwest::Client,
    endpoint: &'static str,
    query: String,
    delay: Duration,
    deadline: Instant,
) -> Result<FetchedTile> {
    tokio::time::sleep(delay).await;
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .context("OSM phase budget exhausted")?;

    let resp = client
        .post(endpoint)
        .form(&[("data", &query)])
        .timeout(ATTEMPT_TIMEOUT.min(remaining))
        .send()
        .await
        .with_context(|| format!("{endpoint}: request failed"))?;

    let status = resp.status();
    if !status.is_success() {
        // 429/406 = rate-limited or rejected; anything else is a server error.
        anyhow::bail!("{}: HTTP {}", endpoint, status);
    }
    let data: Value = resp
        .json()
        .await
        .with_context(|| format!("{endpoint}: invalid JSON"))?;
    // Overpass reports server-side timeout/OOM as 200 + remark + a TRUNCATED
    // element list — accepting it would silently punch holes in the coverage,
    // so treat it as a failure (the tile gets hedged elsewhere or split).
    if let Some(remark) = data["remark"].as_str() {
        if remark.contains("timed out") || remark.contains("out of memory") {
            anyhow::bail!("{}: {}", endpoint, remark);
        }
    }
    let element_count = data["elements"]
        .as_array()
        .map(|a| a.len())
        .with_context(|| format!("{endpoint}: missing 'elements'"))?;
    Ok(FetchedTile {
        source: endpoint,
        element_count,
        data: RawTile::Geom(data),
    })
}

/// The OSM direct API as the final hedge: OSMF-maintained and far more
/// available than community Overpass mirrors, but it returns EVERY element in
/// the bbox (node-ref format) and rejects tiles above 50 000 nodes.
/// Note: bbox order is west,south,east,north (lon,lat,lon,lat).
async fn direct_api_attempt(
    client: reqwest::Client,
    tile: BoundingBox,
    delay: Duration,
    deadline: Instant,
) -> Result<FetchedTile> {
    tokio::time::sleep(delay).await;
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .context("OSM phase budget exhausted")?;

    let url = format!(
        "https://api.openstreetmap.org/api/0.6/map.json?bbox={},{},{},{}",
        tile.min_lon, tile.min_lat, tile.max_lon, tile.max_lat
    );
    let resp = client
        .get(&url)
        .timeout(ATTEMPT_TIMEOUT.min(remaining))
        .send()
        .await
        .context("OSM API: request failed")?;

    let status = resp.status();
    if status.as_u16() == 509 {
        anyhow::bail!("OSM API: tile too dense (> 50 000 nodes)");
    }
    if !status.is_success() {
        anyhow::bail!("OSM API: HTTP {}", status);
    }
    let data: Value = resp.json().await.context("OSM API: invalid JSON")?;
    let element_count = data["elements"]
        .as_array()
        .map(|a| a.len())
        .context("OSM API: missing 'elements'")?;
    Ok(FetchedTile {
        source: "OSM direct API",
        element_count,
        data: RawTile::NodeRefs(data),
    })
}

// ── Element construction ──────────────────────────────────────────────────────

/// Validate a footprint and append a `Building` with tags resolved.
/// Footprints are clipped to the request bbox first: OSM returns the COMPLETE
/// geometry of any element intersecting the bbox, and unclipped giants (city
/// walls, palace complexes) would stretch the mesh beyond the terrain tile and
/// shrink the print scale for everything else.
fn push_building(
    buildings: &mut Vec<Building>,
    id: u64,
    footprint: Vec<(f64, f64)>,
    holes: Vec<Vec<(f64, f64)>>,
    tags: &Value,
    bbox: &BoundingBox,
    min_area_m2: f64,
    src: &mut HeightSources,
) {
    // Inference (roof shape from a quad footprint) looks at the original
    // corner count, not the clip-induced one.
    let corners = corner_count(&footprint);
    let footprint = clip_ring_to_bbox(&footprint, bbox);
    if footprint.len() < 3 || footprint_area_m2(&footprint) < min_area_m2 {
        src.skipped += 1;
        return;
    }
    let holes: Vec<Vec<(f64, f64)>> = holes
        .into_iter()
        .map(|h| clip_ring_to_bbox(&h, bbox))
        .filter(|h| h.len() >= 3)
        .collect();
    let parsed = parse_building_tags(tags, id, corners, src);
    buildings.push(Building {
        id,
        footprint,
        holes,
        height: parsed.height,
        min_height: parsed.min_height,
        roof_shape: parsed.roof_shape,
        roof_height: parsed.roof_height,
        is_part: parsed.is_part,
        kind: parsed.kind,
        height_source: parsed.height_source,
    });
}

/// Sutherland–Hodgman clip of a polygon ring against the request bbox
/// (the clip region is convex, so the result is a single ring; polygons fully
/// outside come back empty).
fn clip_ring_to_bbox(ring: &[(f64, f64)], bbox: &BoundingBox) -> Vec<(f64, f64)> {
    let mut pts = ring.to_vec();
    for edge in 0..4 {
        let inside = |p: (f64, f64)| match edge {
            0 => p.0 >= bbox.min_lon,
            1 => p.0 <= bbox.max_lon,
            2 => p.1 >= bbox.min_lat,
            _ => p.1 <= bbox.max_lat,
        };
        let intersect = |a: (f64, f64), b: (f64, f64)| -> (f64, f64) {
            match edge {
                0 => {
                    let t = (bbox.min_lon - a.0) / (b.0 - a.0);
                    (bbox.min_lon, a.1 + t * (b.1 - a.1))
                }
                1 => {
                    let t = (bbox.max_lon - a.0) / (b.0 - a.0);
                    (bbox.max_lon, a.1 + t * (b.1 - a.1))
                }
                2 => {
                    let t = (bbox.min_lat - a.1) / (b.1 - a.1);
                    (a.0 + t * (b.0 - a.0), bbox.min_lat)
                }
                _ => {
                    let t = (bbox.max_lat - a.1) / (b.1 - a.1);
                    (a.0 + t * (b.0 - a.0), bbox.max_lat)
                }
            }
        };
        let input = pts;
        pts = Vec::new();
        let n = input.len();
        if n == 0 {
            return pts;
        }
        for i in 0..n {
            let cur = input[i];
            let prev = input[(i + n - 1) % n];
            match (inside(prev), inside(cur)) {
                (true, true) => pts.push(cur),
                (true, false) => pts.push(intersect(prev, cur)),
                (false, true) => {
                    pts.push(intersect(prev, cur));
                    pts.push(cur);
                }
                (false, false) => {}
            }
        }
    }
    pts
}

/// Assemble a multipolygon relation's member segments into closed rings and
/// emit one `Building` per outer ring, with the inner rings it contains as holes.
fn push_relation(
    buildings: &mut Vec<Building>,
    id: u64,
    outer_segments: Vec<Vec<(f64, f64)>>,
    inner_segments: Vec<Vec<(f64, f64)>>,
    tags: &Value,
    bbox: &BoundingBox,
    min_area_m2: f64,
    src: &mut HeightSources,
) {
    let outer_rings = assemble_rings(outer_segments);
    let inner_rings = assemble_rings(inner_segments);
    for ring in outer_rings {
        let holes: Vec<Vec<(f64, f64)>> = inner_rings
            .iter()
            .filter(|h| !h.is_empty() && point_in_ring(h[0], &ring))
            .cloned()
            .collect();
        push_building(buildings, id, ring, holes, tags, bbox, min_area_m2, src);
    }
}

/// Stitch way segments into closed rings by matching endpoints.
/// Multipolygon outlines are routinely split across several ways (e.g. each
/// colonnade crescent is its own way); unclosed leftovers are dropped.
fn assemble_rings(mut segments: Vec<Vec<(f64, f64)>>) -> Vec<Vec<(f64, f64)>> {
    // OSM members share exact node coordinates, so a tiny epsilon suffices.
    const EPS: f64 = 1e-9;
    let near = |a: (f64, f64), b: (f64, f64)| (a.0 - b.0).abs() < EPS && (a.1 - b.1).abs() < EPS;

    let mut rings = Vec::new();
    while let Some(mut current) = segments.pop() {
        loop {
            let first = current[0];
            let last = *current.last().unwrap();
            if current.len() >= 4 && near(first, last) {
                current.pop(); // drop the closing duplicate
                rings.push(current);
                break;
            }
            // Find a segment that continues from `last` (either direction).
            let mut found = None;
            for (i, seg) in segments.iter().enumerate() {
                if near(seg[0], last) {
                    found = Some((i, false));
                    break;
                }
                if near(*seg.last().unwrap(), last) {
                    found = Some((i, true));
                    break;
                }
            }
            match found {
                Some((i, reversed)) => {
                    let mut seg = segments.remove(i);
                    if reversed {
                        seg.reverse();
                    }
                    current.extend(seg.into_iter().skip(1));
                }
                None => break, // open ring — incomplete data, drop it
            }
        }
    }
    rings
}

/// Ray-casting point-in-polygon test (lon/lat treated as planar — fine at city scale).
fn point_in_ring(p: (f64, f64), ring: &[(f64, f64)]) -> bool {
    let (px, py) = p;
    let mut inside = false;
    let n = ring.len();
    for i in 0..n {
        let (x0, y0) = ring[i];
        let (x1, y1) = ring[(i + 1) % n];
        if (y0 > py) != (y1 > py) {
            let x_cross = x0 + (py - y0) / (y1 - y0) * (x1 - x0);
            if px < x_cross {
                inside = !inside;
            }
        }
    }
    inside
}

/// Drop building outlines whose footprint is substantially covered by
/// ground-level `building:part` elements: per Simple 3D Buildings the parts
/// carry the real shape (dome, drum, nave…) and extruding the outline as well
/// would bury them in a solid block.
fn suppress_covered_outlines(buildings: &mut Vec<Building>) -> usize {
    // (bbox, footprint, area) of every ground-level part. Parts with
    // min_height > 0 sit on other parts and don't contribute ground coverage.
    let ground_parts: Vec<(BBox2, Vec<(f64, f64)>, f64)> = buildings
        .iter()
        .filter(|b| b.is_part && b.min_height < 0.5)
        .map(|b| (BBox2::of(&b.footprint), b.footprint.clone(), footprint_area_m2(&b.footprint)))
        .collect();
    if ground_parts.is_empty() {
        return 0;
    }

    let before = buildings.len();
    buildings.retain(|b| {
        if b.is_part {
            return true;
        }
        let outline_bbox = BBox2::of(&b.footprint);
        let outline_area = footprint_area_m2(&b.footprint);
        let mut covered_m2 = 0.0;
        for (part_bbox, part_fp, part_area) in &ground_parts {
            if !outline_bbox.intersects(part_bbox) {
                continue;
            }
            // Part counts as inside when most of its vertices fall in the outline
            // (strict majority, so shared-wall touches with neighbours don't count).
            let inside = part_fp.iter().filter(|&&p| point_in_ring(p, &b.footprint)).count();
            if inside * 2 > part_fp.len() {
                covered_m2 += part_area;
            }
        }
        covered_m2 < PART_COVERAGE_SUPPRESS_FRAC * outline_area
    });
    before - buildings.len()
}

#[derive(Clone, Copy)]
struct BBox2 {
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
}

impl BBox2 {
    fn of(pts: &[(f64, f64)]) -> Self {
        let mut b = BBox2 { min_x: f64::MAX, min_y: f64::MAX, max_x: f64::MIN, max_y: f64::MIN };
        for &(x, y) in pts {
            b.min_x = b.min_x.min(x);
            b.min_y = b.min_y.min(y);
            b.max_x = b.max_x.max(x);
            b.max_y = b.max_y.max(y);
        }
        b
    }

    fn intersects(&self, other: &BBox2) -> bool {
        self.min_x <= other.max_x
            && self.max_x >= other.min_x
            && self.min_y <= other.max_y
            && self.max_y >= other.min_y
    }
}

// ── Tag parsing ───────────────────────────────────────────────────────────────

struct ParsedTags {
    height: f32,
    min_height: f32,
    roof_shape: RoofShape,
    roof_height: f32,
    is_part: bool,
    kind: String,
    height_source: HeightSource,
}

/// Resolve height, vertical extent and roof information from OSM tags.
/// `corner_count` is the number of distinct footprint vertices — used to limit
/// roof-shape inference to simple quad footprints.
fn parse_building_tags(tags: &Value, id: u64, corner_count: usize, src: &mut HeightSources) -> ParsedTags {
    let is_building = tags["building"].is_string() && tags["building"] != "no";
    let is_part = !is_building && tags["building:part"].is_string();

    // Roof shape: explicit tag, otherwise inferred gabled for simple rectangular
    // houses (untagged flat roofs on detached housing are almost always wrong).
    let mut roof_shape = tags["roof:shape"]
        .as_str()
        .map(RoofShape::from_tag)
        .unwrap_or(RoofShape::Flat);
    if roof_shape == RoofShape::Flat && !tags["roof:shape"].is_string() && corner_count == 4 {
        let kind = if is_part {
            tags["building:part"].as_str()
        } else {
            tags["building"].as_str()
        };
        if matches!(
            kind,
            Some("house" | "detached" | "semidetached_house" | "bungalow" | "cabin" | "hut" | "farmhouse")
        ) {
            roof_shape = RoofShape::Gabled;
        }
    }

    // Roof height in metres: explicit tag, or storey count; 0 = auto (mesh picks
    // a plausible value from the footprint size).
    let roof_height = tags["roof:height"]
        .as_str()
        .and_then(parse_meters)
        .or_else(|| {
            tags["roof:levels"]
                .as_str()
                .and_then(|s| s.trim().parse::<f32>().ok())
                .map(|l| l * 3.2)
        })
        .unwrap_or(0.0)
        .max(0.0);

    // Bottom of the solid: building:part elements stack via min_height.
    let min_height = tags["min_height"]
        .as_str()
        .and_then(parse_meters)
        .or_else(|| {
            tags["building:min_level"]
                .as_str()
                .and_then(|s| s.trim().parse::<f32>().ok())
                .map(|l| l * 3.2)
        })
        .unwrap_or(0.0)
        .max(0.0);

    // Total height (top of the solid above ground, roof included).
    let kind_tag = if is_part { &tags["building:part"] } else { &tags["building"] };
    let kind = kind_tag.as_str().unwrap_or("yes").to_string();
    let (height, height_source) = building_height(tags, &kind, id, src);
    let height = height.max(min_height + 0.5);

    ParsedTags { height, min_height, roof_shape, roof_height, is_part, kind, height_source }
}

/// Resolve a building height in metres from OSM tags, with its provenance.
/// Priority: explicit `height`/`est_height` → `building:levels` (+roof) → type-aware default.
/// Defaulted heights are only a placeholder for most outlines: the handler
/// replaces them with contextual estimates (services::height_inference).
fn building_height(tags: &Value, kind: &str, id: u64, src: &mut HeightSources) -> (f32, HeightSource) {
    // 1. Explicit height in metres (tolerant of "12 m", "12.5", ranges "10-20").
    if let Some(h) = tags["height"].as_str().and_then(parse_meters) {
        src.tag += 1;
        return (h.max(1.0), HeightSource::Tag);
    }
    if let Some(h) = tags["est_height"].as_str().and_then(parse_meters) {
        src.tag += 1;
        return (h.max(1.0), HeightSource::Tag);
    }

    // 2. Storey count → metres (~3.2 m/floor), adding roof levels when tagged.
    if let Some(l) = tags["building:levels"].as_str().and_then(|s| s.trim().parse::<f32>().ok()) {
        src.levels += 1;
        let roof = tags["roof:levels"]
            .as_str()
            .and_then(|s| s.trim().parse::<f32>().ok())
            .unwrap_or(0.0);
        return (((l + roof) * 3.2).max(2.5), HeightSource::Levels);
    }

    // 3. Type-aware default with deterministic per-building variation.
    src.default += 1;
    (default_height_for_type(kind) * jitter(id), HeightSource::TypeDefault)
}

/// Parse a metric height value, tolerating a trailing unit and "a-b" ranges (→ midpoint).
fn parse_meters(s: &str) -> Option<f32> {
    let s = s.trim().trim_end_matches(['m', ' ']).trim();
    if let Some((a, b)) = s.split_once('-') {
        if let (Ok(a), Ok(b)) = (a.trim().parse::<f32>(), b.trim().parse::<f32>()) {
            return Some((a + b) / 2.0);
        }
    }
    s.parse::<f32>().ok()
}

/// Plausible default height (m) by OSM `building=` type, so missing-data areas
/// keep relative proportions (a house is not as tall as a cathedral).
pub(crate) fn default_height_for_type(kind: &str) -> f32 {
    match kind {
        "house" | "detached" | "bungalow" | "cabin" | "hut" | "static_caravan" => 6.0,
        "garage" | "garages" | "shed" | "carport" | "roof" | "kiosk" => 3.0,
        "apartments" | "residential" | "dormitory" | "terrace" => 15.0,
        "commercial" | "office" | "hotel" => 14.0,
        "retail" | "supermarket" | "warehouse" => 8.0,
        "industrial" | "manufacture" | "hangar" => 9.0,
        "church" | "cathedral" | "mosque" | "temple" | "chapel" | "basilica" => 18.0,
        "school" | "university" | "college" | "hospital" | "public" | "civic" | "government" => 12.0,
        "tower" | "skyscraper" => 40.0,
        "colonnade" => 16.0,
        _ => 8.0,
    }
}

/// Deterministic ±~12% multiplier seeded by OSM id (stable across runs).
fn jitter(id: u64) -> f32 {
    let h = (id.wrapping_mul(2_654_435_761) % 1000) as f32 / 1000.0; // [0,1)
    0.88 + 0.24 * h
}

/// Number of distinct corners in a footprint (ignores the closing duplicate node).
fn corner_count(footprint: &[(f64, f64)]) -> usize {
    let n = footprint.len();
    if n >= 2 {
        let (f, l) = (footprint[0], footprint[n - 1]);
        if (f.0 - l.0).abs() < 1e-9 && (f.1 - l.1).abs() < 1e-9 {
            return n - 1;
        }
    }
    n
}

fn log_water_stats(water: &WaterFeatures, skipped: usize) {
    let mut by_kind: std::collections::BTreeMap<&'static str, usize> = std::collections::BTreeMap::new();
    for a in &water.areas {
        *by_kind.entry(a.kind.name()).or_insert(0) += 1;
    }
    let mut lines: std::collections::BTreeMap<&'static str, usize> = std::collections::BTreeMap::new();
    for w in &water.waterways {
        *lines.entry(w.kind.name()).or_insert(0) += 1;
    }
    tracing::info!(
        "   │  💧 Water: {} area(s) {:?}, {} waterway(s) {:?}, {} coastline way(s)",
        water.areas.len(), by_kind, water.waterways.len(), lines, water.coastlines.len()
    );
    if skipped > 0 {
        tracing::debug!("   │  Skipped {} water element(s) (open rings)", skipped);
    }
}

fn log_building_stats(
    buildings: &[Building],
    src: &HeightSources,
    relations: usize,
    suppressed: usize,
) {
    if buildings.is_empty() {
        tracing::info!("   │  ⚠ No printable buildings found");
    } else {
        let heights: Vec<f32> = buildings.iter().map(|b| b.height).collect();
        let min_h = heights.iter().cloned().fold(f32::INFINITY, f32::min);
        let max_h = heights.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        let avg_h: f32 = heights.iter().sum::<f32>() / heights.len() as f32;
        let parts = buildings.iter().filter(|b| b.is_part).count();
        let with_holes = buildings.iter().filter(|b| !b.holes.is_empty()).count();
        let shaped_roofs = buildings.iter().filter(|b| b.roof_shape != RoofShape::Flat).count();
        tracing::info!("   │  ✓ {} printable elements ({} building:part)", buildings.len(), parts);
        tracing::info!(
            "   │  Multipolygons: {} relations, {} with courtyards; outlines replaced by parts: {}",
            relations, with_holes, suppressed
        );
        tracing::info!("   │  Roofs: {} non-flat (tagged or inferred)", shaped_roofs);
        tracing::info!("   │  Height sources: tag={} levels={} default={}", src.tag, src.levels, src.default);
        tracing::info!("   │  Heights: min={:.0}m max={:.0}m avg={:.0}m", min_h, max_h, avg_h);
    }
    if src.skipped > 0 {
        tracing::debug!("   │  Skipped {} elements (no geometry / too small)", src.skipped);
    }
}

/// Shoelace formula for polygon area in m² on a (lon, lat) ring.
/// Longitude degrees shrink by cos(lat); accurate enough for filtering and
/// density estimates, not for precise measurements.
pub fn footprint_area_m2(pts: &[(f64, f64)]) -> f64 {
    let n = pts.len();
    if n < 3 {
        return 0.0;
    }
    let mut area = 0.0_f64;
    for i in 0..n {
        let (x0, y0) = pts[i];
        let (x1, y1) = pts[(i + 1) % n];
        area += x0 * y1 - x1 * y0;
    }
    let mean_lat = pts.iter().map(|p| p.1).sum::<f64>() / n as f64;
    (area.abs() / 2.0) * 111_320.0 * 111_320.0 * mean_lat.to_radians().cos()
}

impl Default for OsmService {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ALL: FetchOptions = FetchOptions { buildings: true, water: true };

    #[test]
    fn classify_routes_by_tags() {
        let c = |t: Value, rel: bool| classify_element(&t, rel, ALL);
        assert_eq!(c(json!({"building": "yes"}), false), ElementClass::Building);
        assert_eq!(c(json!({"natural": "water"}), false), ElementClass::WaterArea(WaterKind::Lake));
        assert_eq!(c(json!({"natural": "water", "water": "river"}), false), ElementClass::WaterArea(WaterKind::River));
        assert_eq!(c(json!({"waterway": "riverbank"}), false), ElementClass::WaterArea(WaterKind::River));
        assert_eq!(c(json!({"landuse": "reservoir"}), false), ElementClass::WaterArea(WaterKind::Lake));
        assert_eq!(
            c(json!({"type": "multipolygon", "natural": "water", "water": "canal"}), true),
            ElementClass::WaterArea(WaterKind::Canal)
        );
        assert_eq!(c(json!({"natural": "water"}), true), ElementClass::Skip, "relation must be a multipolygon");
        assert_eq!(c(json!({"natural": "water", "covered": "yes"}), false), ElementClass::Skip);
        assert_eq!(c(json!({"waterway": "river"}), false), ElementClass::Waterway(WaterKind::River));
        assert_eq!(c(json!({"waterway": "stream", "tunnel": "culvert"}), false), ElementClass::Skip);
        assert_eq!(c(json!({"waterway": "stream", "intermittent": "yes"}), false), ElementClass::Skip);
        assert_eq!(
            c(json!({"waterway": "river", "intermittent": "yes"}), false),
            ElementClass::Waterway(WaterKind::River),
            "seasonal rivers keep their bed"
        );
        assert_eq!(c(json!({"waterway": "ditch"}), false), ElementClass::Skip);
        assert_eq!(c(json!({"natural": "coastline"}), false), ElementClass::Coastline);
        // Buildings win over water tags; disabled classes are skipped.
        assert_eq!(c(json!({"building": "yes", "natural": "water"}), false), ElementClass::Building);
        let water_only = FetchOptions { buildings: false, water: true };
        assert_eq!(classify_element(&json!({"building": "yes"}), false, water_only), ElementClass::Skip);
        let buildings_only = FetchOptions { buildings: true, water: false };
        assert_eq!(classify_element(&json!({"natural": "water"}), false, buildings_only), ElementClass::Skip);
    }

    fn bbox() -> BoundingBox {
        BoundingBox { min_lat: 44.49, min_lon: 11.33, max_lat: 44.50, max_lon: 11.345 }
    }

    // A ~40 × 40 m square at (lon0, lat0), closed.
    fn square(lon0: f64, lat0: f64) -> Vec<(f64, f64)> {
        let (dx, dy) = (0.0005, 0.00036);
        vec![(lon0, lat0), (lon0 + dx, lat0), (lon0 + dx, lat0 + dy), (lon0, lat0 + dy), (lon0, lat0)]
    }

    fn check(out: &OsmFeatures) {
        assert_eq!(out.buildings.len(), 1, "one building");
        assert_eq!(out.water.areas.len(), 1, "one lake");
        assert_eq!(out.water.areas[0].outer.len(), 4, "closing node dropped");
        assert_eq!(out.water.waterways.len(), 1, "culverted stream dropped, river kept");
        assert_eq!(out.water.waterways[0].width_m, 25.0, "width tag parsed");
        assert_eq!(out.water.coastlines.len(), 1);
    }

    fn parse(data: Value, noderefs: bool) -> OsmFeatures {
        let b = bbox();
        let mut seen = HashSet::new();
        let mut out = OsmFeatures::default();
        let mut stats = FetchStats::default();
        let mut ctx = ParseCtx { bbox: &b, min_area_m2: 10.0, opts: ALL, seen: &mut seen, out: &mut out, stats: &mut stats };
        if noderefs {
            parse_noderefs_response(data, &mut ctx).unwrap();
        } else {
            parse_geom_response(data, &mut ctx).unwrap();
        }
        out
    }

    fn ways() -> Vec<(u64, Value, Vec<(f64, f64)>)> {
        vec![
            (1, json!({"building": "yes"}), square(11.335, 44.495)),
            (2, json!({"natural": "water"}), square(11.340, 44.495)),
            (3, json!({"waterway": "river", "width": "25 m"}), vec![(11.33, 44.492), (11.345, 44.492)]),
            (4, json!({"waterway": "stream", "tunnel": "culvert"}), vec![(11.33, 44.498), (11.34, 44.498)]),
            (5, json!({"natural": "coastline"}), vec![(11.32, 44.491), (11.35, 44.491)]),
        ]
    }

    #[test]
    fn overpass_geom_parser_dispatches_water() {
        let elements: Vec<Value> = ways()
            .into_iter()
            .map(|(id, tags, pts)| {
                json!({
                    "type": "way", "id": id, "tags": tags,
                    "geometry": pts.iter().map(|&(lon, lat)| json!({"lon": lon, "lat": lat})).collect::<Vec<_>>(),
                })
            })
            .collect();
        check(&parse(json!({ "elements": elements }), false));
    }

    #[test]
    fn direct_api_parser_dispatches_water() {
        let mut elements: Vec<Value> = Vec::new();
        let mut next_node = 100u64;
        let mut node_of: HashMap<(u64, u64), u64> = HashMap::new();
        for (id, tags, pts) in ways() {
            let refs: Vec<u64> = pts
                .iter()
                .map(|&(lon, lat)| {
                    *node_of.entry((lon.to_bits(), lat.to_bits())).or_insert_with(|| {
                        next_node += 1;
                        elements.push(json!({"type": "node", "id": next_node, "lon": lon, "lat": lat}));
                        next_node
                    })
                })
                .collect();
            elements.push(json!({"type": "way", "id": id, "tags": tags, "nodes": refs}));
        }
        check(&parse(json!({ "elements": elements }), true));
    }
}
