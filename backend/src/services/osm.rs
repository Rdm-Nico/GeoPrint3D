use crate::models::{BoundingBox, Building, RoofShape};
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;

// Community-run Overpass mirrors — tried first (compact out geom response).
const OVERPASS_ENDPOINTS: &[&str] = &[
    "https://overpass-api.de/api/interpreter",
    "https://overpass.kumi.systems/api/interpreter",
    "https://overpass.private.coffee/api/interpreter",
];

// Buildings below this footprint (m²) are sub-pixel at print scale and skipped.
const MIN_BUILDING_AREA_M2: f64 = 10.0;

// Above this area, buildings would be < 1 mm on a 100 mm print — skip the query.
pub const MAX_AREA_FOR_BUILDINGS_KM2: f64 = 5.0;

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

    /// Fetch building footprints, heights and roof shapes from OSM.
    ///
    /// Fetched element classes:
    ///   - `way[building]`                — plain building outlines
    ///   - `way[building:part]`           — Simple 3D Buildings parts (domes, towers,
    ///                                      naves at different heights / min_height)
    ///   - `relation[type=multipolygon]`  — buildings with courtyards (inner rings)
    ///                                      or outlines split across several ways
    ///                                      (e.g. St Peter's colonnade)
    ///
    /// Strategy (each step is tried only if the previous one fails):
    ///   1. Overpass mirrors 1-3 — compact `out geom` query, no recurse step.
    ///   2. OSM direct API    — `api.openstreetmap.org/api/0.6/map.json`, OSMF-maintained,
    ///                          much more available than community Overpass instances.
    pub async fn fetch_buildings(&self, bbox: &BoundingBox) -> Result<Vec<Building>> {
        tracing::info!("   ┌─ OpenStreetMap Building Service");
        tracing::info!(
            "   │  Bbox: ({:.4}, {:.4}) → ({:.4}, {:.4})",
            bbox.min_lat, bbox.min_lon, bbox.max_lat, bbox.max_lon
        );

        let request_start = std::time::Instant::now();

        // ── Step 1: Overpass mirrors ──────────────────────────────────────────
        // `out geom qt` returns coordinates inline — ~10× smaller than
        // `out body;>;out skel qt` because no separate node objects are emitted.
        // Relations include per-member geometry and roles (outer/inner).
        let bb = format!(
            "{},{},{},{}",
            bbox.min_lat, bbox.min_lon, bbox.max_lat, bbox.max_lon
        );
        let overpass_query = format!(
            "[out:json][timeout:25];(\
             way[\"building\"][\"building\"!=\"no\"]({bb});\
             way[\"building:part\"][\"building:part\"!=\"no\"]({bb});\
             relation[\"type\"=\"multipolygon\"][\"building\"][\"building\"!=\"no\"]({bb});\
             relation[\"type\"=\"multipolygon\"][\"building:part\"][\"building:part\"!=\"no\"]({bb});\
             );out geom qt;"
        );

        for (i, &endpoint) in OVERPASS_ENDPOINTS.iter().enumerate() {
            tracing::info!("   │  Overpass attempt {}/{}: {}", i + 1, OVERPASS_ENDPOINTS.len(), endpoint);

            let resp = match self
                .client
                .post(endpoint)
                .form(&[("data", &overpass_query)])
                .timeout(Duration::from_secs(30))
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("   │  Connection failed: {}", e);
                    continue;
                }
            };

            let status = resp.status();
            tracing::info!("   │  Status: {} ({:.2}s)", status, request_start.elapsed().as_secs_f64());

            match status.as_u16() {
                200..=299 => {
                    let data: Value = resp.json().await.context("Failed to parse Overpass JSON")?;
                    let elapsed = request_start.elapsed();
                    tracing::info!("   │  Source: Overpass (out geom)");
                    return self.parse_geom_response(data, bbox, elapsed);
                }
                429 | 406 => {
                    tracing::warn!("   │  Rate-limited / rejected — trying next mirror");
                    continue;
                }
                _ => {
                    let body = resp.text().await.unwrap_or_default();
                    tracing::warn!("   │  Error {}: {}", status, body.chars().take(120).collect::<String>());
                    continue;
                }
            }
        }

        // ── Step 2: OSM direct API (fallback) ────────────────────────────────
        // Returns all OSM elements in the bbox as JSON. More available than
        // Overpass because it is the primary OSMF-maintained endpoint.
        // Note: bbox order is west,south,east,north (lon,lat,lon,lat).
        tracing::info!("   │  All Overpass mirrors failed — falling back to OSM direct API");
        self.fetch_buildings_osm_api(bbox, request_start).await
    }

    async fn fetch_buildings_osm_api(
        &self,
        bbox: &BoundingBox,
        request_start: std::time::Instant,
    ) -> Result<Vec<Building>> {
        let url = format!(
            "https://api.openstreetmap.org/api/0.6/map.json?bbox={},{},{},{}",
            bbox.min_lon, bbox.min_lat, bbox.max_lon, bbox.max_lat
        );
        tracing::info!("   │  OSM API: {}", url);

        let resp = self
            .client
            .get(&url)
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .context("Failed to connect to OSM direct API")?;

        let status = resp.status();
        tracing::info!("   │  Status: {} ({:.2}s)", status, request_start.elapsed().as_secs_f64());

        if status.as_u16() == 509 {
            anyhow::bail!("OSM API: bounding box too large (> 50 000 nodes) — area exceeds service limit");
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("OSM API error {}: {}", status, body.chars().take(200).collect::<String>());
        }

        let data: Value = resp.json().await.context("Failed to parse OSM API JSON")?;
        let elapsed = request_start.elapsed();
        tracing::info!("   │  Source: OSM direct API");
        self.parse_noderefs_response(data, bbox, elapsed)
    }

    // ── Parsers ───────────────────────────────────────────────────────────────

    /// Parse an Overpass `out geom` response.
    /// Ways carry a `geometry` array with inline lat/lon objects; relations carry
    /// per-member geometry with `outer`/`inner` roles.
    fn parse_geom_response(&self, data: Value, bbox: &BoundingBox, elapsed: Duration) -> Result<Vec<Building>> {
        let elements = data["elements"]
            .as_array()
            .context("Missing 'elements' in Overpass response")?;

        tracing::info!("   │  Received {} OSM elements", elements.len());
        let mut buildings = Vec::new();
        let mut src = HeightSources::default();
        let mut relations = 0usize;

        for el in elements.iter() {
            let tags = &el["tags"];
            let id = el["id"].as_u64().unwrap_or(0);

            match el["type"].as_str().unwrap_or("") {
                "way" => {
                    let Some(geometry) = el["geometry"].as_array() else {
                        src.skipped += 1;
                        continue;
                    };
                    let footprint: Vec<(f64, f64)> = geometry
                        .iter()
                        .filter_map(|g| Some((g["lon"].as_f64()?, g["lat"].as_f64()?)))
                        .collect();
                    push_building(&mut buildings, id, footprint, Vec::new(), tags, bbox, &mut src);
                }
                "relation" => {
                    let Some(members) = el["members"].as_array() else {
                        continue;
                    };
                    relations += 1;
                    let mut outer_segments: Vec<Vec<(f64, f64)>> = Vec::new();
                    let mut inner_segments: Vec<Vec<(f64, f64)>> = Vec::new();
                    for m in members {
                        let Some(geom) = m["geometry"].as_array() else {
                            continue;
                        };
                        let line: Vec<(f64, f64)> = geom
                            .iter()
                            .filter_map(|g| Some((g["lon"].as_f64()?, g["lat"].as_f64()?)))
                            .collect();
                        if line.len() < 2 {
                            continue;
                        }
                        if m["role"].as_str() == Some("inner") {
                            inner_segments.push(line);
                        } else {
                            outer_segments.push(line);
                        }
                    }
                    push_relation(&mut buildings, id, outer_segments, inner_segments, tags, bbox, &mut src);
                }
                _ => {}
            }
        }

        let suppressed = suppress_covered_outlines(&mut buildings);
        log_building_stats(&buildings, &src, relations, suppressed, elapsed);
        Ok(buildings)
    }

    /// Parse an OSM direct API response (node-ref format).
    /// Ways reference node IDs; relations reference way IDs. Coordinates are
    /// resolved through the node list.
    fn parse_noderefs_response(&self, data: Value, bbox: &BoundingBox, elapsed: Duration) -> Result<Vec<Building>> {
        let elements = data["elements"]
            .as_array()
            .context("Missing 'elements' in OSM API response")?;

        tracing::info!("   │  Received {} total OSM elements", elements.len());

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

        let mut buildings = Vec::new();
        let mut src = HeightSources::default();
        let mut relations = 0usize;

        for el in elements.iter() {
            let tags = &el["tags"];
            let id = el["id"].as_u64().unwrap_or(0);
            let is_building = tags["building"].is_string() && tags["building"] != "no";
            let is_part = tags["building:part"].is_string() && tags["building:part"] != "no";
            if !is_building && !is_part {
                continue;
            }

            match el["type"].as_str().unwrap_or("") {
                "way" => {
                    let footprint = ways.get(&id).cloned().unwrap_or_default();
                    if footprint.len() < 3 {
                        src.skipped += 1;
                        continue;
                    }
                    push_building(&mut buildings, id, footprint, Vec::new(), tags, bbox, &mut src);
                }
                "relation" => {
                    if tags["type"] != "multipolygon" {
                        continue;
                    }
                    let Some(members) = el["members"].as_array() else {
                        continue;
                    };
                    relations += 1;
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
                    push_relation(&mut buildings, id, outer_segments, inner_segments, tags, bbox, &mut src);
                }
                _ => {}
            }
        }

        let suppressed = suppress_covered_outlines(&mut buildings);
        log_building_stats(&buildings, &src, relations, suppressed, elapsed);
        Ok(buildings)
    }
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
    src: &mut HeightSources,
) {
    // Inference (roof shape from a quad footprint) looks at the original
    // corner count, not the clip-induced one.
    let corners = corner_count(&footprint);
    let footprint = clip_ring_to_bbox(&footprint, bbox);
    if footprint.len() < 3 || footprint_area_m2(&footprint) < MIN_BUILDING_AREA_M2 {
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
        push_building(buildings, id, ring, holes, tags, bbox, src);
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
    let height = building_height(tags, kind_tag, id, src).max(min_height + 0.5);

    ParsedTags { height, min_height, roof_shape, roof_height, is_part }
}

/// Resolve a building height in metres from OSM tags.
/// Priority: explicit `height`/`est_height` → `building:levels` (+roof) → type-aware default.
/// Defaulted heights receive a small deterministic jitter (seeded by OSM id) so areas
/// lacking height data stop rendering as a grid of identical cubes.
fn building_height(tags: &Value, kind_tag: &Value, id: u64, src: &mut HeightSources) -> f32 {
    // 1. Explicit height in metres (tolerant of "12 m", "12.5", ranges "10-20").
    if let Some(h) = tags["height"].as_str().and_then(parse_meters) {
        src.tag += 1;
        return h.max(1.0);
    }
    if let Some(h) = tags["est_height"].as_str().and_then(parse_meters) {
        src.tag += 1;
        return h.max(1.0);
    }

    // 2. Storey count → metres (~3.2 m/floor), adding roof levels when tagged.
    if let Some(l) = tags["building:levels"].as_str().and_then(|s| s.trim().parse::<f32>().ok()) {
        src.levels += 1;
        let roof = tags["roof:levels"]
            .as_str()
            .and_then(|s| s.trim().parse::<f32>().ok())
            .unwrap_or(0.0);
        return ((l + roof) * 3.2).max(2.5);
    }

    // 3. Type-aware default with deterministic per-building variation.
    src.default += 1;
    default_height_for_type(kind_tag.as_str().unwrap_or("yes")) * jitter(id)
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
fn default_height_for_type(kind: &str) -> f32 {
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

fn log_building_stats(
    buildings: &[Building],
    src: &HeightSources,
    relations: usize,
    suppressed: usize,
    elapsed: Duration,
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
    tracing::info!("   └─ OSM processing time: {:.2}s", elapsed.as_secs_f64());
}

/// Shoelace formula for polygon area in m².
/// Accurate enough for filtering; not for precise measurements.
fn footprint_area_m2(pts: &[(f64, f64)]) -> f64 {
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
    (area.abs() / 2.0) * 111_320.0 * 111_320.0
}

impl Default for OsmService {
    fn default() -> Self {
        Self::new()
    }
}
