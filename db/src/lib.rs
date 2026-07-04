use anyhow::{anyhow, Context, Result};
use bcrypt::{hash, verify};
use common::Message;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous,
};
use sqlx::{Row, SqlitePool};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

const BCRYPT_COST: u32 = 10;

// ---------------------------------------------------------------------------
// Pool SQLite globale.
//
// A cosa serve il pool:
// - Aprire una connessione SQLite costa (open del file, setup PRAGMA): il pool
//   apre N connessioni all'avvio e le riusa, evitando il costo per ogni query.
// - Limita la concorrenza (qui max 8 connessioni, vedi `init()`): impedisce di
//   saturare il DB e mitiga i lock su file tipici di SQLite.
// - Permette query async parallele: ogni task prende una connessione libera
//   dal pool e la rilascia al termine.
//
// Perché statico (`OnceLock`):
// - Singola istanza per processo, condivisa da tutti i moduli senza dover
//   passare `&SqlitePool` come parametro lungo tutto lo stack di chiamate.
// - `OnceLock` garantisce init lazy e thread-safe: il pool viene settato una
//   sola volta dentro `init()` e poi è in sola lettura.
// ---------------------------------------------------------------------------
static POOL: OnceLock<SqlitePool> = OnceLock::new();

// Restituisce l'URL del database: usa la variabile d'ambiente `DATABASE_URL`
// se presente, altrimenti ricade su un file SQLite locale nella working dir.
fn database_url() -> String {
    std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite://./georuggine.db".to_string())
}

// Accesso globale al pool già inizializzato. Panica se `init()` non è ancora
// stato chiamato: il pool deve esistere prima di qualsiasi query.
pub fn pool() -> &'static SqlitePool {
    POOL.get()
        .expect("DB non inizializzato: chiama db::init() prima")
}

/// Apre il pool, abilita foreign_keys e applica le migrations.
pub async fn init() -> Result<()> {
    let url = database_url();
    // WAL + synchronous=Normal: scritture concorrenti non bloccano i lettori e
    // si paga solo una fsync per commit invece che due. Trade-off accettabile:
    // in caso di crash del SO al massimo si perdono le ultime transazioni non
    // ancora checkpointate, ma niente corruzione.
    // `:memory:` non supporta WAL, quindi resto sul default in quel caso.
    let mut opts = SqliteConnectOptions::from_str(&url)
        .context("DATABASE_URL non valido")?
        .create_if_missing(true)
        .foreign_keys(true);
    if url != "sqlite::memory:" {
        opts = opts
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal);
    }

    let max_connections = if url == "sqlite::memory:" { 1 } else { 8 };
    let pool = SqlitePoolOptions::new()
        .max_connections(max_connections)
        .connect_with(opts)
        .await
        .context("apertura pool sqlite")?;

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .context("migrations")?;

    POOL.set(pool)
        .map_err(|_| anyhow!("pool gia' inizializzato"))?;
    Ok(())
}

/// Compat: il server chiama `ensure_file_exists` al boot.
pub async fn ensure_file_exists() -> Result<()> {
    if POOL.get().is_none() {
        init().await?;
    }
    Ok(())
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Sessioni in memoria (token <-> username).
// ---------------------------------------------------------------------------

pub struct AuthStore {
    sessions: HashMap<String, String>,
}

impl AuthStore {
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
        }
    }
}

impl Default for AuthStore {
    fn default() -> Self {
        Self::new()
    }
}

static AUTH_STORE: OnceLock<Mutex<AuthStore>> = OnceLock::new();

pub fn get_auth_store() -> &'static Mutex<AuthStore> {
    AUTH_STORE.get_or_init(|| Mutex::new(AuthStore::new()))
}

pub async fn insert_token(username: &str, token: &str) {
    let mut store = get_auth_store().lock().await;
    let old: Vec<String> = store
        .sessions
        .iter()
        .filter_map(|(t, u)| (u == username).then(|| t.clone()))
        .collect();
    for t in old {
        store.sessions.remove(&t);
    }
    store
        .sessions
        .insert(token.to_string(), username.to_string());
}

