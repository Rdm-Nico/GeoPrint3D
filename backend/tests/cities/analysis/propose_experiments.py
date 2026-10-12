"""Turn the audit's proposed outline/part rule into data patches, so its
effect can be rendered with the UNCHANGED pipeline (run.sh --experiments).

    hide    → delete the outline element
    plinth  → height := F, roof:shape := flat (roof:height removed)

Usage (from backend/, after audit.py):
    /opt/anaconda3/bin/python3 tests/cities/analysis/propose_experiments.py
    tests/cities/run.sh --experiments --experiments-file tests/cities/output/audit/experiments_proposta.json
"""
import glob
import json
import os

HERE = os.path.dirname(os.path.abspath(__file__))
AUDIT = os.path.join(os.path.dirname(HERE), "output", "audit")

experiments = []
for path in sorted(glob.glob(os.path.join(AUDIT, "audit_*.json"))):
    a = json.load(open(path))
    patches = []
    for r in a["s3db"]["changed"]:
        if r["proposed"] == "nascosto":
            patches.append({"element": r["element"], "delete": True})
        elif r["proposed"].startswith("plinto"):
            patches.append({"element": r["element"],
                            "set": {"height": f"{r['plinth_m']:g}", "roof:shape": "flat"},
                            "unset": ["roof:height", "roof:levels"]})
    if patches:
        experiments.append({
            "id": f"{a['city']}_proposta_s3db",
            "city": a["city"],
            "description": "Simulazione della regola outline/part proposta (v2): "
                           + "; ".join(f"{r['element']} {r['name'] or ''} → {r['proposed']}" for r in a["s3db"]["changed"]),
            "patches": patches,
        })
out = os.path.join(AUDIT, "experiments_proposta.json")
json.dump({"experiments": experiments}, open(out, "w"), indent=1, ensure_ascii=False)
print(out, len(experiments), "esperimenti")
