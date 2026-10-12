# Test delle città (solo backend)

Genera i luoghi di test **fissi** di [`cities.json`](cities.json) direttamente in **STL**, senza frontend né HTTP. Per ogni luogo produce anche anteprime PNG e un manifest diagnostico. L'analisi dei casi problematici (San Pietro, Torre Eiffel, Taj Mahal…) è in [`ANALISI.md`](ANALISI.md).

```bash
cd backend
tests/cities/run.sh                      # tutti i luoghi (città + close-up dei landmark)
tests/cities/run.sh --group cities       # solo le 6 città di concepts/cities.txt
tests/cities/run.sh --only rome,paris    # solo alcuni id
tests/cities/run.sh --refresh            # riscarica i dati live e aggiorna gli snapshot
tests/cities/run.sh --experiments        # esperimenti "what-if" sui dati (experiments.json)
tests/cities/run.sh --help               # tutte le opzioni
```

`run.sh` compila il binario `city_tests` (`src/bin/city_tests/`) e lo lancia da `backend/`, così `.env` (`MAPBOX_TOKEN`) viene letto come dal server. Il server resta `cargo run` (`default-run`).

Se `run.sh` resta fermo su *"Blocking waiting for file lock on build directory"* c'è un'altra build cargo in corso, per esempio rust-analyzer di VS Code. Basta aspettare che finisca.

## Luoghi fissi

| id | gruppo | area | provenienza bbox |
|---|---|---|---|
| `rome` | cities | 4.7 km² | run dell'utente 2026-10-12 (concepts/Rome_*.png) |
| `paris` | cities | 9.4 km² | run dell'utente (concepts/Paris_*.png) |
| `taj_mahal` | cities | 11.8 km² | run dell'utente (concepts/Taj_Mahal_*.png) |
| `dubai` | cities | 17.4 km² | run dell'utente |
| `barcelona` | cities | 5.7 km² | creata per la suite (Sagrada Família + Eixample) |
| `versailles` | cities | 6.2 km² | creata per la suite (Château + giardini + città) |
| `rome_st_peter` | landmark | 1.1 km² | close-up della basilica |
| `paris_eiffel` | landmark | 0.8 km² | close-up della torre |
| `taj_mahal_closeup` | landmark | 1.9 km² | prima run dell'utente sul Taj Mahal |

I parametri sono congelati in `defaults`: risoluzione 150, `vertical_scale` 2, base 2 mm, stampa 180 mm, edifici, acqua e ponti attivi, supporti di stampa attivi. Corrispondono ai default del frontend. Ogni luogo può sovrascriverli con `params`.

Ogni luogo ha anche una lista di `views` (PNG):
- `az` è la direzione da cui si guarda (180 = da sud);
- `el` è l'elevazione (90 = pianta);
- `center` + `radius_m` definiscono un close-up;
- `buildings_only` nasconde terreno e acqua, utile per vedere i profili dei landmark.

## Snapshot dei dati (riproducibilità)

Alla prima esecuzione, o con `--refresh`, i dati grezzi vengono salvati in `snapshots/<id>/`:
- `osm_raw.json.gz`: il **corpo HTTP esatto** di ogni tile OSM (Overpass o API diretta) e delle relazioni ricaricate intere;
- `elevation.json.gz`: la griglia di quota, salvata come bit IEEE.

Le esecuzioni successive **rigiocano** gli snapshot: niente rete, ~1–3 s per città, e lo stesso parser riceve gli stessi byte. Due run sullo stesso snapshot differiscono quindi **solo se cambia il codice**. Con `--refresh` invece si vede l'effetto delle modifiche fatte su OpenStreetMap.

Se la bbox di un luogo cambia in `cities.json`, lo snapshot non corrisponde più e viene registrato di nuovo in automatico. `--live` scarica senza toccare gli snapshot. `--source overpass|direct` forza una sola sorgente, per confrontare le sorgenti.

> Determinismo verificato il 12/10/2026: 8 luoghi su 9 sono identici bit per bit tra registrazione e replay. Dubai differisce di ±4.8·10⁻⁷ mm (1 ULP) in alcuni triangoli dei **ponti**: è una non-determinazione preesistente di `utils/bridges.rs`, vedi FUTURE_WORK. `analysis/compare_runs.py` la tratta come "rumore float".

## Output

`output/<AAAAMMGG-hhmmss>[-label]/`, più il link `output/latest` all'ultima run:

| file | contenuto |
|---|---|
| `<id>/<id>.stl` | modello stampabile (binary STL, mm) |
| `<id>/*.png` | anteprime (renderer z-buffer software, `src/bin/city_tests/render.rs`) |
| `<id>/manifest.json` | snapshot e sorgente dei tile, copertura OSM parziale, filtro impronte, scala/knee, conteggi, **solidi sospesi**, manifold, impronta FNV dell'STL, tempi |
| `<id>/geometry.json` | report geometrico per elemento: altezze, part, piazzamento (lo stesso di `logs/last_geometry.json` del server) |
| `summary.md` / `summary.json` | tabella riassuntiva della run |
| `run.log` | log completo della pipeline |

Le cartelle `output/` e `snapshots/` sono escluse da git. Gli STL lo sono comunque, tramite `*.stl` nel `.gitignore` di root.

**Solidi sospesi** sono gli elementi con `min_height` ≥ 0.5 m senza niente sotto il loro baricentro che arrivi alla loro base. È un'euristica, non una verifica geometrica completa, ma segnala bene i casi non stampabili, come la cupola di San Pietro sospesa.

## Esperimenti "what-if" sui dati

`experiments.json` rigioca lo snapshot di una città con:
- `patches`: tag aggiunti, cambiati o rimossi, oppure elementi eliminati (`{"element": "way/244159210", "set": {"height": "54"}}`, `"delete": true`);
- `inject`: risposte dell'API OSM, per esempio `relation/{id}/full.json` in `experiments_data/`, analizzate prima dei tile. Simulano il caricamento di un elemento intero.

Tutto avviene **prima** del parsing, con la pipeline invariata. Se un dato plausibile sistema il render, il difetto è nei dati; se nessun dato lo sistema, è nell'algoritmo.

## Analisi (Python)

Gli script in `analysis/` lavorano sugli snapshot e sugli output.

- **Python con shapely ≥ 2**: va bene `/opt/anaconda3/bin/python3`.
  - `audit.py`: tag non leggibili, outline con `building:part` (regola attuale contro regola proposta), filtro impronte, relazioni incomplete per tile, densità degli edifici. L'output va in `output/audit/`.
  - `propose_experiments.py`: trasforma la regola proposta in patch, quindi la si può renderizzare con `--experiments --experiments-file output/audit/experiments_proposta.json`.
  - `compare_runs.py A B`: regressione tra due run (STL identico, differenza massima dei vertici, conteggi).
  - `make_report_assets.py`: le figure di `report_assets/` usate in ANALISI.md.
- **duckdb** (non è in anaconda):
  ```bash
  python3 -m venv venv && venv/bin/pip install duckdb
  ```
  - `overture_compare.py`: conta gli edifici di Overture Maps per fonte (OSM, Google, Microsoft) in una bbox, per misurare quanto manca a OSM.