/// Inserisce token solo se `username` non ha già una sessione attiva.
/// Check + insert atomici sotto lo stesso lock: due login concorrenti
/// per lo stesso utente non possono entrambi superare la verifica.
pub async fn try_insert_token(username: &str, token: &str) -> Result<()> {
    let mut store = get_auth_store().lock().await;
    if store.sessions.values().any(|u| u == username) {
        return Err(anyhow!("utente già loggato da altra sessione"));
    }
    store
        .sessions
        .insert(token.to_string(), username.to_string());
    Ok(())
}

pub async fn get_user_by_token(token: &str) -> Option<String> {
    let store = get_auth_store().lock().await;
    store.sessions.get(token).cloned()
}

pub async fn invalidate_token(token: &str) {
    let mut store = get_auth_store().lock().await;
    store.sessions.remove(token);
}

/// Invalida tutte le sessioni attive per `username`. Usata quando una
/// nuova LOGIN sovrascrive una sessione precedente (single-session policy):
/// elimina il token in modo che, se il vecchio client tornasse a parlare,
/// qualunque richiesta autenticata venga rifiutata con `UNAUTHORIZED`.
pub async fn invalidate_user_sessions(username: &str) -> usize {
    let mut store = get_auth_store().lock().await;
    let tokens: Vec<String> = store
        .sessions
        .iter()
        .filter_map(|(t, u)| (u == username).then(|| t.clone()))
        .collect();
    let n = tokens.len();
    for t in tokens {
        store.sessions.remove(&t);
    }
    n
}

/// Vero se l'utente compare in una sessione attiva (qualunque conn).
pub async fn is_logged_in(username: &str) -> bool {
    let store = get_auth_store().lock().await;
    store.sessions.values().any(|u| u == username)
}

pub async fn clear_sessions() {
    let mut store = get_auth_store().lock().await;
    store.sessions.clear();
}

/// Wipe completo: utile nei test per partire da uno stato pulito.
pub async fn reset_all_for_tests() -> Result<()> {
    sqlx::query("DELETE FROM positions").execute(pool()).await?;
    sqlx::query("DELETE FROM trips").execute(pool()).await?;
    sqlx::query("DELETE FROM users").execute(pool()).await?;
    clear_sessions().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Utenti.
// ---------------------------------------------------------------------------

pub async fn key_exists(username: &str) -> Result<bool> {
    let row = sqlx::query("SELECT 1 FROM users WHERE username = ?")
        .bind(username)
        .fetch_optional(pool())
        .await?;
    Ok(row.is_some())
}

pub async fn check_credentials(msg: &Message) -> Result<()> {
    let (username, password) = match msg {
        Message::Login { username, password } => (username.clone(), password.clone()),
        Message::Register { username, password } => (username.clone(), password.clone()),
        _ => return Err(anyhow!("messaggio non valido")),
    };

    let row = sqlx::query("SELECT password_hash FROM users WHERE username = ?")
        .bind(&username)
        .fetch_optional(pool())
        .await?;
    let Some(row) = row else {
        return Err(anyhow!("utente non trovato"));
    };
    let stored: String = row.try_get("password_hash")?;

    let ok = tokio::task::spawn_blocking(move || verify(&password, &stored).unwrap_or(false))
        .await
        .context("join verify")?;
    if ok {
        Ok(())
    } else {
        Err(anyhow!("credenziali non valide"))
    }
}

pub async fn save_register(msg: &Message) -> Result<()> {
    let (username, password) = match msg {
        Message::Register { username, password } => (username.clone(), password.clone()),
        _ => return Err(anyhow!("save_register: non è una variante Register")),
    };

    let password_hash = tokio::task::spawn_blocking(move || hash(password, BCRYPT_COST))
        .await
        .context("join hash")??;

    let res =
        sqlx::query("INSERT INTO users (username, password_hash, created_at) VALUES (?, ?, ?)")
            .bind(&username)
            .bind(&password_hash)
            .bind(now_secs())
            .execute(pool())
            .await;

    match res {
        Ok(_) => {
            tracing::info!("utente '{username}' registrato");
            Ok(())
        }
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
            Err(anyhow!("utente esiste già"))
        }
        Err(e) => Err(e.into()),
    }
}

