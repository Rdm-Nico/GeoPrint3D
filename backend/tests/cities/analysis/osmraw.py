"""Loader for the harness snapshots (tests/cities/snapshots/<city>/osm_raw.json.gz).

Mirrors what the Rust parser sees: tiles in TileKey order, both raw formats
(Overpass `out geom` and OSM-API `map.json` node refs), first-occurrence
dedupe by (type, id). Also replicates the tag parsing of `services/osm.rs`
(`parse_meters`, levels) so the audits count exactly what the backend drops.
"""
import gzip
import json
import math
import os

HERE = os.path.dirname(os.path.abspath(__file__))
CITIES_DIR = os.path.dirname(HERE)
SNAPSHOTS = os.path.join(CITIES_DIR, "snapshots")


def load_cities():
    with open(os.path.join(CITIES_DIR, "cities.json")) as f:
        return {c["id"]: c for c in json.load(f)["cities"]}


def load_raw(city_id):
    with gzip.open(os.path.join(SNAPSHOTS, city_id, "osm_raw.json.gz"), "rt") as f:
        raw = json.load(f)
    for t in raw["tiles"]:
        if "body" in t:  # v2: the HTTP body as received
            t["data"] = json.loads(t.pop("body"))
    raw["tiles"].sort(key=lambda t: (t["top"], t["path"]))
    return raw


# ── Tag parsing (replica of services/osm.rs) ─────────────────────────────────

def parse_meters(s):
    """Rust `parse_meters`: feet, ft'in", trailing m/spaces, a-b ranges."""
    if s is None:
        return None
    t = s.strip()
    lower = t.lower()
    for suf in ("feet", "ft"):
        if lower.endswith(suf):
            try:
                return float(lower[: -len(suf)].strip()) * 0.3048
            except ValueError:
                return None
    if "'" in t:
        ft, inch = t.split("'", 1)
        try:
            ft = float(ft.strip())
            inch = inch.strip().rstrip('"').strip()
            return (ft + (float(inch) if inch else 0.0) / 12.0) * 0.3048
        except ValueError:
            return None
    s2 = t.rstrip("m ").strip()
    if "-" in s2:
        a, b = s2.split("-", 1)
        try:
            return (float(a.strip()) + float(b.strip())) / 2.0
        except ValueError:
            pass
    try:
        return float(s2)
    except ValueError:
        return None


def parse_float(s):
    """Rust `s.trim().parse::<f32>()` (levels)."""
    if s is None:
        return None
    try:
        return float(s.strip())
    except ValueError:
        return None


DEFAULT_HEIGHT = {
    **dict.fromkeys(["house", "detached", "bungalow", "cabin", "hut", "static_caravan"], 6.0),
    **dict.fromkeys(["garage", "garages", "shed", "carport", "roof", "kiosk"], 3.0),
    **dict.fromkeys(["apartments", "residential", "dormitory", "terrace"], 15.0),
    **dict.fromkeys(["commercial", "office", "hotel"], 14.0),
    **dict.fromkeys(["retail", "supermarket", "warehouse"], 8.0),
    **dict.fromkeys(["industrial", "manufacture", "hangar"], 9.0),
    **dict.fromkeys(["church", "cathedral", "mosque", "temple", "chapel", "basilica"], 18.0),
    **dict.fromkeys(["school", "university", "college", "hospital", "public", "civic", "government"], 12.0),
    **dict.fromkeys(["tower", "skyscraper"], 40.0),
    "colonnade": 16.0,
}


def is_building(tags):
    return tags.get("building", "no") != "no"


def is_part(tags):
    return not is_building(tags) and tags.get("building:part", "no") != "no"


def heights(tags):
    """(height, min_height, source) as resolved by the backend (no jitter)."""
    part = is_part(tags)
    kind = tags.get("building:part" if part else "building", "yes")
    mh = parse_meters(tags.get("min_height"))
    if mh is None:
        lv = parse_float(tags.get("building:min_level"))
        mh = lv * 3.2 if lv is not None else 0.0
    mh = max(mh, 0.0)
    h = parse_meters(tags.get("height"))
    src = "tag"
    if h is None:
        h = parse_meters(tags.get("est_height"))
    if h is None:
        lv = parse_float(tags.get("building:levels"))
        if lv is not None:
            roof = parse_float(tags.get("roof:levels")) or 0.0
            h, src = max((lv + roof) * 3.2, 2.5), "levels"
        else:
            h, src = DEFAULT_HEIGHT.get(kind, 8.0), "default"
    else:
        h = max(h, 1.0)
    return max(h, mh + 0.5), mh, src


# ── Geometry ─────────────────────────────────────────────────────────────────

class Elements:
    """First-occurrence elements across the tiles (like the Rust dedupe)."""

    def __init__(self, raw):
        self.ways = {}       # id -> dict(tags, coords[(lon,lat)])
        self.relations = {}  # id -> dict(tags, members[(role, coords|None)])
        self.tile_of = {}    # (type,id) -> tile label of the first occurrence
        for t in raw["tiles"]:
            label = ".".join([str(t["top"] + 1)] + [str(q) for q in t["path"] if q])
            els = t["data"].get("elements", [])
            if t["format"] == "noderefs":
                nodes = {e["id"]: (e["lon"], e["lat"]) for e in els if e["type"] == "node"}
                way_coords = {
                    e["id"]: [nodes[n] for n in e.get("nodes", []) if n in nodes]
                    for e in els if e["type"] == "way"
                }
            for e in els:
                key = (e["type"], e["id"])
                tags = e.get("tags", {})
                if e["type"] == "way" and key not in self.tile_of:
                    if t["format"] == "geom":
                        coords = [(g["lon"], g["lat"]) for g in e.get("geometry") or [] if g]
                    else:
                        coords = way_coords.get(e["id"], [])
                    self.ways[e["id"]] = {"tags": tags, "coords": coords}
                    self.tile_of[key] = label
                elif e["type"] == "relation" and key not in self.tile_of:
                    members = []
                    for m in e.get("members", []):
                        if m["type"] != "way":
                            continue
                        if t["format"] == "geom":
                            c = [(g["lon"], g["lat"]) for g in m.get("geometry") or [] if g]
                        else:
                            c = way_coords.get(m["ref"])
                        members.append((m.get("role", ""), c if c and len(c) >= 2 else None))
                    self.relations[e["id"]] = {"tags": tags, "members": members}
                    self.tile_of[key] = label


