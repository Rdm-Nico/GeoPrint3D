"""Compare two harness runs city by city (regression check).

For every id present in both runs: identical STL bytes? same triangle count?
max vertex difference (mm) and how many triangles moved; plus the manifest
metrics that matter (elements, parts, floating solids, manifold).

Usage (from backend/):
    /opt/anaconda3/bin/python3 tests/cities/analysis/compare_runs.py tests/cities/output/A tests/cities/output/B
"""
import json
import os
import sys

import numpy as np

TOL_MM = 1e-4  # differences below this are float noise (the bridge module is ±1 ULP between runs)


def load_stl(path):
    raw = open(path, "rb").read()
    n = int(np.frombuffer(raw[80:84], dtype="<u4")[0])
    rec = np.dtype([("n", "<f4", 3), ("v", "<f4", (3, 3)), ("a", "<u2")])
    return np.frombuffer(raw[84:], dtype=rec, count=n)["v"]


def main(a, b):
    ids = sorted(set(os.listdir(a)) & set(os.listdir(b)))
    print(f"| id | STL | triangoli | max Δ (mm) | triangoli spostati | elementi | part | sospesi | manifold |")
    print("|---|---|---|---|---|---|---|---|---|")
    for cid in ids:
        ma, mb = os.path.join(a, cid, "manifest.json"), os.path.join(b, cid, "manifest.json")
        if not (os.path.exists(ma) and os.path.exists(mb)):
            continue
        ja, jb = json.load(open(ma)), json.load(open(mb))
        sa, sb = load_stl(os.path.join(a, cid, ja["stl"]["file"])), load_stl(os.path.join(b, cid, jb["stl"]["file"]))
        if ja["stl"]["fnv1a64"] == jb["stl"]["fnv1a64"]:
            verdict, dmax, moved = "identico", 0.0, 0
        elif sa.shape == sb.shape:
            d = np.abs(sa - sb).max(axis=(1, 2))
            dmax, moved = float(d.max()), int((d > TOL_MM).sum())
            verdict = "uguale (rumore float)" if moved == 0 else "DIVERSO"
        else:
            verdict, dmax, moved = "DIVERSO", float("nan"), -1
        pair = lambda k1, k2: f"{ja[k1][k2]} → {jb[k1][k2]}" if ja[k1][k2] != jb[k1][k2] else f"{ja[k1][k2]}"
        print(f"| {cid} | {verdict} | {len(sa)} → {len(sb)} | {dmax:.2g} | {moved} | "
              f"{pair('buildings', 'elements')} | {pair('buildings', 'parts')} | "
              f"{pair('floating_solids', 'count')} | {jb['mesh']['edges']['manifold']} |")


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