// ---------------------------------------------------------------------------
// Trip.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TripRecord {
    pub id: i64,
    pub username: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
}

/// Apre un nuovo viaggio per l'utente. Ritorna `trip_id`.
pub async fn start_trip(username: &str, lat: f64, lon: f64, ts: i64) -> Result<i64> {
    let row = sqlx::query(
        "INSERT INTO trips (username, started_at, ended_at) VALUES (?, ?, NULL) RETURNING id",
    )
    .bind(username)
    .bind(ts)
    .fetch_one(pool())
    .await?;
    let id: i64 = row.try_get("id")?;
    insert_position(id, lat, lon, ts, false).await?;
    Ok(id)
}

pub async fn insert_position(
    trip_id: i64,
    lat: f64,
    lon: f64,
    ts: i64,
    stopped: bool,
) -> Result<()> {
    sqlx::query("INSERT INTO positions (trip_id, ts, lat, lon, stopped) VALUES (?, ?, ?, ?, ?)")
        .bind(trip_id)
        .bind(ts)
        .bind(lat)
        .bind(lon)
        .bind(stopped)
        .execute(pool())
        .await?;
    Ok(())
}

/// Chiude un viaggio: valida ownership + che non sia gia' chiuso.
pub async fn end_trip(trip_id: i64, username: &str, ts: i64) -> Result<()> {
    let res = sqlx::query(
        "UPDATE trips SET ended_at = ? \
         WHERE id = ? AND username = ? AND ended_at IS NULL",
    )
    .bind(ts)
    .bind(trip_id)
    .bind(username)
    .execute(pool())
    .await?;
    if res.rows_affected() == 0 {
        return Err(anyhow!("viaggio inesistente, non tuo o già chiuso"));
    }
    Ok(())
}

/// Restituisce username del trip se aperto e di proprieta' dell'utente.
pub async fn trip_open_for(trip_id: i64, username: &str) -> Result<bool> {
    let row = sqlx::query("SELECT 1 FROM trips WHERE id = ? AND username = ? AND ended_at IS NULL")
        .bind(trip_id)
        .bind(username)
        .fetch_optional(pool())
        .await?;
    Ok(row.is_some())
}

/// ID di tutti i trip ancora aperti per un utente. Usato in fase di cleanup
/// della connessione WebSocket per chiudere viaggi rimasti pendenti quando
/// il client si disconnette senza inviare `EndTrip`.
pub async fn open_trip_ids_for(username: &str) -> Result<Vec<i64>> {
    let rows = sqlx::query("SELECT id FROM trips WHERE username = ? AND ended_at IS NULL")
        .bind(username)
        .fetch_all(pool())
        .await?;
    rows.into_iter()
        .map(|row| row.try_get::<i64, _>("id").map_err(Into::into))
        .collect()
}

// ---------------------------------------------------------------------------
// Posizioni e statistiche.
// ---------------------------------------------------------------------------

// Tolleranza in metri per evitare che oscillazioni minime del GPS vengano
// considerate movimento reale. Esposta pubblicamente perché anche il modulo
// `trips` lato server la usa per decidere se due punti sono "uguali",
// così la stessa soglia governa sia il flag `stopped` che il calcolo
// statistico delle pause.
pub const MOVEMENT_EPS_METERS: f64 = 5.0;

