# Lavori futuri

Elenco dei lavori rimandati, con il contesto necessario per riprenderli.
Ogni voce indica dove si trova il codice e che cosa è già stato verificato.

---

## Acqua (`feat/water`)

### 1. Canali di Venezia a -2 m, laguna a 0 m  (aperto)

**Sintomo.** Con la Laguna di Venezia (relazione OSM 3049430) caricata, la rete
di canali della città si posiziona a circa -2,04 m, mentre la laguna sta a
circa -0,05 m. Canali e laguna dovrebbero stare allo stesso livello del mare
(0 m). A valle, i canali risultano incassati di circa 2 m (circa 0,2 mm in
stampa con scala verticale 0,105 mm/m).

**Causa individuata.** In `backend/src/utils/hydro.rs`, nel blocco che
livella le reti di canali (circa riga 440), il livello di una rete connessa
è il **minimo** dei livelli dei corpi "affidabili" che tocca:

```rust
let opens_onto = touched
    .iter()
    .filter(|&&k| is_reliable(&bodies[k]))
    .map(|&k| bodies[k].level)
    .fold(None, |acc, l| Some(acc.map_or(l, |a| a.min(l))));
```

Il corpo che vince è un piccolo lago di circa 12.400 m² (id OSM 960676911) a
-2,04 m, appena sopra la soglia `RELIABLE_BODY_CELLS × cell²` (10 celle). La
soglia è stata pensata per distinguere i laghi con campioni sufficienti; qui
il lago è troppo piccolo per essere un riferimento affidabile, ma il minimo
lo fa vincere comunque.

**Perché compare solo ora.** Prima del fix della relazione Laguna (vedi
`FetchStats.pending` in `osm.rs`) la laguna non veniva caricata, la rete di
canali usava il ramo di fallback (10° percentile delle stime
grezze) e restituiva 0,0 m. Il fix ha reso visibile il difetto, non lo ha
introdotto.

**Opzioni da valutare.**

- Usare il corpo *più grande* tra quelli toccati invece del minimo.
- Oppure alzare la soglia di affidabilità del corpo (oltre 12.400 m²).
- Oppure, come regola più semplice, trattare `water=lagoon` come livello 0
  (la laguna è sempre a livello del mare).

**Come verificare.** Venezia, bbox `45.4201,12.2976 → 45.4522,12.3619`.
Log temporaneo che stampa i corpi toccati dal gruppo canali
(`members > 100`). Criterio: canali e laguna entro ~0,1 m di livello.

### 2. Export 3MF con tre livelli separati: acqua, edifici, terreno  (da fare)

**Obiettivo.** Esportare un file `.3mf` in cui acqua, edifici e terreno sono
tre oggetti distinti, ciascuno con il proprio colore o materiale. Serve per
stampanti multi-colore e per poter assegnare colori diversi nello slicer
senza ridisegnare il modello.

**Stato attuale.** Esiste solo l'export STL (`backend/src/utils/export.rs`,
`export_stl`). Nel mesh restituito all'API le tre classi sono già distinguibili
per intervalli di triangoli:

- `triangles[0 .. terrain - water]` → terreno
- `triangles[terrain - water .. terrain]` → acqua
- `triangles[terrain .. terrain + buildings]` → edifici
- il resto → base/pedestallo

Il terreno è sempre prima dell'acqua e dei triangoli di base; vedi la
documentazione di `MeshData` in `backend/src/api/handlers.rs`.

**Note tecniche da considerare.**

- Un 3MF è uno zip con `3D/3dmodel.model` (XML) più metadati; è possibile
  scriverlo senza nuove dipendenze con il crate `zip` già disponibile o da
  aggiungere.
- Ogni classe va in un `<object>` separato dentro un `<resources>` e assemblata
  con `<build>`. Per i colori, usare `<basematerials>` o le proprietà
  `pid`/`pindex` del modello.
- L'acqua è incassata di `WATER_RECESS_MM` (0,6 mm) sotto la riva: nel 3MF
  dev'essere ancora un solido chiuso, altrimenti lo slicer la vede come
  superficie aperta. Le tre parti condividono i vertici di riva, quindi serve
  verificare che ogni oggetto sia manifold da solo.
- Decisione già presa con l'utente (memoria del progetto): STL resta il formato
  di default, il 3MF multi-colore è un'aggiunta.

**Criteri di accettazione.** Il file si apre in uno slicer (es. PrusaSlicer,
Bambu Studio) con tre oggetti; ogni oggetto è manifold; la somma dei volumi
coincide con il mesh di origine entro tolleranza.

---

## Da verificare / note

- **Mirror Overpass instabili.** Il 2026-10-08 i mirror rispondevano 500/429
  indipendentemente dalla query. Le fasi OSM lente di quel giorno non dipendono
  dalla query acqua.
- **Budget di tempo.** Il recupero delle relazioni incomplete ha una finestra
  propria di 15 s dopo la fase OSM (budget 90 s). Nel caso peggiore il totale è
  circa 105 s, contro i 120 s di timeout del frontend (`frontend/src/services/api.ts`).
  Da misurare su un'area grande.
- **Oceano aperto.** Un bbox senza alcuna linea di costa viene trattato come
  terra. È un limite noto e accettato per ora.
- **Laguna come `Lake`.** La Laguna di Venezia è `natural=water` (non
  `natural=coastline`), quindi non è riconosciuta come mare. Il suo livello
  viene stimato dal DEM, che a Venezia è di tipo DSM. Collegato al punto 1.
- **Performance della laguna.** Lo step 5b su Venezia resta sotto 1,5 s con
  circa 1.100 buchi; da ricontrollare se cambia la soglia di affidabilità.
