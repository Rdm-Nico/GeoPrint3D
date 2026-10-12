"""Audit of the city snapshots: what the backend drops or misreads, and why.

Usage (from backend/):
    /opt/anaconda3/bin/python3 tests/cities/analysis/audit.py [city ...] [--out DIR]

Needs shapely ≥ 2 (anaconda has it). Sections:
  tags      height / min_height / levels values the Rust parser cannot read
  s3db      outlines with building:part inside — current rule vs proposed rule
  filter    elements below the print-resolution footprint filter
  relations building multipolygons arriving incomplete from a tile
  density   buildings per km² and footprint coverage on a 250 m grid
Results go to stdout (markdown) and DIR/audit_<city>.json.
"""
import argparse
import collections
import json
import math
import os
import sys

from shapely.geometry import Polygon
from shapely.ops import unary_union

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import osmraw as R  # noqa: E402

METER_KEYS = ["height", "est_height", "min_height", "roof:height"]
LEVEL_KEYS = ["building:levels", "building:min_level", "roof:levels"]


def name(tags):
    return tags.get("name") or tags.get("name:en") or ""


# ── tags ─────────────────────────────────────────────────────────────────────

def audit_tags(els):
    bad = []
    total = collections.Counter()
    for kind, store in (("way", els.ways), ("relation", els.relations)):
        for eid, e in store.items():
            t = e["tags"]
            if not (R.is_building(t) or R.is_part(t)):
                continue
            for k in METER_KEYS + LEVEL_KEYS:
                if k not in t:
                    continue
                total[k] += 1
                ok = (R.parse_meters if k in METER_KEYS else R.parse_float)(t[k]) is not None
                if not ok:
                    h, mh, src = R.heights(t)
                    bad.append({"element": f"{kind}/{eid}", "key": k, "value": t[k], "name": name(t),
                                "resolved_height_m": round(h, 1), "height_source": src})
    by_key = collections.Counter(b["key"] for b in bad)
    comma = sum(1 for b in bad if "," in b["value"])
    return {"values_checked": dict(total), "unparsed": len(bad), "unparsed_by_key": dict(by_key),
            "unparsed_with_comma": comma, "examples": bad[:40]}


# ── s3db ─────────────────────────────────────────────────────────────────────

GROUND_SUPPRESS = 0.55  # current rule (osm.rs PART_COVERAGE_SUPPRESS_FRAC)
UNION_HIDE = 0.75       # proposed: the parts describe (most of) the whole building
SUPPORTED = 0.10        # a floating part rests on ≥10% of its area on parts spanning its base
PLINTH_MIN_AREA = 0.05  # unsupported parts that matter: ≥5% of the outline area
TOL_M = 1.0
PLINTH_MAX_FRAC = 0.5    # never fill more than half of the total height


def poly(proj, ring, holes=()):
    try:
        p = Polygon(proj(ring), [proj(h) for h in holes]).buffer(0)
        return p if p.area > 0.05 else None
    except Exception:
        return None


def bbox_of(ring):
    xs = [p[0] for p in ring]
    ys = [p[1] for p in ring]
    return (min(xs), min(ys), max(xs), max(ys))


def classify_outline(oh, otag_h, inside, og, oarea):
    """Current vs proposed (v2) treatment of one outline with parts.

    Proposed rule, in order:
      1. ground parts cover ≥55%                         → hide (today's rule)
      2. parts' union covers ≥75% and no large part
         floats unsupported                              → hide (S3DB: parts replace the outline)
      3. some large part (≥5% of the outline) floats with <10% of its area resting
         on a part spanning its base: F = max min_height of those parts
         - the outline's own (non-total) height reaches F → keep
         - F above half the total height                 → "da verificare" (no safe answer)
         - otherwise (outline lower than F, or its height
           is the TOTAL height = top of the highest part) → plinth: outline height := F
      4. otherwise                                       → keep (today's behaviour)
    """
    ground = sum(R.area_m2(p["ring"]) for p in inside if p["mh"] < 0.5) / oarea
    union = unary_union([p["g"] for p in inside]).intersection(og).area / og.area
    max_top = max(p["h"] for p in inside)
    unsupported = []
    for p in inside:
        if p["mh"] < 0.5 or p["g"].area < PLINTH_MIN_AREA * og.area:
            continue
        span = [q["g"] for q in inside
                if q is not p and q["mh"] <= p["mh"] + TOL_M and q["h"] >= p["mh"] - TOL_M]
        sup = p["g"].intersection(unary_union(span)).area / p["g"].area if span else 0.0
        if sup < SUPPORTED:
            unsupported.append((round(p["mh"], 1), p["id"], round(p["g"].area), round(sup, 2)))
    total = otag_h is not None and abs(otag_h - max_top) <= TOL_M
    current = "nascosto" if ground >= GROUND_SUPPRESS else "estruso"
    plinth = None
    if ground >= GROUND_SUPPRESS or (union >= UNION_HIDE and not unsupported):
        proposed = "nascosto"
    elif unsupported:
        f = max(u[0] for u in unsupported)
        if not total and oh >= f - TOL_M:
            proposed = "estruso"  # the outline's own body already reaches them
        elif f > PLINTH_MAX_FRAC * max_top:
            # A plinth that high would be a fat block (tower + podium mapped
            # as one outline, Dubai): no safe automatic answer.
            proposed = "da verificare"
        else:
            proposed, plinth = f"plinto {f:.0f} m", f
    else:
        proposed = "estruso"
    return ground, union, max_top, unsupported, total, current, proposed, plinth