/// Massimo gap accettato tra due campioni consecutivi nello stesso trip
/// quando si calcolano le statistiche. Spec: cadenza nominale 30 s, quindi
/// 90 s = 3× tolleranza copre piccoli ritardi/jitter. Oltre questa soglia
/// il segmento non è affidabile (es. client offline, app crashata, server
/// riavviato) e va escluso dai conteggi: altrimenti un buco di ore
/// finirebbe contato come "pausa" o "movimento" gonfiando le statistiche.
const MAX_SAMPLE_GAP_SECS: i64 = 90;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PositionRecord {
    pub trip_id: i64,
    pub ts: i64,
    pub lat: f64,
    pub lon: f64,
    pub stopped: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MovementStats {
    pub username: String,
    pub from_ts: i64,
    pub to_ts: i64,
    pub distance_m: f64,
    pub movement_secs: i64,
    pub pause_secs: i64,
    pub total_secs: i64,
    pub avg_speed_mps: f64,
    pub avg_speed_kmh: f64,
    pub points: i64,
}

pub async fn trajectory_for_user(
    username: &str,
    from_ts: i64,
    to_ts: i64,
) -> Result<Vec<PositionRecord>> {
    if from_ts > to_ts {
        return Err(anyhow!("intervallo temporale non valido"));
    }

    let rows = sqlx::query(
        "SELECT p.trip_id, p.ts, p.lat, p.lon, p.stopped
         FROM positions p
         JOIN trips t ON t.id = p.trip_id
         WHERE t.username = ?
           AND p.ts >= ?
           AND p.ts <= ?
         ORDER BY p.trip_id, p.ts",
    )
    .bind(username)
    .bind(from_ts)
    .bind(to_ts)
    .fetch_all(pool())
    .await?;

    rows.into_iter()
        .map(|row| {
            Ok(PositionRecord {
                trip_id: row.try_get("trip_id")?,
                ts: row.try_get("ts")?,
                lat: row.try_get("lat")?,
                lon: row.try_get("lon")?,
                stopped: row.try_get("stopped")?,
            })
        })
        .collect()
}

pub async fn movement_stats_for_user(
    username: &str,
    from_ts: i64,
    to_ts: i64,
) -> Result<MovementStats> {
    let points = trajectory_for_user(username, from_ts, to_ts).await?;
    Ok(compute_movement_stats(username, from_ts, to_ts, &points))
}

pub fn compute_movement_stats(
    username: &str,
    from_ts: i64,
    to_ts: i64,
    points: &[PositionRecord],
) -> MovementStats {
    let mut distance_m = 0.0;
    let mut movement_secs = 0_i64;
    let mut pause_secs = 0_i64;
    let mut total_secs = 0_i64;

    for window in points.windows(2) {
        let a = &window[0];
        let b = &window[1];

        // Non collegare artificialmente due viaggi diversi.
        if a.trip_id != b.trip_id {
            continue;
        }

        let dt = b.ts - a.ts;
        if dt <= 0 || dt > MAX_SAMPLE_GAP_SECS {
            continue;
        }

        total_secs += dt;
        let d = haversine_m(a.lat, a.lon, b.lat, b.lon);

        // Il modello aggiornato ha già il booleano `stopped`: lo usiamo come
        // segnale principale. La distanza sotto soglia resta una protezione
        // utile contro rumore GPS o dati vecchi senza flag coerente.
        let is_pause = b.stopped || d <= MOVEMENT_EPS_METERS;

        if is_pause {
            pause_secs += dt;
        } else {
            distance_m += d;
            movement_secs += dt;
        }
    }

    let avg_speed_mps = if movement_secs > 0 {
        distance_m / movement_secs as f64
    } else {
        0.0
    };

    MovementStats {
        username: username.to_string(),
        from_ts,
        to_ts,
        distance_m,
        movement_secs,
        pause_secs,
        total_secs,
        avg_speed_mps,
        avg_speed_kmh: avg_speed_mps * 3.6,
        points: points.len() as i64,
    }
}

pub fn haversine_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let r = 6_371_000.0_f64;

    let phi1 = lat1.to_radians();
    let phi2 = lat2.to_radians();
    let d_phi = (lat2 - lat1).to_radians();
    let d_lambda = (lon2 - lon1).to_radians();

    let a = (d_phi / 2.0).sin().powi(2) + phi1.cos() * phi2.cos() * (d_lambda / 2.0).sin().powi(2);

    let c = 2.0 * a.sqrt().atan2((1.0 - a).sqrt());

    r * c
}

