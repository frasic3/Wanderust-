//! Simulatore di movimento veicolo su grafo stradale, compilato a WebAssembly.
//!
//! Gira nel browser (zero carico sul server: il server riceve solo i
//! `POSITION` ogni 30 s). Il veicolo cammina sugli archi di un grafo stradale
//! di Torino (esportato da OpenStreetMap, vedi `tools/export_turin_graph.py`):
//! segue la forma reale delle strade, sceglie una direzione agli incroci, va a
//! velocità urbane (30–70 km/h) e fa pause occasionali (semafori/sosta).
//!
//! `advance(dt_secs)` fa progredire la simulazione di `dt_secs` *logici*. Il
//! chiamante lo invoca spesso con dt piccolo (es. 0.1 s) per un movimento
//! fluido, e campiona/invia la posizione al server ogni 30 s, come da spec.
//! Disaccoppiare animazione (fluida) e invio (ogni 30 s) evita i "salti".

use serde::Deserialize;
use wasm_bindgen::prelude::*;

// --- Costanti fisiche -------------------------------------------------------

/// Range di velocità urbana, km/h.
const SPEED_MIN_KMH: f64 = 30.0;
const SPEED_MAX_KMH: f64 = 70.0;

/// Tempo di guida (secondi logici) tra una pausa e l'altra.
const DRIVE_MIN_SECS: f64 = 90.0;
const DRIVE_MAX_SECS: f64 = 600.0;

/// Durata di una pausa (secondi logici). Alcune superano i 180 s ⇒ il server
/// le riconosce come stato "fermo"; le più brevi (semafori) restano "in
/// movimento". Coerente con la spec.
const PAUSE_MIN_SECS: f64 = 10.0;
const PAUSE_MAX_SECS: f64 = 240.0;

/// Tetto di iterazioni per `advance`: difesa contro cicli di archi a
/// lunghezza ~0 nel grafo (coordinate duplicate).
const MAX_STEPS: u32 = 10_000;

/// Raggio terrestre medio in metri (per haversine).
const EARTH_R_M: f64 = 6_371_000.0;

// --- Grafo ------------------------------------------------------------------

#[derive(Deserialize)]
struct Graph {
    /// `nodes[i] = [lat, lon]`.
    nodes: Vec<[f64; 2]>,
    /// `adj[i]` = indici dei nodi adiacenti a `i` (grafo non orientato).
    adj: Vec<Vec<u32>>,
}

impl Graph {
    fn coord(&self, i: usize) -> (f64, f64) {
        let n = self.nodes[i];
        (n[0], n[1])
    }

    fn seg_len_m(&self, a: usize, b: usize) -> f64 {
        let (alat, alon) = self.coord(a);
        let (blat, blon) = self.coord(b);
        haversine_m(alat, alon, blat, blon)
    }

    /// Un nodo è un incrocio se non ha esattamente 2 vicini: i nodi di grado 2
    /// sono semplici punti di forma lungo una strada (nessuna vera scelta).
    fn is_intersection(&self, i: usize) -> bool {
        self.adj[i].len() != 2
    }
}

/// Distanza in metri tra due coordinate (formula dell'emisenoverso).
fn haversine_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dphi = (lat2 - lat1).to_radians();
    let dlmb = (lon2 - lon1).to_radians();
    let a = (dphi / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dlmb / 2.0).sin().powi(2);
    2.0 * EARTH_R_M * a.sqrt().asin()
}

// --- PRNG -------------------------------------------------------------------

/// xorshift64*: PRNG minimale e deterministico dato il seme. Niente dipendenze
/// esterne (evita `getrandom` e la sua configurazione per il target wasm).
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15 | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// f64 uniforme in [0, 1).
    fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.unit()
    }

    /// Indice uniforme in [0, n).
    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}

// --- Simulatore -------------------------------------------------------------

#[wasm_bindgen]
pub struct Simulator {
    graph: Graph,
    rng: Rng,
    /// Nodo da cui si proviene nel segmento corrente.
    from: usize,
    /// Nodo verso cui si sta andando.
    to: usize,
    /// Distanza già percorsa lungo il segmento `from → to`, in metri.
    offset_m: f64,
    /// Velocità corrente in m/s (ricampionata agli incroci).
    speed_mps: f64,
    /// Secondi di guida rimanenti prima della prossima pausa.
    drive_secs_left: f64,
    /// Secondi di pausa rimanenti; > 0 ⇒ fermo (coordinata invariata).
    pause_secs_left: f64,
    /// Velocità mostrata in UI (0 durante le pause).
    last_speed_kmh: f64,
    lat: f64,
    lon: f64,
}

