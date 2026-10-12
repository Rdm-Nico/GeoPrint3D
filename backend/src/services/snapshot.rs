//! Record / replay of the raw inputs of one generation: the OSM tile responses,
//! the whole relations re-fetched after the merge, and the elevation grid.
//!
//! Used by the city regression harness (`bin/city_tests`). Replaying a snapshot
//! feeds the SAME bytes to the SAME parsers, so two replays can only differ
//! because the code changed; recording again (`--refresh`) shows the effect of
//! OSM edits. Patches rewrite tags of (or drop) raw elements before parsing:
//! "what-if" experiments that tell data problems from algorithm problems
//! without touching the pipeline. The HTTP server never uses snapshots.

use crate::models::{BoundingBox, ElevationPoint};
use anyhow::{Context, Result};
use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};

/// Bumped whenever the on-disk layout changes.
pub const SNAPSHOT_VERSION: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotMode {
    /// Fetch live, then save the raw inputs.
    Record,
    /// Never touch the network: read the raw inputs saved by `Record`.
    Replay,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub dir: PathBuf,
    pub mode: SnapshotMode,
    /// Applied to the raw OSM elements before parsing (replay only).
    pub patches: Vec<Patch>,
    /// Extra OSM-API responses (node-ref JSON, e.g. `relation/{id}/full.json`)
    /// parsed BEFORE the recorded tiles (replay only): simulates fetching an
    /// element whole, so its complete copy wins the (type, id) dedupe.
    pub inject: Vec<PathBuf>,
}

impl Snapshot {
    pub fn osm_path(&self) -> PathBuf {
        self.dir.join("osm_raw.json.gz")
    }

    pub fn elevation_path(&self) -> PathBuf {
        self.dir.join("elevation.json.gz")
    }

    pub fn is_replay(&self) -> bool {
        self.mode == SnapshotMode::Replay
    }

    pub fn is_record(&self) -> bool {
        self.mode == SnapshotMode::Record
    }

    /// True when both raw files of a complete recording exist in `dir`.
    pub fn exists(dir: &Path) -> bool {
        dir.join("osm_raw.json.gz").is_file() && dir.join("elevation.json.gz").is_file()
    }
}

/// One "what-if" edit of a raw OSM element.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Patch {
    /// `way/244159210`, `relation/4114839`, `node/…`
    pub element: String,
    /// Tags to add or overwrite.
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    /// Tags to remove.
    #[serde(default)]
    pub unset: Vec<String>,
    /// Drop the element from the response entirely.
    #[serde(default)]
    pub delete: bool,
}

impl Patch {
    fn target(&self) -> Option<(&str, u64)> {
        let (kind, id) = self.element.split_once('/')?;
        Some((kind, id.trim().parse().ok()?))
    }
}

/// Apply `patches` to the `elements` array of one raw OSM response (Overpass
/// `out geom` or OSM-API node-ref format — both list elements with `type`,
/// `id` and `tags`). Returns how many elements were touched. Geometry that an
/// Overpass relation carries inline for its member ways is not patched.
pub fn apply_patches(data: &mut Value, patches: &[Patch]) -> usize {
    if patches.is_empty() {
        return 0;
    }
    let targets: Vec<(&Patch, &str, u64)> = patches
        .iter()
        .filter_map(|p| p.target().map(|(k, id)| (p, k, id)))
        .collect();
    let Some(elements) = data["elements"].as_array_mut() else {
        return 0;
    };
    let mut touched = 0;
    elements.retain_mut(|el| {
        let (Some(kind), Some(id)) = (el["type"].as_str().map(str::to_string), el["id"].as_u64()) else {
            return true;
        };
        let mut keep = true;
        for (p, k, pid) in &targets {
            if *k != kind.as_str() || *pid != id {
                continue;
            }
            touched += 1;
            if p.delete {
                keep = false;
                continue;
            }
            if !el["tags"].is_object() {
                el["tags"] = Value::Object(Default::default());
            }
            let tags = el["tags"].as_object_mut().expect("tags is an object");
            for key in &p.unset {
                tags.remove(key);
            }
            for (key, value) in &p.set {
                tags.insert(key.clone(), Value::String(value.clone()));
            }
        }
        keep
    });
    touched
}

// ── On-disk records ───────────────────────────────────────────────────────────

/// Raw OSM data of one fetch, as read back by a replay.
#[derive(Deserialize)]
pub struct OsmRaw {
    pub version: u32,
    pub recorded_at: String,
    pub bbox: BoundingBox,
    pub buildings: bool,
    pub water: bool,
    pub bridges: bool,
    pub failed_tiles: usize,
    pub tiles: Vec<RawTileRecord>,
    /// Whole relations re-fetched after the merge (OSM API `full.json`):
    /// response bodies by id.
    pub relations: BTreeMap<u64, String>,
}