def audit_s3db(els, bbox):
    fps = R.building_footprints(els)
    lat0 = (bbox["min_lat"] + bbox["max_lat"]) / 2
    proj = R.metric_projector(lat0)
    outlines = [f for f in fps if R.is_building(f[2])]
    parts = []
    for f in fps:
        if R.is_part(f[2]):
            g = poly(proj, f[3], f[4])
            if g is not None:
                h, mh, _ = R.heights(f[2])
                parts.append({"type": f[0], "id": f[1], "tags": f[2], "ring": f[3], "g": g, "h": h, "mh": mh,
                              "bb": bbox_of(f[3])})
    rows = []
    for (otype, oid, ot, oring, oholes) in outlines:
        ob = bbox_of(oring)
        near = [p for p in parts if p["bb"][0] <= ob[2] and p["bb"][2] >= ob[0] and p["bb"][1] <= ob[3] and p["bb"][3] >= ob[1]]
        inside = [p for p in near if sum(R.point_in_ring(v, oring) for v in p["ring"]) * 2 > len(p["ring"])]
        if not inside:
            continue
        og = poly(proj, oring, oholes)
        if og is None:
            continue
        oarea = R.area_m2(oring)
        oh, _, src = R.heights(ot)
        otag_h = R.parse_meters(ot.get("height"))
        ground, union, max_top, unsupported, total, current, proposed, plinth = classify_outline(
            oh, otag_h, inside, og, oarea)
        rows.append({
            "element": f"{otype}/{oid}", "name": name(ot), "kind": ot.get("building"),
            "area_m2": round(oarea), "parts": len(inside),
            "ground_cover": round(ground, 3), "union_cover": round(union, 3),
            "outline_height_tag": ot.get("height"), "outline_height_m": round(oh, 1), "outline_height_source": src,
            "outline_height_is_total": total, "max_part_top_m": round(max_top, 1),
            "unsupported_floating": unsupported,
            "current": current, "proposed": proposed, "plinth_m": plinth,
            "changes": current != proposed.split()[0] and proposed != "da verificare",
            "needs_review": proposed == "da verificare",
        })
    rows.sort(key=lambda r: -r["area_m2"])
    swallow = [r for r in rows if r["current"] == "estruso" and r["outline_height_m"] >= r["max_part_top_m"] - TOL_M]
    return {
        "outlines_with_parts": len(rows),
        "current_hidden": sum(r["current"] == "nascosto" for r in rows),
        "proposed_hidden": sum(r["proposed"] == "nascosto" for r in rows),
        "proposed_plinth": sum(r["proposed"].startswith("plinto") for r in rows),
        "changed": [r for r in rows if r["changes"]],
        "needs_review": [r for r in rows if r["needs_review"]],
        "extruded_at_or_above_all_parts": len(swallow),
        "rows": rows,
    }


# ── filter ───────────────────────────────────────────────────────────────────

