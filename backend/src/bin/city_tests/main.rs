//! City regression harness: generates the fixed test places of
//! `tests/cities/cities.json` straight to STL (no frontend, no HTTP), plus a
//! manifest and PNG previews per place. See `tests/cities/README.md`.
//!
//! Raw inputs (OSM tiles, elevation grid) are snapshotted per city under
//! `tests/cities/snapshots/<id>/` and replayed by default, so two runs differ
//! only when the code does; `--refresh` records fresh data. `--experiments`
//! replays snapshots with the tag patches of `tests/cities/experiments.json`.

mod render;

use anyhow::{bail, Context, Result};
use geoprint3d_backend::api::handlers::{generate, GenerateOptions, GenerateOutput};
use geoprint3d_backend::models::{BoundingBox, Building, GenerateRequest};
use geoprint3d_backend::services::osm::{FetchConfig, SourcePolicy};
use geoprint3d_backend::services::snapshot::{read_gz_json, ElevationRaw, Patch, Snapshot, SnapshotMode};
use geoprint3d_backend::services::{ElevationService, OsmService};
use geoprint3d_backend::utils::MeshExporter;
use render::{crop_around, Ranges, View};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const DEFAULT_CONFIG: &str = "tests/cities/cities.json";
const DEFAULT_EXPERIMENTS: &str = "tests/cities/experiments.json";
const SNAPSHOT_ROOT: &str = "tests/cities/snapshots";
const OUTPUT_ROOT: &str = "tests/cities/output";
/// The CLI is not bound by the frontend's 120 s abort: give slow mirrors
/// time instead of shipping partial coverage (Paris hit the 90 s budget).
const CLI_OSM_BUDGET_S: u64 = 240;

// ── Configuration ─────────────────────────────────────────────────────────────

#[derive(Deserialize, Clone, Debug, serde::Serialize)]
struct Params {
    resolution: u32,
    vertical_scale: f32,
    base_height: f32,
    print_size: f32,
    include_buildings: bool,
    include_water: bool,
    include_bridges: bool,
    bridge_print_supports: bool,
}

#[derive(Deserialize, Clone, Debug, Default)]
struct ParamOverrides {
    resolution: Option<u32>,
    vertical_scale: Option<f32>,
    base_height: Option<f32>,
    print_size: Option<f32>,
    include_buildings: Option<bool>,
    include_water: Option<bool>,
    include_bridges: Option<bool>,
    bridge_print_supports: Option<bool>,
}

impl Params {
    fn with(&self, o: &ParamOverrides) -> Params {
        Params {
            resolution: o.resolution.unwrap_or(self.resolution),
            vertical_scale: o.vertical_scale.unwrap_or(self.vertical_scale),
            base_height: o.base_height.unwrap_or(self.base_height),
            print_size: o.print_size.unwrap_or(self.print_size),
            include_buildings: o.include_buildings.unwrap_or(self.include_buildings),
            include_water: o.include_water.unwrap_or(self.include_water),
            include_bridges: o.include_bridges.unwrap_or(self.include_bridges),
            bridge_print_supports: o.bridge_print_supports.unwrap_or(self.bridge_print_supports),
        }
    }

    fn request(&self, bbox: &BoundingBox) -> GenerateRequest {
        GenerateRequest {
            bbox: bbox.clone(),
            resolution: Some(self.resolution),
            vertical_scale: Some(self.vertical_scale),
            base_height: Some(self.base_height),
            include_buildings: Some(self.include_buildings),
            print_size: Some(self.print_size),
            include_water: Some(self.include_water),
            include_bridges: Some(self.include_bridges),
            bridge_print_supports: Some(self.bridge_print_supports),
        }
    }
}

#[derive(Deserialize, Clone, Debug)]
struct ViewCfg {
    name: String,
    az: f32,
    el: f32,
    /// Close-up centre as [lat, lon]; omitted = whole model.
    center: Option<[f64; 2]>,
    #[serde(default = "default_radius")]
    radius_m: f64,
    /// Hide terrain and water (landmark profiles).
    #[serde(default)]
    buildings_only: bool,
    width: Option<u32>,
    height: Option<u32>,
}

