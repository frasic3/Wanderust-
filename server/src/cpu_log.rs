use cpu_time::ProcessTime;
use std::io::Write;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time;

const LOG_INTERVAL: Duration = Duration::from_secs(120);
const LOG_FILE: &str = "server_cpu.log";

/// Task in background: ogni 2 minuti misura il tempo CPU usato dal processo
/// e lo appende a `server_cpu.log`.
///
/// Riporta due valori per ogni riga:
/// - `cpu_totale`: CPU time cumulativo dall'avvio del logger (user+system).
/// - `cpu_intervallo`: CPU time consumato negli ultimi 2 minuti.
pub async fn start_cpu_logger() {
    tracing::info!("cpu_log: avviato, log su '{LOG_FILE}' ogni {}s", LOG_INTERVAL.as_secs());

    let t_start = ProcessTime::now();
    let mut t_prev = ProcessTime::now();

    let mut ticker = time::interval(LOG_INTERVAL);
    ticker.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
    ticker.tick().await; // salta il tick immediato iniziale

    loop {
        ticker.tick().await;

        let cpu_total = t_start.elapsed();
        let cpu_interval = t_prev.elapsed();
        t_prev = ProcessTime::now();

        write_entry(cpu_total, cpu_interval);
    }
}

fn write_entry(total: Duration, interval: Duration) {
    let wall = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let h = (wall / 3600) % 24;
    let m = (wall / 60) % 60;
    let s = wall % 60;

    // Normalizza la % sul numero di core logici. Esempio: 1 core saturo per
    // 2 minuti su una macchina a 8 core = 1/8 = 12.5%, non 100%. Senza
    // normalizzazione la percentuale può sforare 100 (più core saturi
    // contemporaneamente) e perde leggibilità.
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1) as f64;
    let pct = (interval.as_secs_f64() / (LOG_INTERVAL.as_secs_f64() * cores)) * 100.0;

    let line = format!(
        "[{wall}] [{h:02}:{m:02}:{s:02} UTC]  cpu_totale={:.3}s  cpu_intervallo_2min={:.3}s  cpu_pct={:.2}%  cores={}\n",
        total.as_secs_f64(),
        interval.as_secs_f64(),
        pct,
        cores as u64,
    );

    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(LOG_FILE)
    {
        Ok(mut f) => {
            if let Err(e) = f.write_all(line.as_bytes()) {
                tracing::warn!("cpu_log: scrittura su {LOG_FILE} fallita: {e}");
            }
        }
        Err(e) => tracing::warn!("cpu_log: apertura {LOG_FILE} fallita: {e}"),
    }
}
