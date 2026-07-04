// Test integration: lanciano il binary server con porta e DB isolati
// per essere parallel-safe. Prerequisito: `cargo build -p server`.
use common::{decode, encode, Message};
use futures_util::{SinkExt, StreamExt};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message as WsMessage;

static PORT_COUNTER: AtomicU16 = AtomicU16::new(17878);

fn next_port() -> u16 {
    PORT_COUNTER.fetch_add(1, Ordering::SeqCst)
}

fn server_binary() -> String {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let exe = if cfg!(windows) { "server.exe" } else { "server" };
    format!("{manifest}/../target/debug/{exe}")
}

fn ws_url(addr: &str) -> String {
    format!("ws://{addr}/ws")
}

struct Harness {
    server: Child,
    addr: String,
    db_path: String,
}

impl Harness {
    fn start() -> Self {
        let port = next_port();
        let addr = format!("127.0.0.1:{port}");
        let db_path = std::env::temp_dir()
            .join(format!("rust_proj_{port}.sqlite"))
            .to_string_lossy()
            .to_string();
        let _ = std::fs::remove_file(&db_path);
        let database_url = format!("sqlite://{}?mode=rwc", db_path.replace('\\', "/"));

        let server = Command::new(server_binary())
            .env("SERVER_ADDR", &addr)
            .env("DATABASE_URL", &database_url)
            .spawn()
            .expect("server binary not found — run `cargo build -p server` first");

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if std::net::TcpStream::connect(&addr).is_ok() {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("server non si avvia entro 5s");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Self {
            server,
            addr,
            db_path,
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.server.kill().ok();
        self.server.wait().ok();
        let _ = std::fs::remove_file(&self.db_path);
        let _ = std::fs::remove_file(format!("{}-shm", self.db_path));
        let _ = std::fs::remove_file(format!("{}-wal", self.db_path));
    }
}

async fn invia_e_ricevi(addr: &str, msg: &Message) -> Message {
    let (mut ws, _) = tokio_tungstenite::connect_async(ws_url(addr))
        .await
        .expect("connect ws");
    let payload = encode(msg).unwrap();
    ws.send(WsMessage::Text(payload)).await.unwrap();
    loop {
        match ws.next().await.expect("stream chiuso").expect("ws error") {
            WsMessage::Text(t) => return decode(&t).unwrap(),
            _ => continue,
        }
    }
}

#[tokio::test]
async fn register_risponde_con_auth_ok() {
    let h = Harness::start();
    let risposta = invia_e_ricevi(
        &h.addr,
        &Message::Register {
            username: "mario".into(),
            password: "secret".into(),
        },
    )
    .await;
    match risposta {
        Message::AuthOk { token } => assert!(!token.is_empty()),
        altro => panic!("risposta inattesa: {altro:?}"),
    }
}

#[tokio::test]
async fn login_dopo_register_risponde_con_auth_ok() {
    let h = Harness::start();
    invia_e_ricevi(
        &h.addr,
        &Message::Register {
            username: "luigi".into(),
            password: "pass123".into(),
        },
    )
    .await;
    let risposta = invia_e_ricevi(
        &h.addr,
        &Message::Login {
            username: "luigi".into(),
            password: "pass123".into(),
        },
    )
    .await;
    match risposta {
        Message::AuthOk { token } => assert!(!token.is_empty()),
        altro => panic!("risposta inattesa: {altro:?}"),
    }
}

#[tokio::test]
async fn login_password_sbagliata_risponde_con_error() {
    let h = Harness::start();
    invia_e_ricevi(
        &h.addr,
        &Message::Register {
            username: "peach".into(),
            password: "corretta".into(),
        },
    )
    .await;
    let risposta = invia_e_ricevi(
        &h.addr,
        &Message::Login {
            username: "peach".into(),
            password: "sbagliata".into(),
        },
    )
    .await;
    assert!(matches!(risposta, Message::Error { .. }));
}

#[tokio::test]
async fn login_utente_inesistente_risponde_con_error() {
    let h = Harness::start();
    let risposta = invia_e_ricevi(
        &h.addr,
        &Message::Login {
            username: "fantasma".into(),
            password: "qualsiasi".into(),
        },
    )
    .await;
    assert!(matches!(risposta, Message::Error { .. }));
}

#[tokio::test]
async fn register_duplicato_risponde_con_error() {
    let h = Harness::start();
    let msg = Message::Register {
        username: "bowser".into(),
        password: "secret".into(),
    };
    invia_e_ricevi(&h.addr, &msg).await;
    let risposta = invia_e_ricevi(&h.addr, &msg).await;
    assert!(matches!(risposta, Message::Error { .. }));
}

#[tokio::test]
async fn start_e_end_trip_lifecycle() {
    let h = Harness::start();
    let (mut ws, _) = tokio_tungstenite::connect_async(ws_url(&h.addr))
        .await
        .unwrap();

    ws.send(WsMessage::Text(
        encode(&Message::Register {
            username: "yoshi".into(),
            password: "secret".into(),
        })
        .unwrap(),
    ))
    .await
    .unwrap();
    let token = loop {
        match ws.next().await.unwrap().unwrap() {
            WsMessage::Text(t) => match decode(&t).unwrap() {
                Message::AuthOk { token } => break token,
                other => panic!("atteso AuthOk, ricevuto {other:?}"),
            },
            _ => continue,
        }
    };

    ws.send(WsMessage::Text(
        encode(&Message::StartTrip {
            token: token.clone(),
            lat: 45.07,
            lon: 7.69,
            ts: 1_700_000_000,
        })
        .unwrap(),
    ))
    .await
    .unwrap();
    let trip_id = loop {
        match ws.next().await.unwrap().unwrap() {
            WsMessage::Text(t) => match decode(&t).unwrap() {
                Message::TripStarted { trip_id, .. } => break trip_id,
                other => panic!("atteso TripStarted, ricevuto {other:?}"),
            },
            _ => continue,
        }
    };
    assert!(trip_id > 0);

    ws.send(WsMessage::Text(
        encode(&Message::EndTrip { token, trip_id, ts: 0 }).unwrap(),
    ))
    .await
    .unwrap();
    loop {
        match ws.next().await.unwrap().unwrap() {
            WsMessage::Text(t) => {
                assert!(matches!(decode(&t).unwrap(), Message::Ack));
                break;
            }
            _ => continue,
        }
    }
}

#[tokio::test]
async fn start_trip_richiede_token_valido() {
    let h = Harness::start();
    let risposta = invia_e_ricevi(
        &h.addr,
        &Message::StartTrip {
            token: "fake".into(),
            lat: 0.0,
            lon: 0.0,
            ts: 0,
        },
    )
    .await;
    match risposta {
        Message::Error { code, .. } => assert_eq!(code, "UNAUTHORIZED"),
        altro => panic!("risposta inattesa: {altro:?}"),
    }
}