#[derive(Deserialize)]
pub struct RawTileRecord {
    pub top: usize,
    pub path: Vec<u8>,
    pub source: String,
    /// `geom` (Overpass `out geom`) or `noderefs` (OSM API `map.json`).
    pub format: String,
    /// The HTTP response body exactly as received (JSON text). Stored as
    /// text, not as a parsed value: re-serialising parsed floats is not
    /// round-trip exact with serde_json's default parser.
    pub body: String,
}

/// Borrowing twin of `OsmRaw` for writing without cloning the (large) tiles.
#[derive(Serialize)]
pub struct OsmRawOut<'a> {
    pub version: u32,
    pub recorded_at: String,
    pub bbox: &'a BoundingBox,
    pub buildings: bool,
    pub water: bool,
    pub bridges: bool,
    pub failed_tiles: usize,
    pub tiles: Vec<RawTileOut<'a>>,
    pub relations: BTreeMap<u64, &'a str>,
}

#[derive(Serialize)]
pub struct RawTileOut<'a> {
    pub top: usize,
    pub path: Vec<u8>,
    pub source: &'a str,
    pub format: &'static str,
    pub body: &'a str,
}

/// The elevation grid exactly as the elevation service returned it (before
/// the handler's smoothing). Rows are the IEEE-754 bit patterns
/// `[lat f64, lon f64, elevation f32]`, so the replay is bit-exact.
#[derive(Serialize, Deserialize)]
pub struct ElevationRaw {
    pub version: u32,
    pub recorded_at: String,
    pub bbox: BoundingBox,
    pub resolution: u32,
    pub source: String,
    pub points: Vec<[u64; 3]>,
}

impl ElevationRaw {
    pub fn new(bbox: &BoundingBox, resolution: u32, source: &str, points: &[ElevationPoint]) -> Self {
        Self {
            version: SNAPSHOT_VERSION,
            recorded_at: chrono::Utc::now().to_rfc3339(),
            bbox: bbox.clone(),
            resolution,
            source: source.to_string(),
            points: points
                .iter()
                .map(|p| [p.lat.to_bits(), p.lon.to_bits(), p.elevation.to_bits() as u64])
                .collect(),
        }
    }

    /// The points back, bit for bit.
    pub fn points(&self) -> Vec<ElevationPoint> {
        self.points
            .iter()
            .map(|p| ElevationPoint {
                lat: f64::from_bits(p[0]),
                lon: f64::from_bits(p[1]),
                elevation: f32::from_bits(p[2] as u32),
            })
            .collect()
    }
}

pub fn write_gz_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let mut enc = GzEncoder::new(BufWriter::new(file), Compression::default());
    serde_json::to_writer(&mut enc, value).with_context(|| format!("writing {}", path.display()))?;
    enc.finish()?;
    Ok(())
}

pub fn read_gz_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    serde_json::from_reader(GzDecoder::new(BufReader::new(file)))
        .with_context(|| format!("reading {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn response() -> Value {
        json!({"elements": [
            {"type": "node", "id": 1, "lat": 0.0, "lon": 0.0},
            {"type": "way", "id": 7, "nodes": [1], "tags": {"building": "basilica", "height": "136,5"}},
            {"type": "way", "id": 8, "nodes": [1]},
            {"type": "relation", "id": 7, "members": [], "tags": {"type": "multipolygon"}},
        ]})
    }

    #[test]
    fn patches_set_unset_and_delete_by_type_and_id() {
        let mut data = response();
        let patches = vec![
            Patch {
                element: "way/7".into(),
                set: [("height".to_string(), "54".to_string())].into(),
                unset: vec!["building".into()],
                delete: false,
            },
            Patch { element: "way/8".into(), set: [("building".to_string(), "yes".to_string())].into(), ..Default::default() },
            Patch { element: "relation/7".into(), delete: true, ..Default::default() },
        ];
        assert_eq!(apply_patches(&mut data, &patches), 3);
        let els = data["elements"].as_array().unwrap();
        assert_eq!(els.len(), 3, "relation/7 dropped, way/7 kept (same id, other type)");
        assert_eq!(els[1]["tags"]["height"], "54");
        assert!(els[1]["tags"].get("building").is_none());
        assert_eq!(els[2]["tags"]["building"], "yes", "untagged way gets a tags object");
    }

    #[test]
    fn elevation_round_trip_is_exact() {
        let bbox = BoundingBox { min_lat: 41.0, min_lon: 12.0, max_lat: 41.1, max_lon: 12.1 };
        let pts = vec![ElevationPoint { lat: 41.012345678, lon: 12.0987654321, elevation: 37.123_456 }];
        let raw = ElevationRaw::new(&bbox, 1, "test", &pts);
        let back: ElevationRaw = serde_json::from_str(&serde_json::to_string(&raw).unwrap()).unwrap();
        let p = &back.points()[0];
        assert_eq!((p.lat, p.lon, p.elevation), (pts[0].lat, pts[0].lon, pts[0].elevation));
    }
}
