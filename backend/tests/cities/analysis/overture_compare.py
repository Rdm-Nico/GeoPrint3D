"""How complete is OSM? Count Overture Maps buildings per source (OSM,
Microsoft ML footprints, Google Open Buildings, Esri…) inside a bbox.
Overture merges OSM with machine-learned footprints, so the non-OSM share is
a direct measure of what OpenStreetMap is missing there.

Needs duckdb (not in anaconda):  python3 -m venv venv && venv/bin/pip install duckdb
Usage (from backend/):  venv/bin/python tests/cities/analysis/overture_compare.py taj_mahal rome
                        venv/bin/python tests/cities/analysis/overture_compare.py --bbox S,W,N,E
"""
import argparse
import json
import os
import sys

import duckdb

RELEASE = "2026-09-23.1"
HERE = os.path.dirname(os.path.abspath(__file__))
CITIES = os.path.join(os.path.dirname(HERE), "cities.json")


def query(con, bbox, min_area):
    # ST_Area_Spheroid expects (lat, lon) axis order; Overture stores (lon, lat).
    path = f"s3://overturemaps-us-west-2/release/{RELEASE}/theme=buildings/type=building/*"
    rows = con.execute(f"""
        SELECT sources[1].dataset AS src,
               count(*) AS n,
               sum(ST_Area_Spheroid(ST_FlipCoordinates(geometry))) AS area_m2,
               count(*) FILTER (WHERE ST_Area_Spheroid(ST_FlipCoordinates(geometry)) >= {min_area}) AS n_printable
        FROM read_parquet('{path}', hive_partitioning=1)
        WHERE bbox.xmin <= {bbox['max_lon']} AND bbox.xmax >= {bbox['min_lon']}
          AND bbox.ymin <= {bbox['max_lat']} AND bbox.ymax >= {bbox['min_lat']}
        GROUP BY src ORDER BY n DESC
    """).fetchall()
    return [{"source": r[0], "buildings": r[1], "area_m2": round(r[2] or 0), "above_filter": r[3]} for r in rows]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("cities", nargs="*")
    ap.add_argument("--bbox", help="S,W,N,E")
    ap.add_argument("--min-area", type=float, default=0.0, help="footprint filter (m²) for the 'above_filter' column")
    a = ap.parse_args()
    con = duckdb.connect()
    for stmt in ("INSTALL spatial", "LOAD spatial", "INSTALL httpfs", "LOAD httpfs",
                 "SET s3_region='us-west-2'", "SET s3_access_key_id=''", "SET s3_secret_access_key=''"):
        con.execute(stmt)
    targets = []
    if a.bbox:
        s, w, n, e = map(float, a.bbox.split(","))
        targets.append(("bbox", {"min_lat": s, "min_lon": w, "max_lat": n, "max_lon": e}))
    cities = {c["id"]: c for c in json.load(open(CITIES))["cities"]}
    for cid in a.cities:
        targets.append((cid, cities[cid]["bbox"]))
    out = {}
    for label, bbox in targets:
        rows = query(con, bbox, a.min_area)
        total = sum(r["buildings"] for r in rows) or 1
        osm = sum(r["buildings"] for r in rows if r["source"] == "OpenStreetMap")
        out[label] = {"bbox": bbox, "release": RELEASE, "by_source": rows,
                      "osm_share": round(osm / total, 3)}
        print(f"## {label} (Overture {RELEASE})")
        for r in rows:
            print(f"- {r['source']}: {r['buildings']} edifici, {r['area_m2'] / 1e3:.0f}k m² "
                  f"(≥{a.min_area:g} m²: {r['above_filter']})")
        print(f"- quota OSM: {100 * osm / total:.1f}%\n")
    json.dump(out, sys.stderr, indent=1)


if __name__ == "__main__":
    main()