def audit_filter(els, bbox):
    min_area, mm_per_m = R.min_area_m2(bbox)
    fps = R.building_footprints(els)
    dropped = {"outline": 0, "part": 0}
    kept = {"outline": 0, "part": 0}
    notable = []
    for (t, eid, tags, ring, holes) in fps:
        clipped = R.clip_ring(ring, bbox)
        if len(clipped) < 3:
            continue
        a = R.area_m2(clipped)
        cls = "part" if R.is_part(tags) else "outline"
        if a < min_area:
            dropped[cls] += 1
            h, mh, _ = R.heights(tags)
            if cls == "part" and (h >= 20 or mh >= 5):
                notable.append({"element": f"{t}/{eid}", "area_m2": round(a, 1), "height_m": round(h, 1),
                                "min_height_m": round(mh, 1), "roof": tags.get("roof:shape"), "name": name(tags),
                                "printed_width_mm": round(math.sqrt(a) * mm_per_m, 2)})
        else:
            kept[cls] += 1
    notable.sort(key=lambda n: -n["height_m"])
    return {"min_area_m2": round(min_area, 1), "mm_per_m": round(mm_per_m, 4),
            "kept": kept, "dropped": dropped, "tall_parts_dropped": notable[:40],
            "tall_parts_dropped_count": len(notable)}


# ── relations ────────────────────────────────────────────────────────────────

def audit_relations(raw):
    """Building multipolygons per tile: is the FIRST copy (the one the
    backend keeps, marking the id seen) complete? Is a complete copy shadowed?"""
    copies = collections.defaultdict(list)  # rid -> [(tile, missing, open, rings, tags)]
    for t in raw["tiles"]:
        label = ".".join([str(t["top"] + 1)] + [str(q) for q in t["path"] if q])
        els = t["data"].get("elements", [])
        if t["format"] == "noderefs":
            nodes = {e["id"]: (e["lon"], e["lat"]) for e in els if e["type"] == "node"}
            wc = {e["id"]: [nodes[n] for n in e.get("nodes", []) if n in nodes] for e in els if e["type"] == "way"}
        for e in els:
            tags = e.get("tags", {})
            if e["type"] != "relation" or tags.get("type") != "multipolygon":
                continue
            if not (R.is_building(tags) or R.is_part(tags)):
                continue
            segs, missing = [], 0
            for m in e.get("members", []):
                if m["type"] != "way":
                    continue
                if t["format"] == "geom":
                    c = [(g["lon"], g["lat"]) for g in m.get("geometry") or [] if g]
                else:
                    c = wc.get(m["ref"])
                if not c or len(c) < 2:
                    missing += 1
                elif m.get("role") != "inner":
                    segs.append(c)
            rings, open_ = R.assemble_rings(segs)
            copies[e["id"]].append((label, missing, open_, rings, tags))
    incomplete_first, shadowed, never_complete = [], [], []
    for rid, cs in copies.items():
        first = cs[0]
        if first[1] + first[2] == 0:
            continue
        complete = [c for c in cs[1:] if c[1] + c[2] == 0]
        area = sum(R.area_m2(r) for r in (complete[0][3] if complete else first[3]))
        row = {"relation": rid, "name": name(first[4]), "first_tile": first[0], "missing_members": first[1],
               "open_chains": first[2], "rings_kept": len(first[3]), "copies": len(cs)}
        incomplete_first.append(row)
        if complete:
            row["complete_in_tile"] = complete[0][0]
            row["area_lost_m2"] = round(area - sum(R.area_m2(r) for r in first[3]))
            shadowed.append(row)
        else:
            never_complete.append(row)
    return {"building_relations": len(copies), "first_copy_incomplete": len(incomplete_first),
            "complete_copy_shadowed": len(shadowed), "never_complete": len(never_complete),
            "shadowed": shadowed[:30], "never_complete_examples": never_complete[:30]}


# ── density ──────────────────────────────────────────────────────────────────

