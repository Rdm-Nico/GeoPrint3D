"""Compose the comparison sheets used by ANALISI.md into report_assets/.

Usage (from backend/):
    /opt/anaconda3/bin/python3 tests/cities/analysis/make_report_assets.py \
        --baseline tests/cities/output/<run> \
        --experiments tests/cities/output/<run>-experiments \
        --proposta tests/cities/output/<run>-proposta
"""
import argparse
import os

from PIL import Image, ImageDraw, ImageFont

HERE = os.path.dirname(os.path.abspath(__file__))
ASSETS = os.path.join(os.path.dirname(HERE), "report_assets")
FONT = "/System/Library/Fonts/Helvetica.ttc"


def sheet(items, out, w=640, cols=2):
    font = ImageFont.truetype(FONT, 21) if os.path.exists(FONT) else None
    tiles = []
    for label, path in items:
        im = Image.open(path).convert("RGB")
        im = im.resize((w, int(im.height * w / im.width)), Image.LANCZOS)
        tile = Image.new("RGB", (w, im.height + 32), (18, 20, 28))
        tile.paste(im, (0, 32))
        ImageDraw.Draw(tile).text((10, 5), label, fill=(240, 240, 240), font=font)
        tiles.append(tile)
    th = max(t.height for t in tiles)
    rows = (len(tiles) + cols - 1) // cols
    canvas = Image.new("RGB", (cols * w, rows * th), (28, 32, 44))
    for i, t in enumerate(tiles):
        canvas.paste(t, ((i % cols) * w, (i // cols) * th))
    path = os.path.join(ASSETS, out)
    # 128-colour palette: flat-shaded renders lose nothing visible, files shrink ~3×.
    canvas.quantize(colors=128, method=Image.Quantize.MEDIANCUT).save(path, optimize=True)
    print(path, canvas.size)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--baseline", required=True)
    ap.add_argument("--experiments", required=True)
    ap.add_argument("--proposta", required=True)
    a = ap.parse_args()
    B, E, P = a.baseline, a.experiments, a.proposta
    os.makedirs(ASSETS, exist_ok=True)
    j = os.path.join

    sheet([("a) dati attuali: height=\"136,5\" → letto 16 m", j(E, "st_peter_a_attuale", "basilica_profile.png")),
           ("b) height=54 (dati v39 del 02/09/2026)", j(E, "st_peter_b_height54_v39", "basilica_profile.png")),
           ("c) height=136.5 (parser che accetta la virgola)", j(E, "st_peter_c_height136_5_punto", "basilica_profile.png")),
           ("d) outline eliminato (regola S3DB pura)", j(E, "st_peter_d_senza_outline", "basilica_profile.png"))],
          "san_pietro_esperimenti.png")
    sheet([("attuale (bbox utente, 4.7 km²)", j(B, "rome", "st_peter.png")),
           ("regola proposta simulata", j(P, "rome_proposta_s3db", "st_peter.png"))],
          "san_pietro_bbox_utente.png")
    sheet([("a) attuale: outline estruso → piramide", j(E, "eiffel_a_attuale", "tower_profile.png")),
           ("b) outline eliminato (regola S3DB)", j(E, "eiffel_b_senza_outline", "tower_profile.png")),
           ("a) attuale, vista obliqua", j(E, "eiffel_a_attuale", "tower.png")),
           ("b) outline eliminato, vista obliqua", j(E, "eiffel_b_senza_outline", "tower.png"))],
          "eiffel_esperimenti.png")
    sheet([("attuale (bbox utente, 9.4 km²)", j(B, "paris", "eiffel.png")),
           ("outline Eiffel eliminato", j(E, "paris_b_senza_outline_eiffel", "eiffel.png"))],
          "eiffel_bbox_utente.png")
    sheet([("bbox grande (11.8 km², filtro ≥75 m²)", j(B, "taj_mahal", "taj_profile.png")),
           ("bbox piccola (1.9 km², filtro ≥13 m²)", j(B, "taj_mahal_closeup", "taj_profile.png")),
           ("bbox grande: dintorni (Taj Ganj in basso)", j(B, "taj_mahal", "overview.png")),
           ("bbox piccola: vista ravvicinata", j(B, "taj_mahal_closeup", "taj.png"))],
          "taj_mahal_filtro.png")
    sheet([("attuale", j(B, "dubai", "business_bay.png") if os.path.exists(j(B, "dubai", "business_bay.png")) else j(B, "dubai", "overview.png")),
           ("regola proposta simulata", j(P, "dubai_proposta_s3db", "business_bay.png"))],
          "dubai_podi.png")
    sheet([("attuale: il Château manca (pianta, solo edifici)", j(B, "versailles", "chateau_plan.png")),
           ("relazione 1149002 caricata intera (simulazione)", j(E, "versailles_b_relazioni_intere", "chateau_plan.png"))],
          "versailles_chateau.png")
    sheet([("attuale: il Dubai Mall manca", j(B, "dubai", "burj_khalifa.png")),
           ("relazione 18195959 caricata intera (simulazione)", j(E, "dubai_b_relazioni_intere", "burj_khalifa.png"))],
          "dubai_mall.png")
    sheet([(f"{c}", j(B, c, "overview.png")) for c in ("rome", "paris", "taj_mahal", "dubai", "barcelona", "versailles")],
          "citta_overview.png", w=560, cols=3)


if __name__ == "__main__":
    main()
