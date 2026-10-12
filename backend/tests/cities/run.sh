#!/usr/bin/env bash
# Genera i luoghi di test fissi (cities.json) direttamente in STL, solo backend.
# Uso: tests/cities/run.sh [--only rome,paris] [--group cities|landmark|all]
#      [--refresh] [--experiments] [--no-render] …   (dettagli: --help, README.md)
set -euo pipefail
cd "$(dirname "$0")/../.."   # backend/ — .env (MAPBOX_TOKEN) e i percorsi relativi partono da qui
# Compila (se serve) e poi esegue il binario: se cargo resta in attesa del lock
# della build (es. rust-analyzer aperto in VS Code) il messaggio resta visibile.
cargo build --release --quiet --bin city_tests
exec target/release/city_tests "$@"