fn default_radius() -> f64 {
    300.0
}

#[derive(Deserialize, Clone, Debug)]
struct CityCfg {
    id: String,
    name: String,
    group: String,
    bbox: BoundingBox,
    /// Where the bbox comes from (user run, created for the suite…).
    source: String,
    #[serde(default)]
    notes: String,
    #[serde(default)]
    params: ParamOverrides,
    #[serde(default)]
    views: Vec<ViewCfg>,
}

#[derive(Deserialize)]
struct Config {
    defaults: Params,
    cities: Vec<CityCfg>,
}

#[derive(Deserialize, Clone, Debug)]
struct ExperimentCfg {
    id: String,
    city: String,
    description: String,
    #[serde(default)]
    patches: Vec<Patch>,
    /// OSM-API responses parsed before the recorded tiles (paths relative to
    /// backend/), e.g. a relation fetched whole.
    #[serde(default)]
    inject: Vec<PathBuf>,
    /// Defaults to the city's views.
    #[serde(default)]
    views: Vec<ViewCfg>,
}

#[derive(Deserialize)]
struct ExperimentsFile {
    experiments: Vec<ExperimentCfg>,
}

// ── CLI ───────────────────────────────────────────────────────────────────────

struct Args {
    config: PathBuf,
    experiments_file: PathBuf,
    only: Option<Vec<String>>,
    group: String,
    refresh: bool,
    live: bool,
    experiments: bool,
    source: SourcePolicy,
    budget_s: u64,
    render: bool,
    out: Option<PathBuf>,
    label: Option<String>,
}

const USAGE: &str = "\
city_tests — generate the fixed test places to STL (backend only)

USAGE: tests/cities/run.sh [options]   (or: cargo run --release --bin city_tests -- [options])

  --only a,b          only these city (or experiment) ids
  --group G           cities | landmark | all (default: all)
  --refresh           fetch live data and (re)record the snapshots
  --live              fetch live data without touching the snapshots
  --experiments       run the patch experiments of experiments.json (replay)
  --source S          hedged | overpass | direct (default: hedged; live fetches only)
  --budget SECS       OSM tile-phase budget (default: 240)
  --no-render         skip the PNG previews
  --out DIR           output directory (default: tests/cities/output/<timestamp>)
  --label NAME        suffix for the default output directory
  --config FILE       default: tests/cities/cities.json
  --experiments-file FILE   default: tests/cities/experiments.json
";

fn parse_args() -> Result<Args> {
    let mut a = Args {
        config: DEFAULT_CONFIG.into(),
        experiments_file: DEFAULT_EXPERIMENTS.into(),
        only: None,
        group: "all".into(),
        refresh: false,
        live: false,
        experiments: false,
        source: SourcePolicy::Hedged,
        budget_s: CLI_OSM_BUDGET_S,
        render: true,
        out: None,
        label: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().with_context(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--only" => a.only = Some(value("--only")?.split(',').map(|s| s.trim().to_string()).collect()),
            "--group" => a.group = value("--group")?,
            "--refresh" => a.refresh = true,
            "--live" => a.live = true,
            "--experiments" => a.experiments = true,
            "--source" => {
                a.source = match value("--source")?.as_str() {
                    "hedged" => SourcePolicy::Hedged,
                    "overpass" => SourcePolicy::OverpassOnly,
                    "direct" => SourcePolicy::DirectOnly,
                    other => bail!("unknown --source {other} (hedged | overpass | direct)"),
                }
            }
            "--budget" => a.budget_s = value("--budget")?.parse().context("--budget expects seconds")?,
            "--no-render" => a.render = false,
            "--out" => a.out = Some(value("--out")?.into()),
            "--label" => a.label = Some(value("--label")?),
            "--config" => a.config = value("--config")?.into(),
            "--experiments-file" => a.experiments_file = value("--experiments-file")?.into(),
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            other => bail!("unknown argument {other}\n\n{USAGE}"),
        }
    }
    if a.refresh && a.live {
        bail!("--refresh and --live are mutually exclusive");
    }
    if a.experiments && (a.refresh || a.live) {
        bail!("--experiments always replays snapshots (record them first with --refresh)");
    }
    Ok(a)
}