#[wasm_bindgen]
impl Simulator {
    /// Crea il simulatore dal JSON del grafo (`{nodes, adj}`) e da un seme
    /// (es. `Date.now()` lato JS). Posizione iniziale: nodo casuale con almeno
    /// un vicino. Ritorna errore se il JSON è invalido o il grafo è vuoto.
    #[wasm_bindgen(constructor)]
    pub fn new(graph_json: &str, seed: f64) -> Result<Simulator, JsValue> {
        // `JsValue` non è utilizzabile fuori da wasm (panica): tieni la logica
        // in `build` con errore `String`, testabile nativamente, e qui mappa
        // solo l'errore al tipo JS.
        Self::build(graph_json, seed).map_err(|e| JsValue::from_str(&e))
    }

    fn build(graph_json: &str, seed: f64) -> Result<Simulator, String> {
        let graph: Graph = serde_json::from_str(graph_json)
            .map_err(|e| format!("grafo JSON non valido: {e}"))?;
        if graph.nodes.is_empty() {
            return Err("grafo vuoto".to_string());
        }

        let mut rng = Rng::new(seed as u64);

        // Cerca un nodo di partenza con almeno un arco uscente.
        let n = graph.nodes.len();
        let mut from = rng.below(n);
        for _ in 0..n {
            if !graph.adj[from].is_empty() {
                break;
            }
            from = (from + 1) % n;
        }
        if graph.adj[from].is_empty() {
            return Err("grafo senza archi".to_string());
        }

        let to = graph.adj[from][rng.below(graph.adj[from].len())] as usize;
        let (lat, lon) = graph.coord(from);
        let speed_mps = rng.range(SPEED_MIN_KMH, SPEED_MAX_KMH) / 3.6;
        let drive_secs_left = rng.range(DRIVE_MIN_SECS, DRIVE_MAX_SECS);

        Ok(Simulator {
            graph,
            rng,
            from,
            to,
            offset_m: 0.0,
            speed_mps,
            drive_secs_left,
            pause_secs_left: 0.0,
            last_speed_kmh: 0.0,
            lat,
            lon,
        })
    }

    fn resample_speed(&mut self) {
        self.speed_mps = self.rng.range(SPEED_MIN_KMH, SPEED_MAX_KMH) / 3.6;
    }

    /// Sceglie il prossimo nodo da `node`, evitando di tornare su `avoid`
    /// (il nodo da cui si è appena arrivati) se esistono alternative.
    /// Su un vicolo cieco torna indietro (U-turn).
    fn pick_next(&mut self, node: usize, avoid: usize) -> usize {
        let neigh = &self.graph.adj[node];
        let alt = neigh.iter().filter(|&&x| x as usize != avoid).count();
        if alt == 0 {
            return avoid; // vicolo cieco
        }
        let mut k = self.rng.below(alt);
        for &x in neigh {
            if x as usize == avoid {
                continue;
            }
            if k == 0 {
                return x as usize;
            }
            k -= 1;
        }
        avoid // irraggiungibile
    }

    /// Fa progredire la simulazione di `dt_secs` secondi logici. Chiamato spesso
    /// con dt piccolo (animazione fluida); la posizione va campionata/inviata
    /// dal chiamante ogni 30 s.
    pub fn advance(&mut self, dt_secs: f64) {
        if dt_secs <= 0.0 {
            return;
        }

        // In pausa: fermo, coordinata invariata.
        if self.pause_secs_left > 0.0 {
            self.pause_secs_left -= dt_secs;
            if self.pause_secs_left > 0.0 {
                self.last_speed_kmh = 0.0;
                return;
            }
            // Pausa finita: riparti con una nuova velocità.
            self.pause_secs_left = 0.0;
            self.resample_speed();
        }

        // Scadenza del tempo di guida → inizia una pausa nel punto corrente
        // (anche a metà strada: sosta nel traffico).
        self.drive_secs_left -= dt_secs;
        if self.drive_secs_left <= 0.0 {
            self.pause_secs_left = self.rng.range(PAUSE_MIN_SECS, PAUSE_MAX_SECS);
            self.drive_secs_left = self.rng.range(DRIVE_MIN_SECS, DRIVE_MAX_SECS);
            self.last_speed_kmh = 0.0;
            return;
        }

        self.last_speed_kmh = self.speed_mps * 3.6;
        let mut budget_m = self.speed_mps * dt_secs;

        let mut steps = 0;
        loop {
            steps += 1;
            if steps > MAX_STEPS {
                break;
            }

            let seg = self.graph.seg_len_m(self.from, self.to);
            let remain = seg - self.offset_m;

            if budget_m < remain {
                self.offset_m += budget_m;
                break;
            }

            // Raggiunto il nodo `to`: prosegui su un nuovo arco.
            budget_m -= remain.max(0.0);
            let arrived = self.to;
            let came_from = self.from;
            let next = self.pick_next(arrived, came_from);
            self.from = arrived;
            self.to = next;
            self.offset_m = 0.0;
            // A un vero incrocio, cambia velocità (strada nuova).
            if self.graph.is_intersection(arrived) {
                self.resample_speed();
            }
        }

        self.update_position();
    }