def assemble_rings(segments):
    """Rust `assemble_rings_checked`: (closed rings, open chains)."""
    segs = [list(s) for s in segments]
    rings, open_ = [], 0
    near = lambda a, b: abs(a[0] - b[0]) < 1e-9 and abs(a[1] - b[1]) < 1e-9
    while segs:
        cur = segs.pop()
        while True:
            if len(cur) >= 4 and near(cur[0], cur[-1]):
                rings.append(cur[:-1])
                break
            found = None
            for i, s in enumerate(segs):
                if near(s[0], cur[-1]):
                    found = (i, False)
                    break
                if near(s[-1], cur[-1]):
                    found = (i, True)
                    break
            if found is None:
                open_ += 1
                break
            s = segs.pop(found[0])
            if found[1]:
                s.reverse()
            cur.extend(s[1:])
    return rings, open_


def area_m2(ring):
    """Rust `footprint_area_m2` (shoelace on lon/lat × cos(lat))."""
    n = len(ring)
    if n < 3:
        return 0.0
    a = sum(ring[i][0] * ring[(i + 1) % n][1] - ring[(i + 1) % n][0] * ring[i][1] for i in range(n))
    mean_lat = sum(p[1] for p in ring) / n
    return abs(a) / 2 * 111320.0 * 111320.0 * math.cos(math.radians(mean_lat))


def point_in_ring(p, ring):
    x, y = p
    inside = False
    n = len(ring)
    for i in range(n):
        x0, y0 = ring[i]
        x1, y1 = ring[(i + 1) % n]
        if (y0 > y) != (y1 > y) and x < x0 + (y - y0) / (y1 - y0) * (x1 - x0):
            inside = not inside
    return inside


def closed(coords):
    if len(coords) >= 4 and coords[0] == coords[-1]:
        return coords[:-1]
    return None


def building_footprints(els):
    """[(type, id, tags, outer_ring, holes)] for every building/part element
    whose rings close (relations: one entry per outer ring)."""
    out = []
    for wid, w in els.ways.items():
        t = w["tags"]
        if not (is_building(t) or is_part(t)):
            continue
        ring = closed(w["coords"]) or (w["coords"] if len(w["coords"]) >= 3 else None)
        if ring:
            out.append(("way", wid, t, ring, []))
    for rid, r in els.relations.items():
        t = r["tags"]
        if t.get("type") != "multipolygon" or not (is_building(t) or is_part(t)):
            continue
        outer, _ = assemble_rings([c for role, c in r["members"] if c and role != "inner"])
        inner, _ = assemble_rings([c for role, c in r["members"] if c and role == "inner"])
        for ring in outer:
            holes = [h for h in inner if h and point_in_ring(h[0], ring)]
            out.append(("relation", rid, t, ring, holes))
    return out


def metric_projector(lat0):
    kx = 111320.0 * math.cos(math.radians(lat0))
    return lambda ring: [(x * kx, y * 111320.0) for x, y in ring]


def min_area_m2(bbox, print_size=180.0):
    """Handler's print-resolution footprint filter (floored at 10 m²)."""
    center = math.radians((bbox["min_lat"] + bbox["max_lat"]) / 2)
    lat_m = (bbox["max_lat"] - bbox["min_lat"]) * 111320.0
    lon_m = (bbox["max_lon"] - bbox["min_lon"]) * 111320.0 * math.cos(center)
    mm_per_m = print_size / max(lat_m, lon_m, 1.0)
    return max(0.2 / (mm_per_m * mm_per_m), 10.0), mm_per_m


def clip_ring(ring, bbox):
    """Sutherland–Hodgman clip to the bbox (Rust `clip_ring_to_bbox`)."""
    pts = list(ring)
    for edge in range(4):
        if not pts:
            return pts
        def inside(p):
            return [p[0] >= bbox["min_lon"], p[0] <= bbox["max_lon"], p[1] >= bbox["min_lat"], p[1] <= bbox["max_lat"]][edge]
        def inter(a, b):
            if edge < 2:
                xv = bbox["min_lon"] if edge == 0 else bbox["max_lon"]
                t = (xv - a[0]) / (b[0] - a[0])
                return (xv, a[1] + t * (b[1] - a[1]))
            yv = bbox["min_lat"] if edge == 2 else bbox["max_lat"]
            t = (yv - a[1]) / (b[1] - a[1])
            return (a[0] + t * (b[0] - a[0]), yv)
        inp, pts = pts, []
        for i, cur in enumerate(inp):
            prev = inp[i - 1]
            if inside(prev) and inside(cur):
                pts.append(cur)
            elif inside(prev):
                pts.append(inter(prev, cur))
            elif inside(cur):
                pts.append(inter(prev, cur))
                pts.append(cur)
    return pts
