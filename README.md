# Georuggine

App di tracciamento viaggi: un server, più client (CLI o browser). Mappa live di Torino con un veicolo che si muove su grafo stradale, autenticazione utenti e chat in tempo reale.

## Panoramica

Workspace Cargo con 5 crate:

- `common` — protocollo `Message` (enum JSON) + validatori
- `db` — SQLite via `sqlx`, migrations, sessioni in-memory
- `server` — HTTP + WebSocket (`axum`), serve mappa e asset
- `client` — REPL CLI su WebSocket
- `sim` — simulatore veicolo compilato a WebAssembly, gira nel browser

Trasporto: WebSocket full-duplex su `/ws`, porta `7878`. Storage: SQLite locale (`georuggine.db`).

## Quick start

```bash
cargo run -p server     # terminale 1
cargo run -p client     # terminale 2
```

Poi apri `http://127.0.0.1:7878` nel browser per la mappa.

## Test

```bash
cargo test --workspace
```

## Documentazione

- [Manuale utente](docs/manuale-utente.md) — installazione, avvio, comandi, uso in LAN
- [Manuale del progettista](docs/manuale-progettista.md) — architettura, scelte progettuali, protocollo, sicurezza, valutazione
