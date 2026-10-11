# Lavori futuri

Elenco dei lavori rimandati, con il contesto necessario per riprenderli.
Ogni voce indica dove si trova il codice e che cosa è già stato verificato.

---

## Efficienze da poter aggiungere:

## Caching delle tails

**sintomo**: se l'utente fa una rendering di una zona e poi ne fa un altro molto simile, attualemnte viene fatto un altro flusso completo che porta a ulteriore sforzo computazionale e ti tempo.
**Idea** si potrebbe fare un caching del risultato ottenuto alla fine della generazione della mappa per tails. questi risultati vengono salvati per un tempo breve(5min) e vengono confrontati con le bbox che l'utente disegna:

1. Con una sovrapposizione completa ed anzi un bbox più piccola di quella già fatta si può rimpicciolire semplicemente l'area da quel risultato e fornire quello in output.
2. Se si ha una sovrapposizione laterale per una o più tile, si può ipotizzare di utilizzare la tile come risultato giá calcolato e fare il calcolo solo delle altri. In questo caso bisognerebbe capire se é fattibile fare un merging di questi due risultati realizzati in due momenti diversi ed se é facile tagliare così l'area.
3. Se invece non é possibile fare una correlazione effettiva tra la nuova bbox dell'utente e quella cachata oppure il lavoro di divisione e unioni di più tails supera una certa soglia (tipo il 30% o di meno di sovrapposizione può portare a troppo lavoro rispetto a semplicemente rifare tutto il flusso).

In ogni caso sarebbe da testare se si pùo fare perché al momento é solo un idea.

## Acqua (`feat/water`)

### 1. Canali di Venezia a -2 m, laguna a 0 m (aperto)

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

- Usare il corpo _più grande_ tra quelli toccati invece del minimo.
- Oppure alzare la soglia di affidabilità del corpo (oltre 12.400 m²).
- Oppure, come regola più semplice, trattare `water=lagoon` come livello 0
  (la laguna è sempre a livello del mare).

**Come verificare.** Venezia, bbox `45.4201,12.2976 → 45.4522,12.3619`.
Log temporaneo che stampa i corpi toccati dal gruppo canali
(`members > 100`). Criterio: canali e laguna entro ~0,1 m di livello.

### 2. Export 3MF con tre livelli separati: acqua, edifici, terreno (da fare)

**Obiettivo.** Esportare un file `.3mf` in cui acqua, edifici e terreno sono
tre oggetti distinti, ciascuno con il proprio colore o materiale. Serve per
stampanti multi-colore e per poter assegnare colori diversi nello slicer
senza ridisegnare il modello.

**Stato attuale.** Esiste solo l'export STL (`backend/src/utils/export.rs`,
`export_stl`). Nel mesh restituito all'API le tre classi sono già distinguibili
per intervalli di triangoli:

- `triangles[0 .. terrain - water]` → terreno
- `triangles[terrain - water .. terrain]` → acqua
- `triangles[terrain .. terrain + buildings]` → edifici (impalcati, pile e
  torri dei ponti compresi)
- `triangles[terrain + buildings .. + supports]` → supporti removibili sotto
  le campate dei ponti (`support_triangle_count`, `feat/bridges`)
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

## Ponti (`feat/bridges`)

### 1. Export 3MF con support enforcer per le campate dei ponti (da fare)

**Obiettivo.** Oggi le campate più lunghe di 10 mm stampati ricevono colonnine
removibili integrate nella geometria (`BridgeBuild::emit_columns` in
`backend/src/utils/bridges.rs`, range `support_triangle_count`). L'alternativa
è non toccare la geometria e marcare nel 3MF le zone sotto gli impalcati come
*support enforcer*: lo slicer genera i suoi supporti solo lì, con le
impostazioni scelte dall'utente (interfaccia, Z-gap, pattern).

**Perché non subito.**

- Lo STL non porta metadati: senza 3MF non c'è modo di dire allo slicer dove
  supportare.
- PrusaSlicer, OrcaSlicer e Bambu Studio salvano enforcer e modificatori in
  config 3MF proprietarie e incompatibili tra loro; Cura usa un meccanismo
  ancora diverso (mesh di tipo "support mesh"). Servirebbe un writer per
  slicer, o la scelta di uno solo.
