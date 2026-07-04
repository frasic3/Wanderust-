use std::{collections::HashMap, sync::OnceLock};

use tokio::sync::Mutex;

use crate::status::{self, UserStatus};

#[derive(Debug, Clone, Default)]
struct CoordState {
    lat: f64,
    lon: f64,
    n: u32,
    inserted: bool,
}

pub struct TripsStore {
    // Associa trip_id all'ultima coordinata nota
    last_coordinates: HashMap<i64, CoordState>,
}

impl TripsStore {
    pub fn new() -> Self {
        Self {
            last_coordinates: HashMap::new(),
        }
    }
}

impl Default for TripsStore {
    fn default() -> Self {
        Self::new()
    }
}

static TRIPS_STORE: OnceLock<Mutex<TripsStore>> = OnceLock::new();

pub fn get_trips_store() -> &'static Mutex<TripsStore> {
    TRIPS_STORE.get_or_init(|| Mutex::new(TripsStore::new()))
}

pub async fn initialize_trips(user: String, lat: f64, lon: f64, ts: i64) -> Result<i64, String> {
    let trip_id = db::start_trip(&user, lat, lon, ts)
        .await
        .map_err(|e| format!("Failed to start trip: {}", e))?;

    {
        let mut store = get_trips_store().lock().await;
        store.last_coordinates.insert(
            trip_id,
            CoordState {
                lat,
                lon,
                n: 1,
                inserted: false,
            },
        );
    }

    // Spec: appena loggato/avviato l'utente è "fermo"; passa a "in movimento"
    // solo al primo cambio di coordinata.
    status::set(&user, UserStatus::Stationary).await;

    Ok(trip_id)
}

pub async fn terminate_trip(trip_id: i64, user: String, ts: i64) -> Result<(), String> {
    db::end_trip(trip_id, &user, ts)
        .await
        .map_err(|e| format!("Failed to end trip: {}", e))?;

    let mut store = get_trips_store().lock().await;
    store.last_coordinates.remove(&trip_id);

    Ok(())
}

/// Rimuove un trip dallo store in memoria senza toccare il DB. Usato dalla
/// procedura di cleanup della connessione quando si chiude un trip orfano
/// (la riga DB è già stata aggiornata dal chiamante via `db::end_trip`).
pub async fn drop_trip(trip_id: i64) {
    let mut store = get_trips_store().lock().await;
    store.last_coordinates.remove(&trip_id);
}

// Numero di campioni `POSITION` consecutivi con coordinate identiche oltre
// il quale l'utente viene marcato "fermo". Con cadenza di 30 s lato client
// (spec), 6 campioni = 180 s = 3 min, come richiesto dalla specifica.
const STOP_THRESHOLD_SAMPLES: u32 = 6;

/// `true` se il trip è presente nello store in memoria.
/// Usato dal dispatcher per distinguere un trip "vivo" da un trip rimasto
/// aperto in DB dopo un crash/restart del server (in tal caso lo stato
/// di movimento è perso e non è recuperabile: meglio chiuderlo).
pub async fn is_tracked(trip_id: i64) -> bool {
    let store = get_trips_store().lock().await;
    store.last_coordinates.contains_key(&trip_id)
}

/// Decide se due coordinate possono essere considerate "uguali" ai fini
/// del rilevamento della pausa. Usa la distanza haversine sotto la stessa
/// soglia adottata da `compute_movement_stats`, così il flag `stopped`
/// scritto in DB e il calcolo delle pause restano coerenti: niente
/// confronti `==` su `f64`, che sarebbero fragili (rumore numerico,
/// arrotondamenti del client).
fn coords_equal(a_lat: f64, a_lon: f64, b_lat: f64, b_lon: f64) -> bool {
    db::haversine_m(a_lat, a_lon, b_lat, b_lon) <= db::MOVEMENT_EPS_METERS
}

pub async fn update_trip(
    trip_id: i64,
    user: &str,
    lat: f64,
    lon: f64,
    ts: i64,
) -> Result<(), String> {
    enum InsertAction {
        Insert { stopped: bool },
        Skip,
    }

    enum StatusTransition {
        ToMoving,
        ToStationary,
        Unchanged,
    }

    let (action, transition) = {
        let mut store = get_trips_store().lock().await;
        match store.last_coordinates.get_mut(&trip_id) {
            Some(coord) => {
                if coords_equal(coord.lat, coord.lon, lat, lon) {
                    if coord.n < STOP_THRESHOLD_SAMPLES {
                        coord.n += 1;
                        (
                            InsertAction::Insert { stopped: false },
                            StatusTransition::Unchanged,
                        )
                    } else if !coord.inserted {
                        coord.inserted = true;
                        (
                            InsertAction::Insert { stopped: true },
                            StatusTransition::ToStationary,
                        )
                    } else {
                        (InsertAction::Skip, StatusTransition::Unchanged)
                    }
                } else {
                    *coord = CoordState {
                        lat,
                        lon,
                        n: 1,
                        inserted: false,
                    };
                    (
                        InsertAction::Insert { stopped: false },
                        StatusTransition::ToMoving,
                    )
                }
            }
            None => return Err(format!("Trip ID {} not found in store", trip_id)),
        }
    };

    match action {
        InsertAction::Insert { stopped } => db::insert_position(trip_id, lat, lon, ts, stopped)
            .await
            .map_err(|e| format!("Failed to insert position: {}", e))?,
        InsertAction::Skip => {}
    }

    match transition {
        StatusTransition::ToMoving => status::set(user, UserStatus::Moving).await,
        StatusTransition::ToStationary => status::set(user, UserStatus::Stationary).await,
        StatusTransition::Unchanged => {}
    }

    Ok(())
}
