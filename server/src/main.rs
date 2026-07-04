use anyhow::Result;
use axum::{
    extract::{
        ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
        State,
    },
    response::{Html, IntoResponse},
    routing::get,
    Router,
};
use common::{decode, encode, validate_chat, Message};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, Mutex};
use tower_http::services::ServeDir;

use crate::status::UserStatus;
use crate::trips::{drop_trip, is_tracked, update_trip};

mod auth;
mod cpu_log;
mod status;
mod trips;
// Indirizzo TCP di default (host:porta) su cui il server si mette in ascolto
// se la variabile d'ambiente `SERVER_ADDR` non è impostata. `127.0.0.1` =
// loopback, accessibile solo dalla stessa macchina; per esporre all'esterno
// servirebbe `0.0.0.0`.
const ADDR_DEFAULT: &str = "127.0.0.1:7878";

// Capacità del canale `tokio::sync::broadcast` usato per inoltrare i messaggi
// di chat tra le task WebSocket. Se un client lento accumula più di 256
// messaggi non letti, i più vecchi vengono droppati (lagging receiver). Serve
// a evitare che un singolo client lento faccia crescere la memoria senza limiti.
const BROADCAST_CAP: usize = 256;

// Evento di chat che viaggia sul canale broadcast condiviso tra tutte le
// connessioni WebSocket. Pubblicato SOLO dal task di amministrazione (stdin
// del server): il client non chatta con altri client, parla solo col server.
// Ogni task per-client è iscritta al canale e inoltra al proprio socket.
//
// Perché un tipo dedicato invece di passare direttamente `common::Message`:
// serve sapere chi è il mittente (`from`) — qui sempre "SERVER".
#[derive(Debug, Clone)]
struct ChatBroadcast {
    from: String,
    text: String,
}

/// Registro globale dei sink WS per utente autenticato.
/// Usato dall'admin CLI (stdin) per recapitare DM a un utente specifico.
/// Popolato dopo LOGIN/REGISTER, rimosso al cleanup della connessione.
static SINKS: OnceLock<Mutex<HashMap<String, SharedSink>>> = OnceLock::new();
fn sinks_registry() -> &'static Mutex<HashMap<String, SharedSink>> {
    SINKS.get_or_init(|| Mutex::new(HashMap::new()))
}

// Stato condiviso dell'applicazione Axum, clonato in ogni handler.
// Contiene il `Sender` del canale broadcast: ogni task per-client lo clona
// per pubblicare nuovi messaggi e chiama `tx.subscribe()` per ottenere un
// proprio `Receiver` con cui ricevere i messaggi degli altri.
//
// `Clone` è richiesto da Axum perché lo stato viene clonato per ogni
// richiesta in arrivo; `broadcast::Sender` è cheap-clone (è un `Arc` interno).
#[derive(Clone)]
struct AppState {
    tx: broadcast::Sender<ChatBroadcast>,
    /// HTML della pagina di test, caricato una sola volta a startup.
    /// `Arc<str>` evita di rileggere il file da disco a ogni GET `/` e
    /// permette di clonare lo stato a costo zero (puntatore + refcount).
    index_html: Arc<str>,
}