- In PrusaSlicer "Don't support bridges" è attivo di default e in passato
  annullava gli enforcer proprio sotto una campata appoggiata su due torri.
  In Orca gli enforcer hanno la precedenza. Il risultato dipende quindi dallo
  slicer e dalla versione.
- Le colonnine integrate funzionano ovunque, ma presuppongono layer da 0,2 mm
  (top gap `BREAKAWAY_TOP_GAP_MM`).

**Aggancio.** Si costruisce sopra il §2 dell'acqua (3MF a più oggetti): il
range dei supporti diventa un oggetto a parte, oppure viene sostituito dai
volumi enforcer.

**Fonti.**
[Prusa forum, Don't support bridges](https://forum.prusa3d.com/forum/prusaslicer/slicer-will-not-create-supports-where-they-are-needed),
[enforcer ignorati](https://forum.prusa3d.com/forum/prusaslicer/support-enforcer-not-building-support/),
[Orca: enforcer con precedenza](https://git.trueserve.org/PublicCodeMirror/OrcaSlicer-bambulab/commit/9f09f03228478aa77549efec845717b06e46cbc3),
[3MF Orca/Bambu incompatibili](https://3dpros.com/slicers/orcaslicer).

### 2. Cavi parametrici per i ponti sospesi (idea)

I cavi principali e i pendini non sono mappati in OSM (né Golden Gate, né
Brooklyn). Si potrebbero generare in modo parametrico tra le torri
(`BridgeSupport` con ruolo Tower) e le spalle: catenaria dal top delle torri
all'impalcato, spessore minimo stampabile 0,8 mm. Da valutare la
stampabilità: un cavo inclinato e sottile è uno sbalzo, quindi servirebbe
comunque un supporto.

### 3. Arricchimento offline da Wikidata / NBI (idea)

Solo il 2,6% dei `bridge:support` in OSM ha `height`. Wikidata e il National
Bridge Inventory USA hanno altezze e luci, ma una chiamata live per render
costa fino a 3,5 s ed eccede il budget OSM di 90 s. Se mai, va fatto offline
(tabella precalcolata per id OSM). Attenzione: unità miste, la proprietà
P2048 (altezza) è ambigua (impalcato o torre), P2793 è la luce libera sotto
l'impalcato (clearance), non l'altezza.
[Wikidata wbgetentities Q44440](https://www.wikidata.org/w/api.php?action=wbgetentities&ids=Q44440),
[NBI FeatureServer](https://geo.dot.gov/server/rest/services/Hosted/National_Bridge_Inventory/FeatureServer/0).

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
- **Schegge di terreno sul bordo bbox in mare aperto (trovato 2026-10-11).**
  Golden Gate, bbox `37.8080,-122.4870 → 37.8340,-122.4680`, stampa 180 mm:
  lungo il bordo ovest restano 117 triangoli "terra" larghi meno di 1 mm e
  lunghi più di 20 mm (fino a tutto il lato, y 1 → 179 mm). Uno ha un vertice
  a z ≈ 13 mm nell'angolo NW (colline di Marin) e gli altri a z = 0, quindi
  formano un muro inclinato visibile in vista obliqua. Il terreno è identico
  con `include_bridges=false` sullo stesso branch, quindi non dipende dai
  ponti; non è ancora stato confrontato con `main`. Ipotesi: la fascia tra il
  poligono del mare e il bordo della griglia viene triangolata come terra.
  Ci sono anche 237 triangoli terra rivolti verso il basso: possibili pieghe
  del terreno. Il controllo manifold conta solo i lati, quindi non le vede,
  mentre uno slicer sì.
- **Laguna come `Lake`.** La Laguna di Venezia è `natural=water` (non
  `natural=coastline`), quindi non è riconosciuta come mare. Il suo livello
  viene stimato dal DEM, che a Venezia è di tipo DSM. Collegato al punto 1.
- **Performance della laguna.** Lo step 5b su Venezia resta sotto 1,5 s con
  circa 1.100 buchi; da ricontrollare se cambia la soglia di affidabilità.