// ── One generation job (a city or an experiment) ─────────────────────────────

struct Job {
    id: String,
    name: String,
    city: CityCfg,
    params: Params,
    views: Vec<ViewCfg>,
    patches: Vec<Patch>,
    inject: Vec<PathBuf>,
    description: Option<String>,
}

struct Services {
    elevation: ElevationService,
    osm: OsmService,
}

/// Recorded snapshot matching the request (same bbox and resolution)?
fn snapshot_matches(dir: &Path, bbox: &BoundingBox, resolution: u32) -> bool {
    if !Snapshot::exists(dir) {
        return false;
    }
    let Ok(raw) = read_gz_json::<ElevationRaw>(&dir.join("elevation.json.gz")) else {
        return false;
    };
    let same = |a: f64, b: f64| (a - b).abs() < 1e-9;
    raw.resolution == resolution
        && same(raw.bbox.min_lat, bbox.min_lat)
        && same(raw.bbox.min_lon, bbox.min_lon)
        && same(raw.bbox.max_lat, bbox.max_lat)
        && same(raw.bbox.max_lon, bbox.max_lon)
}

async fn run_job(job: &Job, args: &Args, svc: &Services, out_dir: &Path) -> Result<Value> {
    std::fs::create_dir_all(out_dir)?;
    let snap_dir = Path::new(SNAPSHOT_ROOT).join(&job.city.id);
    let matches = snapshot_matches(&snap_dir, &job.city.bbox, job.params.resolution);
    let mode = if args.live {
        None
    } else if !job.patches.is_empty() || !job.inject.is_empty() || args.experiments {
        if !matches {
            bail!(
                "no snapshot of '{}' matching its bbox: run `run.sh --only {} --refresh` first",
                job.city.id, job.city.id
            );
        }
        Some(SnapshotMode::Replay)
    } else if args.refresh || !matches {
        if !args.refresh && Snapshot::exists(&snap_dir) {
            tracing::warn!("snapshot of {} does not match its bbox/resolution — re-recording", job.city.id);
        }
        Some(SnapshotMode::Record)
    } else {
        Some(SnapshotMode::Replay)
    };
    let options = GenerateOptions {
        snapshot: mode.map(|mode| Snapshot {
            dir: snap_dir.clone(),
            mode,
            patches: job.patches.clone(),
            inject: job.inject.clone(),
        }),
        geometry_report_path: out_dir.join("geometry.json"),
        fetch: FetchConfig { budget: Duration::from_secs(args.budget_s), source: args.source },
    };

    let t0 = Instant::now();
    let out = generate(&svc.elevation, &svc.osm, job.params.request(&job.city.bbox), &options)
        .await
        .map_err(|e| anyhow::anyhow!("generation failed: {}", e.message()))?;
    let generate_s = t0.elapsed().as_secs_f64();

    let t1 = Instant::now();
    let stl_path = out_dir.join(format!("{}.stl", job.id));
    MeshExporter::export_stl(&out.mesh, &stl_path)?;
    let stl_bytes = std::fs::read(&stl_path)?;
    let export_s = t1.elapsed().as_secs_f64();

    let t2 = Instant::now();
    let md = &out.response.mesh_data;
    let ranges = Ranges {
        terrain: md.terrain_triangle_count,
        water: md.water_triangle_count,
        buildings: md.building_triangle_count,
        supports: md.support_triangle_count,
    };
    let mut images = Vec::new();
    if args.render {
        for vc in &job.views {
            let view = View {
                az_deg: vc.az,
                el_deg: vc.el,
                crop: vc.center.map(|c| crop_around(&job.city.bbox, c, vc.radius_m)),
                buildings_only: vc.buildings_only,
                width: vc.width.unwrap_or(if vc.center.is_some() { 1200 } else { 1600 }),
                height: vc.height.unwrap_or(if vc.center.is_some() { 900 } else { 1000 }),
            };
            let path = out_dir.join(format!("{}.png", vc.name));
            render::render(&out.mesh, ranges, &view).save(&path)?;
            images.push(path.file_name().unwrap().to_string_lossy().to_string());
        }
    }
    let render_s = t2.elapsed().as_secs_f64();

    let manifest = manifest(job, &options, &out, &stl_path, &stl_bytes, &images, [generate_s, export_s, render_s]);
    std::fs::write(out_dir.join("manifest.json"), serde_json::to_string_pretty(&manifest)?)?;
    Ok(manifest)
}