    /// Aggiorna `lat`/`lon` interpolando lungo il segmento `from → to`.
    fn update_position(&mut self) {
        let (flat, flon) = self.graph.coord(self.from);
        let (tlat, tlon) = self.graph.coord(self.to);
        let seg = self.graph.seg_len_m(self.from, self.to);
        let frac = if seg > 0.0 {
            (self.offset_m / seg).clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.lat = flat + (tlat - flat) * frac;
        self.lon = flon + (tlon - flon) * frac;
    }

    #[wasm_bindgen(getter)]
    pub fn lat(&self) -> f64 {
        self.lat
    }

    #[wasm_bindgen(getter)]
    pub fn lon(&self) -> f64 {
        self.lon
    }

    /// `true` se il veicolo è in movimento (non in pausa).
    #[wasm_bindgen(getter)]
    pub fn moving(&self) -> bool {
        self.pause_secs_left <= 0.0
    }

    /// Velocità corrente in km/h (0 in pausa).
    #[wasm_bindgen(getter)]
    pub fn speed_kmh(&self) -> f64 {
        self.last_speed_kmh
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Grafo lineare 0-1-2-3-4 con coordinate distanziate ~127 m in longitudine.
    fn line_graph_json() -> String {
        let nodes: Vec<String> = (0..5)
            .map(|i| format!("[45.07,{:.6}]", 7.68 + i as f64 * 0.00127))
            .collect();
        let adj = ["[1]", "[0,2]", "[1,3]", "[2,4]", "[3]"];
        format!(
            "{{\"nodes\":[{}],\"adj\":[{}]}}",
            nodes.join(","),
            adj.join(",")
        )
    }

    #[test]
    fn parses_and_starts_on_a_node() {
        let s = Simulator::build(&line_graph_json(), 1.0).expect("costruzione");
        assert!((s.lat() - 45.07).abs() < 1e-6);
    }

    #[test]
    fn rejects_bad_json() {
        assert!(Simulator::build("non-json", 1.0).is_err());
    }

    #[test]
    fn deterministic_with_same_seed() {
        let mut a = Simulator::build(&line_graph_json(), 999.0).unwrap();
        let mut b = Simulator::build(&line_graph_json(), 999.0).unwrap();
        for _ in 0..300 {
            a.advance(0.5);
            b.advance(0.5);
        }
        assert_eq!(a.lat(), b.lat());
        assert_eq!(a.lon(), b.lon());
    }

    #[test]
    fn stays_on_the_road_line() {
        // Su un grafo lineare la latitudine resta costante e la longitudine
        // dentro l'estensione del grafo: il veicolo non "vola" fuori strada.
        let mut s = Simulator::build(&line_graph_json(), 7.0).unwrap();
        for _ in 0..1000 {
            s.advance(0.3);
            assert!((s.lat() - 45.07).abs() < 1e-6);
            assert!(s.lon() >= 7.68 - 1e-3);
            assert!(s.lon() <= 7.68 + 4.0 * 0.00127 + 1e-3);
        }
    }

    #[test]
    fn small_steps_move_smoothly() {
        // Passi piccoli ⇒ spostamenti piccoli (no "salti"): a ~50 km/h in 0.1 s
        // ci si sposta ~1.4 m, ben sotto la lunghezza di un segmento.
        let mut s = Simulator::build(&line_graph_json(), 3.0).unwrap();
        // Salta eventuali pause iniziali: assicura stato di guida.
        s.pause_secs_left = 0.0;
        s.drive_secs_left = 1000.0;
        let (lat0, lon0) = (s.lat(), s.lon());
        s.advance(0.1);
        let moved = haversine_m(lat0, lon0, s.lat(), s.lon());
        assert!(moved < 5.0, "spostamento per step troppo grande: {moved} m");
    }

    #[test]
    fn speed_zero_while_paused() {
        let mut s = Simulator::build(&line_graph_json(), 42.0).unwrap();
        s.pause_secs_left = 100.0;
        let (lat0, lon0) = (s.lat(), s.lon());
        s.advance(1.0);
        assert_eq!(s.speed_kmh(), 0.0);
        assert!(!s.moving());
        assert_eq!(s.lat(), lat0);
        assert_eq!(s.lon(), lon0);
    }
}