// ---------------------------------------------------------------------------
// Test.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    static TEST_GUARD: StdMutex<()> = StdMutex::new(());

    async fn setup() {
        if POOL.get().is_none() {
            std::fs::create_dir_all("target").ok();
            std::fs::remove_file("target/db_tests.sqlite").ok();
            std::fs::remove_file("target/db_tests.sqlite-shm").ok();
            std::fs::remove_file("target/db_tests.sqlite-wal").ok();

            std::env::set_var(
                "DATABASE_URL",
                "sqlite://./target/db_tests.sqlite",
            );

            init().await.unwrap();
        }

        reset_all_for_tests().await.unwrap();
    }

    #[tokio::test]
    async fn register_e_check_credentials_ok() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|p| p.into_inner());
        setup().await;
        let reg = Message::Register {
            username: "mario".into(),
            password: "secret".into(),
        };
        save_register(&reg).await.unwrap();
        assert!(key_exists("mario").await.unwrap());
        let login = Message::Login {
            username: "mario".into(),
            password: "secret".into(),
        };
        assert!(check_credentials(&login).await.is_ok());
        let bad = Message::Login {
            username: "mario".into(),
            password: "wrong".into(),
        };
        assert!(check_credentials(&bad).await.is_err());
    }

    #[tokio::test]
    async fn save_register_duplicato_fallisce() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|p| p.into_inner());
        setup().await;
        let reg = Message::Register {
            username: "luigi".into(),
            password: "secret".into(),
        };
        save_register(&reg).await.unwrap();
        assert!(save_register(&reg).await.is_err());
    }

    #[tokio::test]
    async fn trip_lifecycle() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|p| p.into_inner());
        setup().await;
        save_register(&Message::Register {
            username: "mario".into(),
            password: "secret".into(),
        })
        .await
        .unwrap();

        let trip_id = start_trip("mario", 45.07, 12.29, 0).await.unwrap();
        assert!(trip_open_for(trip_id, "mario").await.unwrap());
        assert!(!trip_open_for(trip_id, "altro").await.unwrap());

        end_trip(trip_id, "mario", 1).await.unwrap();
        assert!(!trip_open_for(trip_id, "mario").await.unwrap());
        assert!(end_trip(trip_id, "mario", 1).await.is_err());
    }

    #[tokio::test]
    async fn end_trip_di_altro_utente_fallisce() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|p| p.into_inner());
        setup().await;
        for u in ["mario", "luigi"] {
            save_register(&Message::Register {
                username: u.into(),
                password: "secret".into(),
            })
            .await
            .unwrap();
        }
        let trip_id = start_trip("mario", 45.07, 12.29, 0).await.unwrap();
        assert!(end_trip(trip_id, "luigi", 1).await.is_err());
    }

    #[test]
    fn statistiche_usano_stopped_e_distanza() {
        let points = vec![
            PositionRecord {
                trip_id: 1,
                ts: 0,
                lat: 45.000,
                lon: 9.000,
                stopped: false,
            },
            PositionRecord {
                trip_id: 1,
                ts: 60,
                lat: 45.000,
                lon: 9.000,
                stopped: true,
            },
            PositionRecord {
                trip_id: 1,
                ts: 120,
                lat: 45.001,
                lon: 9.000,
                stopped: false,
            },
            PositionRecord {
                trip_id: 1,
                ts: 180,
                lat: 45.002,
                lon: 9.000,
                stopped: false,
            },
        ];

        let stats = compute_movement_stats("mario", 0, 180, &points);
        assert_eq!(stats.points, 4);
        assert_eq!(stats.pause_secs, 60);
        assert_eq!(stats.movement_secs, 120);
        assert_eq!(stats.total_secs, 180);
        assert!(stats.distance_m > 200.0 && stats.distance_m < 230.0);
        assert!(stats.avg_speed_kmh > 6.0 && stats.avg_speed_kmh < 7.0);
    }
}
