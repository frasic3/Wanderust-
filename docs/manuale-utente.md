# Georuggine — Manuale utente

App di tracciamento viaggi con mappa live di Torino e chat. Un server, più client (CLI o browser).

## Requisiti

- Rust + Cargo (build da sorgente)
- Solo per la mappa: `wasm-pack` + target `wasm32-unknown-unknown` (una volta)

## Installazione

```bash
cargo build --release -p server -p client
```

Binari in `target/release/` (`server`, `client`).

## Mappa (una tantum)

La pagina mostra un veicolo che si muove su una mappa di Torino. Servono due artefatti, da generare una sola volta:

1. Compila il simulatore in WebAssembly:

   ```bash
   rustup target add wasm32-unknown-unknown
   cargo install wasm-pack
   wasm-pack build sim --release --target web --out-dir ../web/pkg
   ```

2. Genera il grafo stradale (Python 3, solo stdlib; scarica da Overpass):

   ```bash
   python tools/export_turin_graph.py     # -> web/data/graph.json
   ```

Il grafo è già versionato nel repo; gli artefatti WASM no — rigenerali col comando sopra dopo ogni modifica al crate `sim`.

## Avvio

```bash
cargo run -p server     # terminale 1
cargo run -p client     # terminale 2
```

Server su `http://127.0.0.1:7878`. Apri quella URL nel browser per la mappa.

## Uso

**Client CLI** — comandi:

```
/register <utente> <password>
/login <utente> <password>
/start                 avvia viaggio
/end                   termina viaggio
/chat <testo>          broadcast  (@utente testo = messaggio privato)
/quit
```

**Browser** — bottoni Register / Login / Start trip / End trip / Chat / Disconnect. Apri più tab per simulare più utenti.

## Uso in LAN

Per collegare client da altri device sulla stessa rete.

1. Avvia il server con bind su tutte le interfacce:

   ```powershell
   $env:SERVER_ADDR="0.0.0.0:7878"; cargo run -p server     # Linux/macOS: SERVER_ADDR=0.0.0.0:7878 cargo run -p server
   ```

2. Trova l'IP del server:

   ```
   ipconfig
   ```

   Prendi l'IPv4 della rete Wi-Fi/Ethernet (`192.168.x.x` o `10.x.x.x`).

3. Dagli altri device apri `http://<IP-server>:7878` nel browser.