/// Timestamp UNIX in secondi. Usato come fallback quando il client
/// si disconnette senza inviare un `ts` esplicito (es. chiusura WS
/// brutale prima dell'`EndTrip`).
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Individua la cartella `web/` provando i due path tipici (root del
/// workspace e sotto `server/`, a seconda della cwd con cui si lancia
/// `cargo run`). Da qui si serve sia `index.html` sia gli asset WASM in
/// `web/pkg/`. Fallback: `web` relativo alla cwd.
fn web_base() -> PathBuf {
    for p in [PathBuf::from("web"), PathBuf::from("../web")] {
        if p.join("index.html").exists() {
            return p;
        }
    }
    PathBuf::from("web")
}

/// Carica il contenuto di `index.html` dalla cartella web data. Se manca,
/// ritorna un fallback minimale così la GET `/` non risponde mai 500.
async fn load_index_html(base: &std::path::Path) -> String {
    match tokio::fs::read_to_string(base.join("index.html")).await {
        Ok(body) => body,
        Err(_) => "<h1>georuggine</h1><p>web/index.html non trovato</p>".to_string(),
    }
}

// Alias di tipo per ridurre il rumore nelle firme delle funzioni.
//
// `WsSink`: metà "scrittura" di una connessione WebSocket. `WebSocket` viene
// diviso con `.split()` in `SplitSink` (write) + `SplitStream` (read), così
// le due task (loop di lettura dal client e loop di scrittura verso il
// client) possono lavorare in parallelo senza contendersi il socket intero.
type WsSink = SplitSink<WebSocket, WsMessage>;

// `SharedSink`: il sink di scrittura condiviso fra più task della stessa
// connessione (es. il task che legge dal client e quello che riceve dal
// canale broadcast scrivono entrambi sullo stesso socket).
// - `Arc` = ownership condivisa fra task.
// - `Mutex` (di tokio, async) = serializza le scritture: due `send()`
//   concorrenti sullo stesso socket corromperebbero il framing WebSocket.
type SharedSink = Arc<Mutex<WsSink>>;

// `SharedUser`: username associato alla connessione, condiviso fra le task
// della stessa connessione. `Option<String>` perché all'inizio la connessione
// è anonima (nessun login) e viene popolata dopo `LOGIN`/`REGISTER` ok.
// Serve per filtrare DM e per sapere "chi sono" quando si pubblica in chat.
type SharedUser = Arc<Mutex<Option<String>>>;

/// Entry point del server.
/// Cosa fa: inizializza logger e DB, crea il canale broadcast della chat,
/// monta le route HTTP (`/` e `/ws`) e avvia Axum con graceful shutdown.
/// Perché: concentra tutto il bootstrap in un unico punto, così le task
/// successive ricevono uno stato già pronto (DB aperto, canale attivo).
#[tokio::main]
async fn main() -> Result<()> {
    // Inizializza il logger globale con formattazione di default.
    tracing_subscriber::fmt()
        // Legge il livello di log da `RUST_LOG` (es. "debug", "server=trace").
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                // Fallback: se `RUST_LOG` non è settata usa livello "info".
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        // Registra il subscriber come default globale per tutto il processo.
        .init();

    // Apre il pool SQLite e applica le migrations.
    db::ensure_file_exists().await?;

    let addr = std::env::var("SERVER_ADDR").unwrap_or_else(|_| ADDR_DEFAULT.to_string());
    let (tx, _rx) = broadcast::channel::<ChatBroadcast>(BROADCAST_CAP);
    let base = web_base();
    let index_html: Arc<str> = Arc::from(load_index_html(&base).await);
    let state = AppState { tx: tx.clone(), index_html };

    // `/pkg/*` serve gli artefatti WASM generati da wasm-pack (sim.js,
    // sim_bg.wasm). Sono file statici: una read da disco, costo CPU
    // trascurabile (la simulazione gira nel browser, non qui).
    let app = Router::new()
        .route("/", get(serve_index))
        .route("/ws", get(ws_upgrade))
        .route("/cpu_log", get(serve_cpu_log))
        .nest_service("/pkg", ServeDir::new(base.join("pkg")))
        .nest_service("/data", ServeDir::new(base.join("data")))
        .nest_service("/assets", ServeDir::new(base.join("assets")))
        .with_state(state);

    let listener = TcpListener::bind(&addr).await?;
    tracing::info!("server listening on http://{addr} (ws: /ws)");

    tokio::spawn(cpu_log::start_cpu_logger());
    tokio::spawn(admin_stdin(tx.clone()));

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    // Forza exit: il task admin_stdin gira su un thread bloccante di
    // tokio::io::stdin (Windows) che non si interrompe alla shutdown del
    // runtime, lasciando il processo appeso fino al prossimo Invio.
    tracing::info!("exit");
    std::process::exit(0);
}

/// Admin CLI: legge linee da stdin del server e le instrada come messaggi
/// di chat verso i client.
///
/// Sintassi:
/// - `@user testo`  → DM all'utente specifico (se connesso)
/// - `/list`        → elenca gli utenti connessi su stdout
/// - `<testo>`      → broadcast a tutti i client connessi
///
/// Il `from` dei messaggi inviati è sempre `"SERVER"`, così il client può
/// distinguerli dai propri (anche se ora il client non chatta più con altri
/// client: gli unici messaggi entranti provengono dal server).
async fn admin_stdin(tx: broadcast::Sender<ChatBroadcast>) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    println!("admin CLI pronta. Comandi: '@user testo' (DM), '/list', altrimenti broadcast.");
    loop {
        let line = match lines.next_line().await {
            Ok(Some(l)) => l,
            Ok(None) => break,
            Err(e) => {
                tracing::warn!("admin stdin error: {e}");
                break;
            }
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == "/list" {
            let map = sinks_registry().lock().await;
            if map.is_empty() {
                println!("(nessun utente connesso)");
            } else {
                let users: Vec<&String> = map.keys().collect();
                println!("connessi ({}): {users:?}", users.len());
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('@') {
            let Some((user, text)) = rest.split_once(char::is_whitespace) else {
                println!("usa: @user testo");
                continue;
            };
            let text = text.trim().to_string();
            if text.is_empty() {
                println!("messaggio vuoto");
                continue;
            }
            let sink_opt = sinks_registry().lock().await.get(user).cloned();
            let Some(sink) = sink_opt else {
                println!("utente '{user}' non connesso");
                continue;
            };
            let msg = Message::ChatFromServer {
                from: Some("SERVER".into()),
                text,
            };
            match encode(&msg) {
                Ok(line) => {
                    let mut s = sink.lock().await;
                    if let Err(e) = s.send(WsMessage::Text(line)).await {
                        println!("invio DM a '{user}' fallito: {e}");
                    } else {
                        println!("→ DM a '{user}' inviato");
                    }
                }
                Err(e) => println!("encode fallito: {e}"),
            }
            continue;
        }
        // Broadcast a tutti i client connessi.
        let n = sinks_registry().lock().await.len();
        let _ = tx.send(ChatBroadcast {
            from: "SERVER".into(),
            text: trimmed.to_string(),
        });
        println!("→ broadcast inviato ({n} client connessi)");
    }
}

/// Aspetta Ctrl-C e logga l'evento.
/// Perché: passato a `with_graceful_shutdown`, permette ad Axum di chiudere
/// le connessioni in modo pulito invece di terminare il processo a freddo.
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutdown signal received");
}

/// GET / → serve la pagina HTML di test dalla cache in memoria.
/// Il file viene letto una sola volta a startup (vedi `load_index_html`):
/// l'I/O su disco esce dal critical path delle richieste.
async fn serve_index(State(state): State<AppState>) -> impl IntoResponse {
    Html(state.index_html.to_string())
}

/// GET /cpu_log → restituisce il contenuto attuale di `server_cpu.log` (text/plain).
/// Letto a ogni richiesta: il file cresce ogni 2 minuti (cpu_log::start_cpu_logger).
async fn serve_cpu_log() -> impl IntoResponse {
    let body = tokio::fs::read_to_string("server_cpu.log")
        .await
        .unwrap_or_default();
    ([(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")], body)
}

/// GET /ws → handler dell'upgrade HTTP→WebSocket.
/// Cosa fa: accetta l'header `Upgrade: websocket`, completa l'handshake e
/// passa il socket aperto a `handle_client`. Logga eventuali errori del client
/// senza propagarli (una connessione rotta non deve abbattere il server).
/// Perché qui: è il punto in cui HTTP "diventa" WebSocket, da qui in poi il
/// protocollo è full-duplex su frame testuali JSON.
async fn ws_upgrade(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| async move {
        if let Err(e) = handle_client(socket, state.tx).await {
            tracing::warn!("client error: {e:?}");
        }
    })
}

/// Serializza un `Message` in JSON e lo spedisce sul WebSocket.
/// Perché il lock: il sink è condiviso (vedi `SharedSink`), va serializzato
/// per evitare frame interlacciati da task diverse sulla stessa connessione.
async fn send(sink: &SharedSink, msg: &Message) -> Result<()> {
    let line = encode(msg)?;
    let mut s = sink.lock().await;
    s.send(WsMessage::Text(line)).await?;
    Ok(())
}

/// Helper: invia un `Message::Error` con codice e descrizione al client.
/// Perché esiste: ridurre il boilerplate nei tanti rami di errore del dispatcher.
async fn send_error(sink: &SharedSink, code: &str, message: &str) -> Result<()> {
    send(
        sink,
        &Message::Error {
            code: code.into(),
            message: message.into(),
        },
    )
    .await
}

/// Loop principale per-connessione.
/// Cosa fa:
/// - splitta il WebSocket in read+write, condivide il write con `Arc<Mutex>`;
/// - spawn di una task secondaria (`chat_task`) iscritta al canale broadcast
///   che inoltra al socket i messaggi di altri utenti (filtro su `from != me`);
/// - dispatch dei messaggi entranti: Login/Register, StartTrip/EndTrip,
///   Chat, Disconnect; ogni operazione che richiede identità passa da
///   `validate_token`;
/// - cleanup finale: invalida il token e abortisce la chat task.
/// Perché due task: il reader è bloccato in `stream.next()` mentre arrivano
/// messaggi di chat da altri client; servono in parallelo, altrimenti la
/// chat sarebbe consegnata solo quando il client manda qualcosa.
async fn handle_client(ws: WebSocket, tx: broadcast::Sender<ChatBroadcast>) -> Result<()> {
    let (sink, mut stream): (WsSink, SplitStream<WebSocket>) = ws.split();
    let sink: SharedSink = Arc::new(Mutex::new(sink));

    let user_state: SharedUser = Arc::new(Mutex::new(None));

    let mut rx = tx.subscribe();
    let sink_for_chat = sink.clone();
    let user_state_chat = user_state.clone();
    let chat_task = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(bc) => {
                    let me = user_state_chat.lock().await.clone();
                    let Some(me) = me else { continue };
                    if bc.from == me {
                        continue;
                    }
                    let msg = Message::ChatFromServer {
                        from: Some(bc.from),
                        text: bc.text,
                    };
                    if let Ok(line) = encode(&msg) {
                        let mut s = sink_for_chat.lock().await;
                        if s.send(WsMessage::Text(line)).await.is_err() {
                            break;
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("chat lag: {n} messages dropped");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    loop {
        let ws_msg = match stream.next().await {
            Some(Ok(m)) => m,
            Some(Err(e)) => {
                tracing::info!("ws read error: {e}");
                break;
            }
            None => break,
        };

        let line = match ws_msg {
            WsMessage::Text(t) => t,
            WsMessage::Binary(b) => match String::from_utf8(b) {
                Ok(t) => t,
                Err(_) => {
                    send_error(&sink, "BAD_REQUEST", "payload binario non UTF-8").await?;
                    continue;
                }
            },
            WsMessage::Ping(_) | WsMessage::Pong(_) => continue,
            WsMessage::Close(_) => break,
        };

        if line.is_empty() {
            continue;
        }

        let msg = match decode(&line) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("parse error: {e}");
                send_error(&sink, "BAD_REQUEST", "messaggio non valido").await?;
                continue;
            }
        };

        match msg {
            Message::Login { ref username, .. } => {
                let current_user = user_state.lock().await.clone();
                // Già autenticato come questo utente su questa connessione: un
                // altro /login è ridondante. Rifiuta invece di emettere un nuovo
                // token (niente AUTH_OK multipli per lo stesso utente).
                if current_user.as_deref() == Some(username.as_str()) {
                    send_error(
                        &sink,
                        "ALREADY_AUTHENTICATED",
                        &format!("già autenticato come {username}"),
                    )
                    .await?;
                    continue;
                }
                // Altrimenti è uno switch di account (o un login su connessione
                // anonima). La sessione precedente si invalida solo se il nuovo
                // login riesce: un tentativo fallito (es. password errata) non
                // scollega l'utente già autenticato sulla connessione.
                match auth::login(&msg).await {
                    Ok((username, token)) => {
                        if let Some(u) = current_user.as_deref() {
                            db::invalidate_user_sessions(u).await;
                            status::remove(u).await;
                            sinks_registry().lock().await.remove(u);
                        }
                        *user_state.lock().await = Some(username.clone());
                        status::set(&username, UserStatus::Stationary).await;
                        sinks_registry().lock().await.insert(username.clone(), sink.clone());
                        send(&sink, &Message::AuthOk { token }).await?;
                    }
                    Err(e) => {
                        send_error(&sink, "AUTH_FAILED", &e.to_string()).await?;
                    }
                }
            }
            Message::Register { .. } => {
                // Register: il nome utente è sempre nuovo, quindi non può mai
                // essere lo "stesso utente" già autenticato. Invalidiamo la
                // sessione corrente solo dopo che il register è andato a buon
                // fine, così un fallimento (es. utente già esistente) non
                // disconnette l'eventuale sessione attiva.
                let current_user = user_state.lock().await.clone();
                match auth::register(&msg).await {
                    Ok((username, token)) => {
                        if let Some(u) = current_user.as_deref() {
                            db::invalidate_user_sessions(u).await;
                            status::remove(u).await;
                            sinks_registry().lock().await.remove(u);
                        }
                        *user_state.lock().await = Some(username.clone());
                        status::set(&username, UserStatus::Stationary).await;
                        sinks_registry().lock().await.insert(username.clone(), sink.clone());
                        send(&sink, &Message::AuthOk { token }).await?;
                    }
                    Err(e) => {
                        send_error(&sink, "REGISTER_FAILED", &e.to_string()).await?;
                    }
                }
            }
            Message::StartTrip {
                token,
                lat,
                lon,
                ts,
            } => {
                let user = match validate_token(&token, &user_state).await {
                    Some(u) => u,
                    None => {
                        send_error(&sink, "UNAUTHORIZED", "token non valido").await?;
                        continue;
                    }
                };

                if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
                    send_error(&sink, "BAD_REQUEST", "coordinate fuori range").await?;
                    continue;
                }
                if ts <= 0 {
                    send_error(&sink, "BAD_REQUEST", "timestamp non valido").await?;
                    continue;
                }

                match trips::initialize_trips(user.clone(), lat, lon, ts).await {
                    Ok(trip_id) => {
                        tracing::info!(user = %user, trip_id, ts, "viaggio avviato");
                        send(
                            &sink,
                            &Message::TripStarted {
                                trip_id,
                                lat,
                                lon,
                                ts,
                            },
                        )
                        .await?;
                    }
                    Err(e) => {
                        send_error(&sink, "START_TRIP_FAILED", &e).await?;
                    }
                }
            }
            Message::Position {
                token,
                trip_id,
                lat,
                lon,
                ts,
            } => {
                let user = match validate_token(&token, &user_state).await {
                    Some(u) => u,
                    None => {
                        send_error(&sink, "UNAUTHORIZED", "token non valido").await?;
                        continue;
                    }
                };

                if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
                    send_error(&sink, "BAD_REQUEST", "coordinate fuori range").await?;
                    continue;
                }
                if ts <= 0 {
                    send_error(&sink, "BAD_REQUEST", "timestamp non valido").await?;
                    continue;
                }

                match db::trip_open_for(trip_id, &user).await {
                    Ok(true) => {}
                    Ok(false) => {
                        send_error(
                            &sink,
                            "BAD_REQUEST",
                            "viaggio inesistente, non tuo o già chiuso",
                        )
                        .await?;
                        continue;
                    }
                    Err(e) => {
                        send_error(&sink, "BAD_REQUEST", &e.to_string()).await?;
                        continue;
                    }
                }

                // Trip aperto in DB ma assente dallo store in memoria → orfano
                // (tipicamente dopo un restart del server). Lo stato di
                // movimento è perso: lo chiudiamo subito e segnaliamo al
                // client di non riprovare.
                if !is_tracked(trip_id).await {
                    if let Err(e) = db::end_trip(trip_id, &user, ts).await {
                        tracing::warn!(user = %user, trip_id, "chiusura trip orfano fallita: {e}");
                    } else {
                        tracing::info!(user = %user, trip_id, ts, "trip orfano chiuso");
                    }
                    send_error(
                        &sink,
                        "TRIP_TERMINATED",
                        "viaggio chiuso dal server (stato perso): apri un nuovo viaggio",
                    )
                    .await?;
                    continue;
                }

                match update_trip(trip_id, &user, lat, lon, ts).await {
                    Ok(()) => {
                        tracing::info!(user = %user, trip_id, ts, lat, lon, "posizione aggiornata");
                        send(&sink, &Message::Ack).await?;
                    }
                    Err(e) => {
                        send_error(&sink, "UPDATE_TRIP_FAILED", &e).await?;
                    }
                }
            }
            Message::EndTrip { token, trip_id, ts } => {
                let user = match validate_token(&token, &user_state).await {
                    Some(u) => u,
                    None => {
                        send_error(&sink, "UNAUTHORIZED", "token non valido").await?;
                        continue;
                    }
                };

                match trips::terminate_trip(trip_id, user.clone(), ts).await {
                    Ok(()) => {
                        tracing::info!(user = %user, trip_id, ts, "viaggio terminato");
                        send(&sink, &Message::Ack).await?;
                    }
                    Err(e) => {
                        send_error(&sink, "BAD_REQUEST", &e).await?;
                    }
                }
            }
            Message::Stats {
                token,
                from_ts,
                to_ts,
            } => {
                let user = match validate_token(&token, &user_state).await {
                    Some(u) => u,
                    None => {
                        send_error(&sink, "UNAUTHORIZED", "token non valido").await?;
                        continue;
                    }
                };

                match db::movement_stats_for_user(&user, from_ts, to_ts).await {
                    Ok(stats) => {
                        tracing::info!(
                            user = %stats.username,
                            points = stats.points,
                            distance_m = stats.distance_m,
                            movement_secs = stats.movement_secs,
                            pause_secs = stats.pause_secs,
                            avg_speed_kmh = stats.avg_speed_kmh,
                            "statistiche calcolate"
                        );
                        send(
                            &sink,
                            &Message::StatsResult {
                                username: stats.username,
                                from_ts: stats.from_ts,
                                to_ts: stats.to_ts,
                                distance_m: stats.distance_m,
                                movement_secs: stats.movement_secs,
                                pause_secs: stats.pause_secs,
                                total_secs: stats.total_secs,
                                avg_speed_mps: stats.avg_speed_mps,
                                avg_speed_kmh: stats.avg_speed_kmh,
                                points: stats.points,
                            },
                        )
                        .await?;
                    }
                    Err(e) => {
                        send_error(&sink, "BAD_REQUEST", &e.to_string()).await?;
                    }
                }
            }
            Message::ChatToServer { token, text } => {
                let user = match validate_token(&token, &user_state).await {
                    Some(u) => u,
                    None => {
                        send_error(&sink, "UNAUTHORIZED", "token non valido").await?;
                        continue;
                    }
                };
                if let Err(e) = validate_chat(&text) {
                    send_error(&sink, "BAD_REQUEST", e).await?;
                    continue;
                }
                // Modello attuale: il client parla SOLO col server. Niente
                // fan-out agli altri client. Server logga su tracing + stdout
                // così l'operatore può rispondere via admin CLI.
                tracing::info!(user = %user, text = %text, "chat msg from client");
                println!("[chat] {user}: {text}");
                send(&sink, &Message::Ack).await?;
            }
            Message::Disconnect { token } => {
                let state_user = user_state.lock().await.clone();
                let db_user = db::get_user_by_token(&token).await;
                if db_user.is_some() && db_user == state_user {
                    let user = db_user.unwrap();
                    close_open_trips(&user).await;
                    status::remove(&user).await;
                    sinks_registry().lock().await.remove(&user);
                    db::invalidate_token(&token).await;
                    *user_state.lock().await = None;
                }
                send(&sink, &Message::Ack).await?;
                let mut s = sink.lock().await;
                let _ = s.send(WsMessage::Close(None)).await;
                break;
            }

            Message::TripStarted { .. }
            | Message::AuthOk { .. }
            | Message::Ack
            | Message::Error { .. }
            | Message::StatsResult { .. }
            | Message::ChatFromServer { .. } => {
                send_error(&sink, "BAD_REQUEST", "tipo messaggio non valido dal client").await?;
            }
        }
    }

    // Cleanup connessione: chiude i trip lasciati aperti dal client, segna
    // l'utente come disconnesso e invalida il token. Evita di accumulare
    // entry orfane in `last_coordinates` e trip eternamente aperti in DB
    // per client che chiudono la WS senza inviare `Disconnect`.
    let final_user = user_state.lock().await.clone();
    if let Some(user) = final_user.as_deref() {
        close_open_trips(user).await;
        status::remove(user).await;
        sinks_registry().lock().await.remove(user);
        db::invalidate_user_sessions(user).await;
    }
    chat_task.abort();
    Ok(())
}

/// Chiude tutti i trip ancora aperti per `user` in DB e libera le entry
/// corrispondenti dallo store in memoria. Usata sia su `Disconnect`
/// esplicito sia sul cleanup della WebSocket. Eventuali errori del DB
/// vengono solo loggati: il cleanup non deve mai propagare panic.
async fn close_open_trips(user: &str) { 
    let ts = now_secs();
    let ids = match db::open_trip_ids_for(user).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(user, "lookup trip aperti fallito: {e}");
            return;
        }
    };
    for id in ids {
        if let Err(e) = db::end_trip(id, user, ts).await {
            tracing::warn!(user, trip_id = id, "chiusura trip in cleanup fallita: {e}");
        }
        drop_trip(id).await;
    }
}

/// Verifica che il token sia valido e coerente con la connessione.
/// Cosa controlla, in ordine:
/// 1. il token esiste nello store delle sessioni e mappa a un username;
/// 2. lo username dello store coincide con quello memorizzato nello stato
///    della connessione → difesa in profondità contro stati incoerenti.
/// Ritorna `Some(username)` solo se tutti i check passano.
async fn validate_token(
    token: &str,
    user_state: &SharedUser,
) -> Option<String> {
    let user = db::get_user_by_token(token).await?;
    let state_user = user_state.lock().await.clone();
    if state_user.as_deref() != Some(user.as_str()) {
        return None;
    }
    Some(user)
}