def audit_density(els, bbox, cell_m=250.0):
    fps = [f for f in R.building_footprints(els) if R.is_building(f[2])]
    lat0 = (bbox["min_lat"] + bbox["max_lat"]) / 2
    w = (bbox["max_lon"] - bbox["min_lon"]) * 111320.0 * math.cos(math.radians(lat0))
    h = (bbox["max_lat"] - bbox["min_lat"]) * 111320.0
    nx, ny = max(1, int(w // cell_m)), max(1, int(h // cell_m))
    cells = [[0.0] * nx for _ in range(ny)]
    total = 0.0
    n = 0
    for (_, _, tags, ring, _) in fps:
        clipped = R.clip_ring(ring, bbox)
        if len(clipped) < 3:
            continue
        a = R.area_m2(clipped)
        total += a
        n += 1
        cx = sum(p[0] for p in clipped) / len(clipped)
        cy = sum(p[1] for p in clipped) / len(clipped)
        ix = min(nx - 1, int((cx - bbox["min_lon"]) / (bbox["max_lon"] - bbox["min_lon"]) * nx))
        iy = min(ny - 1, int((cy - bbox["min_lat"]) / (bbox["max_lat"] - bbox["min_lat"]) * ny))
        cells[iy][ix] += a
    cell_area = (w / nx) * (h / ny)
    cov = sorted(c / cell_area for row in cells for c in row)
    area_km2 = w * h / 1e6
    q = lambda p: cov[min(len(cov) - 1, int(p * (len(cov) - 1)))]
    return {"area_km2": round(area_km2, 2), "outlines": n, "per_km2": round(n / area_km2),
            "coverage_pct": round(100 * total / (w * h), 1),
            "cells": len(cov), "cells_below_2pct": sum(c < 0.02 for c in cov),
            "cell_coverage_p25_p50_p75_pct": [round(100 * q(p), 1) for p in (0.25, 0.5, 0.75)],
            "grid": [[round(100 * c / cell_area) for c in row] for row in cells]}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("cities", nargs="*")
    ap.add_argument("--out", default=None)
    a = ap.parse_args()
    cities = R.load_cities()
    ids = a.cities or [c for c in cities if os.path.exists(os.path.join(R.SNAPSHOTS, c, "osm_raw.json.gz"))]
    out_dir = a.out or os.path.join(R.CITIES_DIR, "output", "audit")
    os.makedirs(out_dir, exist_ok=True)
    for cid in ids:
        raw = R.load_raw(cid)
        bbox = cities[cid]["bbox"]
        els = R.Elements(raw)
        res = {"city": cid, "recorded_at": raw["recorded_at"],
               "tiles": [(t["top"], t["path"], t["source"]) for t in raw["tiles"]],
               "tags": audit_tags(els), "s3db": audit_s3db(els, bbox), "filter": audit_filter(els, bbox),
               "relations": audit_relations(raw), "density": audit_density(els, bbox)}
        with open(os.path.join(out_dir, f"audit_{cid}.json"), "w") as f:
            json.dump(res, f, indent=1, ensure_ascii=False)
        t, s, fl, rl, d = res["tags"], res["s3db"], res["filter"], res["relations"], res["density"]
        print(f"## {cid} (snapshot {raw['recorded_at'][:16]})")
        print(f"- tag non letti: {t['unparsed']} su {sum(t['values_checked'].values())} "
              f"(con virgola: {t['unparsed_with_comma']}) {t['unparsed_by_key']}")
        print(f"- outline con part: {s['outlines_with_parts']} | nascosti ora {s['current_hidden']} → "
              f"proposta {s['proposed_hidden']} (+ plinto {s['proposed_plinth']}) | cambiano {len(s['changed'])} | "
              f"estrusi alti quanto/più delle part: {s['extruded_at_or_above_all_parts']}")
        for r in s["changed"][:12] + s["needs_review"][:5]:
            print(f"    · {r['element']} {r['name'][:38]!r} area {r['area_m2']} m², part {r['parts']}, "
                  f"terra {r['ground_cover']:.0%}, unione {r['union_cover']:.0%}: {r['current']} → {r['proposed']}")
        print(f"- filtro ≥{fl['min_area_m2']} m² ({fl['mm_per_m']} mm/m): scartati outline {fl['dropped']['outline']}"
              f"/{fl['dropped']['outline'] + fl['kept']['outline']}, part {fl['dropped']['part']}"
              f"/{fl['dropped']['part'] + fl['kept']['part']} (part alte/sospese scartate: {fl['tall_parts_dropped_count']})")
        print(f"- relazioni edificio: {rl['building_relations']} | prima copia incompleta {rl['first_copy_incomplete']} "
              f"(copia completa oscurata {rl['complete_copy_shadowed']}, mai completa {rl['never_complete']})")
        print(f"- densità: {d['outlines']} outline, {d['per_km2']}/km², copertura {d['coverage_pct']}%, "
              f"celle 250 m < 2%: {d['cells_below_2pct']}/{d['cells']}, p25/p50/p75 {d['cell_coverage_p25_p50_p75_pct']}%")
        print()


if __name__ == "__main__":
    main()
