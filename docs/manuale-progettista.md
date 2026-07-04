# Georuggine — Manuale del progettista

## Architettura

Workspace Cargo, 5 crate:

- `common` — protocollo `Message` (enum JSON tagged) + validatori, condiviso client/server
- `db` — pool SQLite (`sqlx`), migrations, API users/trips, sessioni in-memory
- `server` — server HTTP + WebSocket (`axum`): dispatch messaggi, broadcast chat, serve mappa e asset statici
- `client` — REPL CLI su WebSocket
- `sim` — simulatore movimento veicolo su grafo stradale di Torino, compilato a WebAssembly, gira nel browser

Un solo processo server, N client. Trasporto: WebSocket full-duplex su `/ws`, un frame `Text` per `Message` JSON. Porta unica 7878 serve anche pagina mappa (`/`), artefatti WASM (`/pkg`) e grafo (`/data`).

## Scelte progettuali

- **Rust** — sicurezza memoria + un solo linguaggio per server, client e simulatore (via WASM).
- **WebSocket** invece di HTTP polling — chat e posizioni richiedono push server→client in tempo reale.
- **axum + tokio** — async, gestisce molte connessioni concorrenti con pochi thread.
- **SQLite + sqlx** — zero setup, file locale, query verificate a compile-time; migrations versionate applicate al boot.
- **WASM per il simulatore** — stessa logica Rust eseguita nel browser, nessun plugin.
- **Enum `Message` tagged** — un solo tipo serializzato in JSON, parsing sicuro, protocollo esteso aggiungendo varianti.

## Dati

SQLite (`georuggine.db`), schema in `db/migrations/`. Tabelle: utenti, viaggi, posizioni. Foreign key con `ON DELETE CASCADE` da trip a user.

## Concorrenza e sessioni

Chat via canale broadcast tokio. Sessioni in-memory (token UUIDv4, validi per processo). Single-session policy: secondo login dello stesso utente → `ERROR AUTH_FAILED`.

## Sicurezza

- Password: hash bcrypt (cost 10)
- Token sessione: UUIDv4 in-memory
- Validazione input: regex username, lunghezza minima password, lunghezza massima chat
- FK SQLite con cascade
- Limiti noti: token in chiaro su `ws://` (Internet richiede TLS via reverse proxy), nessun rate-limiting sul login

## Test e benchmark

- **62 test** (`cargo test --workspace`): unit sui validatori (`common`), su `db` e `auth`, sul simulatore (`sim`); integrazione end-to-end (`server/tests/integration.rs`); concorrenza multi-client (`server/tests/concurrent.rs`).
- **Profiling CPU** integrato: `server/src/cpu_log.rs` campiona l'uso CPU del server ogni 2 min su `server_cpu.log`. Misure osservate: idle ~0.0–0.3%, picco ~1.7% durante attività.

### Stress test

Consumo CPU sotto registrazioni (`server/tests/stress.rs`, `#[ignore]`): utenti che si registrano uno alla volta a ritmo costante (`STRESS_TRICKLE_DELAY_MS`). Il test campiona CPU% del server (via `sysinfo`, normalizzata sui core logici) e latenza del register.

```powershell
cargo test --release --test stress -- --ignored --nocapture
```

Risultati (release, 16 core logici):

| Ritmo arrivi | CPU media | CPU picco | latenza media | latenza peggiore |
|---|---|---|---|---|
| 1 ogni 0,1 s (~5/s) | 2,8% | 3,5% | ~97 ms | ~135 ms |
| 1 ogni 1 s (~1/s) | 0,5% | 1,7% | ~102 ms | ~125 ms |

La latenza (~100 ms) è dominata da **bcrypt (cost 10)**: costo voluto (anti-brute-force), isolato su `spawn_blocking`. La CPU resta bassa (idle a regime ~0,02%); il floor è il costo per hash (~100 ms di un core).

## Valutazione

- Binari (Windows x86_64): `server.exe` ≈ 5.9 MB, `client.exe` ≈ 1.3 MB
- Uso CPU del server tracciato a runtime (`server/cpu_log.rs` → `server_cpu.log`)
- Cross-platform senza modifiche (Windows / Linux / macOS)
- Estensioni possibili: TLS/`wss://`, rate-limiting, sessioni persistite su DB, mappe di altre città