// ── Diagnostics ───────────────────────────────────────────────────────────────

/// FNV-1a 64-bit: a stable fingerprint of the STL bytes (determinism checks).
fn fnv1a64(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

fn point_in_ring(p: (f64, f64), ring: &[(f64, f64)]) -> bool {
    let mut inside = false;
    let n = ring.len();
    for i in 0..n {
        let (x0, y0) = ring[i];
        let (x1, y1) = ring[(i + 1) % n];
        if (y0 > p.1) != (y1 > p.1) && p.0 < x0 + (p.1 - y0) / (y1 - y0) * (x1 - x0) {
            inside = !inside;
        }
    }
    inside
}

fn centroid(ring: &[(f64, f64)]) -> (f64, f64) {
    let n = ring.len().max(1) as f64;
    (ring.iter().map(|p| p.0).sum::<f64>() / n, ring.iter().map(|p| p.1).sum::<f64>() / n)
}

/// Solids that start above the ground (`min_height` ≥ 0.5 m) with nothing
/// under their centroid reaching their bottom: on an FDM printer they float.
/// Gap = bottom − highest top below it (or the ground), in real metres and
/// printed mm.
fn floating_solids(out: &GenerateOutput) -> Value {
    let (b, p) = (&out.buildings, &out.placements);
    let scale = out.scale.building_scale.max(1e-6);
    let mesh_to_mm = out.scale.mm_per_m; // mesh Z units = metres × scale; XY metres → mm
    let placed: Vec<usize> = (0..b.len().min(p.len())).filter(|&i| p[i].vertices_added > 0).collect();
    let bboxes: HashMap<usize, [f64; 4]> = placed
        .iter()
        .map(|&i| {
            let mut bb = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
            for &(x, y) in &b[i].footprint {
                bb = [bb[0].min(x), bb[1].min(y), bb[2].max(x), bb[3].max(y)];
            }
            (i, bb)
        })
        .collect();
    let mut floating: Vec<(f32, Value)> = Vec::new();
    for &i in &placed {
        if b[i].min_height < 0.5 {
            continue;
        }
        let bottom = p[i].bottom_z;
        let c = centroid(&b[i].footprint);
        let tol = 0.5 * scale; // half a real metre
        let mut best_top = p[i].base_elevation; // the ground under it
        let mut supported = false;
        for &j in &placed {
            if j == i || p[j].bottom_z >= bottom - 1e-3 {
                continue;
            }
            let bb = bboxes[&j];
            if c.0 < bb[0] || c.0 > bb[2] || c.1 < bb[1] || c.1 > bb[3] || !point_in_ring(c, &b[j].footprint) {
                continue;
            }
            if p[j].top_z >= bottom - tol {
                supported = true;
                break;
            }
            best_top = best_top.max(p[j].top_z);
        }
        if !supported {
            let gap_z = bottom - best_top;
            floating.push((
                gap_z,
                json!({
                    "id": b[i].id,
                    "kind": b[i].kind,
                    "is_part": b[i].is_part,
                    "min_height_m": b[i].min_height,
                    "height_m": b[i].height,
                    "gap_m": gap_z / scale,
                    "gap_mm": gap_z * mesh_to_mm,
                }),
            ));
        }
    }
    floating.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let over_1mm = floating.iter().filter(|(g, _)| g * mesh_to_mm >= 1.0).count();
    json!({
        "count": floating.len(),
        "gap_over_1mm": over_1mm,
        "largest": floating.into_iter().take(25).map(|(_, v)| v).collect::<Vec<_>>(),
    })
}

/// Edge-use check on the final mesh: every edge shared by exactly 2 triangles.
fn edge_check(mesh: &geoprint3d_backend::models::TerrainMesh) -> Value {
    let mut edges: HashMap<(usize, usize), u32> = HashMap::with_capacity(mesh.triangles.len() * 2);
    for t in &mesh.triangles {
        for k in 0..3 {
            let (a, b) = (t[k], t[(k + 1) % 3]);
            *edges.entry((a.min(b), a.max(b))).or_insert(0) += 1;
        }
    }
    let open = edges.values().filter(|&&c| c == 1).count();
    let over = edges.values().filter(|&&c| c > 2).count();
    json!({ "manifold": open == 0 && over == 0, "open_edges": open, "nonmanifold_edges": over })
}

fn building_counts(buildings: &[Building], out: &GenerateOutput) -> Value {
    let mut skipped: BTreeMap<String, usize> = BTreeMap::new();
    for p in &out.placements {
        if let Some(r) = &p.skip_reason {
            *skipped.entry(r.clone()).or_insert(0) += 1;
        }
    }
    let mut roofs: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut heights: BTreeMap<&'static str, usize> = BTreeMap::new();
    for b in buildings {
        *roofs.entry(b.roof_shape.name()).or_insert(0) += 1;
        *heights.entry(b.height_source.name()).or_insert(0) += 1;
    }
    json!({
        "elements": buildings.len(),
        "outlines": buildings.iter().filter(|b| !b.is_part).count(),
        "parts": buildings.iter().filter(|b| b.is_part).count(),
        "with_min_height": buildings.iter().filter(|b| b.min_height >= 0.5).count(),
        "with_courtyards": buildings.iter().filter(|b| !b.holes.is_empty()).count(),
        "placed": out.placements.iter().filter(|p| p.vertices_added > 0).count(),
        "skipped": skipped,
        "roof_shapes": roofs,
        "height_sources": heights,
    })
}

fn manifest(
    job: &Job,
    options: &GenerateOptions,
    out: &GenerateOutput,
    stl_path: &Path,
    stl_bytes: &[u8],
    images: &[String],
    timings: [f64; 3],
) -> Value {
    let md = &out.response.mesh_data;
    let st = &out.response.stats;
    json!({
        "id": job.id,
        "name": job.name,
        "city": job.city.id,
        "description": job.description,
        "bbox": job.city.bbox,
        "bbox_source": job.city.source,
        "notes": job.city.notes,
        "params": job.params,
        "generated_at": chrono::Local::now().to_rfc3339(),
        "snapshot": options.snapshot.as_ref().map(|s| json!({
            "dir": s.dir,
            "mode": format!("{:?}", s.mode).to_lowercase(),
            "patches": s.patches,
            "inject": s.inject,
        })),
        "elevation_source": out.elevation_source,
        "osm": out.fetch_report,
        "scale": out.scale,
        "buildings": building_counts(&out.buildings, out),
        "floating_solids": floating_solids(out),
        "bridges_emitted": st.bridges_count,
        "water_features": st.water_features_count,
        "mesh": {
            "vertices": md.vertices.len(),
            "triangles": md.triangles.len(),
            "terrain_triangles": md.terrain_triangle_count,
            "water_triangles": md.water_triangle_count,
            "building_triangles": md.building_triangle_count,
            "support_triangles": md.support_triangle_count,
            "edges": edge_check(&out.mesh),
        },
        "stl": {
            "file": stl_path.file_name().map(|f| f.to_string_lossy().to_string()),
            "bytes": stl_bytes.len(),
            "fnv1a64": fnv1a64(stl_bytes),
        },
        "images": images,
        "timings_s": { "generate": timings[0], "export": timings[1], "render": timings[2] },
    })
}

// ── Summary ───────────────────────────────────────────────────────────────────

fn summary_row(id: &str, m: &Value) -> String {
    let osm = &m["osm"];
    let mut sources: BTreeMap<String, usize> = BTreeMap::new();
    for t in osm["tiles"].as_array().into_iter().flatten() {
        if t["error"].is_null() {
            let s = t["source"].as_str().unwrap_or("?");
            let short = if s.contains("overpass") { "overpass" } else if s.contains("direct") { "api" } else { s };
            *sources.entry(short.to_string()).or_insert(0) += 1;
        }
    }
    let src = sources.iter().map(|(k, v)| format!("{v}×{k}")).collect::<Vec<_>>().join(" ");
    format!(
        "| {} | {} | {} | {} | {} | {} | {} | {} | {}/{} | {:.0}→{:.0} m | {} | {:.1} MB | {:.0} s |",
        id,
        m["snapshot"]["mode"].as_str().unwrap_or("live"),
        osm["recorded_at"].as_str().map(|s| &s[..s.len().min(16)]).unwrap_or("-"),
        src,
        if osm["partial"].as_bool().unwrap_or(false) { "**SÌ**" } else { "no" },
        m["buildings"]["elements"],
        m["buildings"]["parts"],
        m["floating_solids"]["count"],
        m["scale"]["compressed"],
        m["scale"]["knee_m"].as_f64().map(|k| format!("{k:.0}")).unwrap_or_default(),
        m["scale"]["max_height_before_m"].as_f64().unwrap_or(0.0),
        m["scale"]["max_height_after_m"].as_f64().unwrap_or(0.0),
        if m["mesh"]["edges"]["manifold"].as_bool().unwrap_or(false) { "✓" } else { "✗" },
        m["stl"]["bytes"].as_f64().unwrap_or(0.0) / 1e6,
        m["timings_s"]["generate"].as_f64().unwrap_or(0.0),
    )
}

fn write_summary(run_dir: &Path, results: &[(String, Result<Value, String>)]) -> Result<()> {
    let mut md = String::from(
        "# City tests — riepilogo\n\n\
         | id | dati | snapshot | tile | parziale | elementi | part | sospesi | compressi/knee | max h (prima→dopo) | manifold | STL | tempo |\n\
         |---|---|---|---|---|---|---|---|---|---|---|---|---|\n",
    );
    let mut json_rows = serde_json::Map::new();
    for (id, r) in results {
        match r {
            Ok(m) => {
                md.push_str(&summary_row(id, m));
                md.push('\n');
                json_rows.insert(id.clone(), m.clone());
            }
            Err(e) => {
                md.push_str(&format!("| {id} | **ERRORE**: {} |||||||||||| \n", e.replace('|', "/")));
                json_rows.insert(id.clone(), json!({ "error": e }));
            }
        }
    }
    std::fs::write(run_dir.join("summary.md"), md)?;
    std::fs::write(run_dir.join("summary.json"), serde_json::to_string_pretty(&Value::Object(json_rows))?)?;
    Ok(())
}

// ── Main ──────────────────────────────────────────────────────────────────────

fn init_logging(run_dir: &Path) -> tracing_appender::non_blocking::WorkerGuard {
    let appender = tracing_appender::rolling::never(run_dir, "run.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).with_ansi(false).with_writer(writer).init();
    guard
}

fn jobs(args: &Args, cfg: &Config) -> Result<Vec<Job>> {
    let city_job = |c: &CityCfg| Job {
        id: c.id.clone(),
        name: c.name.clone(),
        city: c.clone(),
        params: cfg.defaults.with(&c.params),
        views: c.views.clone(),
        patches: Vec::new(),
        inject: Vec::new(),
        description: None,
    };
    let wanted = |id: &str| args.only.as_ref().is_none_or(|o| o.iter().any(|w| w == id));
    if args.experiments {
        let text = std::fs::read_to_string(&args.experiments_file)
            .with_context(|| format!("reading {}", args.experiments_file.display()))?;
        let file: ExperimentsFile = serde_json::from_str(&text).context("parsing experiments file")?;
        let mut out = Vec::new();
        for e in file.experiments.iter().filter(|e| wanted(&e.id) || wanted(&e.city)) {
            let city = cfg
                .cities
                .iter()
                .find(|c| c.id == e.city)
                .with_context(|| format!("experiment {} refers to unknown city {}", e.id, e.city))?;
            let mut job = city_job(city);
            job.id = e.id.clone();
            job.name = format!("{} — {}", city.name, e.id);
            job.patches = e.patches.clone();
            job.inject = e.inject.clone();
            job.description = Some(e.description.clone());
            if !e.views.is_empty() {
                job.views = e.views.clone();
            }
            out.push(job);
        }
        return Ok(out);
    }
    Ok(cfg
        .cities
        .iter()
        .filter(|c| args.group == "all" || c.group == args.group)
        .filter(|c| wanted(&c.id))
        .map(city_job)
        .collect())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args()?;
    dotenvy::dotenv().ok();

    let text = std::fs::read_to_string(&args.config)
        .with_context(|| format!("reading {} (run from backend/, or use tests/cities/run.sh)", args.config.display()))?;
    let cfg: Config = serde_json::from_str(&text).context("parsing cities config")?;
    let jobs = jobs(&args, &cfg)?;
    if jobs.is_empty() {
        bail!("nothing selected (check --only / --group)");
    }

    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let run_name = match (&args.label, args.experiments) {
        (Some(l), _) => format!("{stamp}-{l}"),
        (None, true) => format!("{stamp}-experiments"),
        (None, false) => stamp,
    };
    let run_dir = args.out.clone().unwrap_or_else(|| Path::new(OUTPUT_ROOT).join(&run_name));
    std::fs::create_dir_all(&run_dir)?;
    let _guard = init_logging(&run_dir);

    let svc = Services {
        elevation: ElevationService::new(std::env::var("MAPBOX_TOKEN").ok()),
        osm: OsmService::new(),
    };
    println!("city_tests → {} ({} job(s)); pipeline log: {}", run_dir.display(), jobs.len(), run_dir.join("run.log").display());

    let mut results = Vec::new();
    for (k, job) in jobs.iter().enumerate() {
        let out_dir = run_dir.join(&job.id);
        print!("[{}/{}] {:<24} ", k + 1, jobs.len(), job.id);
        use std::io::Write;
        std::io::stdout().flush().ok();
        tracing::info!("════════ city_tests job {} ════════", job.id);
        let t = Instant::now();
        match run_job(job, &args, &svc, &out_dir).await {
            Ok(m) => {
                println!(
                    "ok  {:>5.0}s  {} | elementi {} (part {}) | sospesi {} | manifold {} | STL {:.1} MB{}",
                    t.elapsed().as_secs_f64(),
                    m["snapshot"]["mode"].as_str().unwrap_or("live"),
                    m["buildings"]["elements"],
                    m["buildings"]["parts"],
                    m["floating_solids"]["count"],
                    m["mesh"]["edges"]["manifold"],
                    m["stl"]["bytes"].as_f64().unwrap_or(0.0) / 1e6,
                    if m["osm"]["partial"].as_bool().unwrap_or(false) { "  ⚠ COPERTURA OSM PARZIALE" } else { "" },
                );
                if !job.patches.is_empty() && job.inject.is_empty() && m["osm"]["patched_elements"].as_u64() == Some(0) {
                    println!("      ⚠ nessun elemento toccato dalle patch: id sbagliato o fuori bbox?");
                }
                results.push((job.id.clone(), Ok(m)));
            }
            Err(e) => {
                println!("ERRORE: {e:#}");
                tracing::error!("job {} failed: {e:#}", job.id);
                results.push((job.id.clone(), Err(format!("{e:#}"))));
            }
        }
    }
    write_summary(&run_dir, &results)?;

    // `latest` → this run (relative symlink), only for default output dirs.
    if args.out.is_none() {
        let link = Path::new(OUTPUT_ROOT).join("latest");
        let _ = std::fs::remove_file(&link);
        #[cfg(unix)]
        let _ = std::os::unix::fs::symlink(&run_name, &link);
    }
    println!("riepilogo: {}", run_dir.join("summary.md").display());
    if results.iter().any(|(_, r)| r.is_err()) {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv_is_stable() {
        assert_eq!(fnv1a64(b""), "cbf29ce484222325");
        assert_eq!(fnv1a64(b"a"), "af63dc4c8601ec8c");
    }

    #[test]
    fn param_overrides_only_touch_given_fields() {
        let d = Params {
            resolution: 150,
            vertical_scale: 2.0,
            base_height: 2.0,
            print_size: 180.0,
            include_buildings: true,
            include_water: true,
            include_bridges: true,
            bridge_print_supports: true,
        };
        let p = d.with(&ParamOverrides { resolution: Some(200), include_water: Some(false), ..Default::default() });
        assert_eq!((p.resolution, p.include_water, p.print_size), (200, false, 180.0));
    }
}
