// One timeline: latest validator sample + one line (or JSON) per disk window.
// Drift rule: disk p99 sustained above its baseline AND lag above its norm
// -> one alert naming disk. Both must hold; a lone spike = silence.

use std::{
    collections::VecDeque,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use serde_json::json;
use tokio::sync::mpsc::Receiver;

use crate::{
    disk::{WindowStats, compact_stats},
    output,
    rpc::{Sample, ValidatorSample},
};

const HISTORY: usize = 20; // baseline memory (windows)
const MIN_HISTORY: usize = 5; // don't judge before this much normal history
const ELEVATION_FACTOR: u64 = 4; // p99 must be 4x baseline...
const P99_FLOOR_NS: u64 = 1_000_000; // ...and above 1ms (µs noise can't trip it)
const STREAK: u32 = 3; // sustained for this many consecutive windows
const LAG_DELTA: i64 = 3; // lag must exceed its norm by this many slots

/// One output per disk window, carrying the latest validator sample.
pub async fn combine(
    mut disk_rx: Receiver<WindowStats>,
    mut rpc_rx: Receiver<Sample>,
    json_mode: bool,
) {
    let started = Instant::now();
    let mut latest: Option<Sample> = None;
    let mut detector = DriftDetector::default();
    loop {
        tokio::select! {
            s = rpc_rx.recv() => match s {
                Some(s) => latest = Some(s),
                None => return,
            },
            w = disk_rx.recv() => match w {
                Some(w) => {
                    let drift = detector.observe(&w, latest.as_ref());
                    emit(&w, latest.as_ref(), json_mode, started, drift.as_ref());
                }
                None => return,
            },
        }
    }
}

/// A fired verdict: disk elevated AND validator feeling it.
struct Drift {
    p99_ns: u64,
    baseline_ns: u64,
    streak: u32,
    lag: i64,
    lag_norm: i64,
}

#[derive(Default)]
struct DriftDetector {
    p99_history: VecDeque<u64>, // baseline: p99 of recent normal windows
    lag_history: VecDeque<i64>, // norm: lag of recent normal windows
    streak: u32,
    fired: bool, // one alert per episode
}

impl DriftDetector {
    fn observe(&mut self, w: &WindowStats, v: Option<&Sample>) -> Option<Drift> {
        // idle window = no info
        if w.reqs == 0 {
            return None;
        }
        // too little history: just learn
        if self.p99_history.len() < MIN_HISTORY {
            self.remember(w, v);
            return None;
        }

        let baseline = median_u64(&self.p99_history);
        let elevated =
            w.p99_ns > baseline.saturating_mul(ELEVATION_FACTOR) && w.p99_ns > P99_FLOOR_NS;

        if !elevated {
            // back to normal: feed baseline, re-arm
            self.streak = 0;
            self.fired = false;
            self.remember(w, v);
            return None;
        }

        // elevated windows are not remembered (would poison the baseline)
        self.streak += 1;
        if self.streak < STREAK || self.fired {
            return None;
        }

        // disk sustained-elevated: does the validator feel it?
        let lag_norm = median_i64(&self.lag_history);
        let lag = match v {
            Some(Sample::Up(s)) => s.vote_lag,
            Some(Sample::Down { .. }) => i64::MAX, // down = worst case
            None => return None,
        };
        if lag >= lag_norm + LAG_DELTA {
            self.fired = true;
            return Some(Drift {
                p99_ns: w.p99_ns,
                baseline_ns: baseline,
                streak: self.streak,
                lag,
                lag_norm,
            });
        }
        None
    }

    fn remember(&mut self, w: &WindowStats, v: Option<&Sample>) {
        push_capped(&mut self.p99_history, w.p99_ns);
        if let Some(Sample::Up(s)) = v {
            push_capped(&mut self.lag_history, s.vote_lag);
        }
    }
}

fn push_capped<T>(q: &mut VecDeque<T>, v: T) {
    if q.len() == HISTORY {
        q.pop_front();
    }
    q.push_back(v);
}

fn median_u64(q: &VecDeque<u64>) -> u64 {
    let mut v: Vec<u64> = q.iter().copied().collect();
    v.sort_unstable();
    if v.is_empty() { 0 } else { v[v.len() / 2] }
}

fn median_i64(q: &VecDeque<i64>) -> i64 {
    let mut v: Vec<i64> = q.iter().copied().collect();
    v.sort_unstable();
    if v.is_empty() { 0 } else { v[v.len() / 2] }
}

fn emit(
    w: &WindowStats,
    v: Option<&Sample>,
    json_mode: bool,
    started: Instant,
    drift: Option<&Drift>,
) {
    if json_mode {
        println!("{}", to_json(w, v, drift));
    } else {
        let val = match v {
            Some(s) => output::compact(s),
            None => "validator: no sample yet".into(),
        };
        println!("{} | {} || {}", elapsed(started), compact_stats(w), val);
        if let Some(d) = drift {
            let lag = if d.lag == i64::MAX {
                "validator DOWN".into()
            } else {
                format!("vote lag {} vs norm {}", d.lag, d.lag_norm)
            };
            println!(
                "!! DRIFT: disk p99 {} = {}x baseline {} for {} windows — {} — disk is the lead signal",
                crate::disk::human_latency(d.p99_ns),
                d.p99_ns / d.baseline_ns.max(1),
                crate::disk::human_latency(d.baseline_ns),
                d.streak,
                lag,
            );
        }
    }
}

fn to_json(w: &WindowStats, v: Option<&Sample>, drift: Option<&Drift>) -> String {
    let validator = match v {
        Some(Sample::Up(s)) => up_json(s),
        Some(Sample::Down { reason }) => json!({ "state": "down", "reason": reason }),
        None => serde_json::Value::Null,
    };
    let drift = match drift {
        Some(d) => json!({
            "p99_us": d.p99_ns / 1_000,
            "baseline_us": d.baseline_ns / 1_000,
            "factor": d.p99_ns / d.baseline_ns.max(1),
            "windows": d.streak,
            "lag": if d.lag == i64::MAX { serde_json::Value::Null } else { json!(d.lag) },
            "lag_norm": d.lag_norm,
            "lead_signal": "disk",
        }),
        None => serde_json::Value::Null,
    };
    json!({
        "ts": epoch_secs(),
        "disk": {
            "window_secs": w.window_secs,
            "reqs": w.reqs,
            "writes": w.writes,
            "reads": w.reads,
            "others": w.others,
            "bytes": w.bytes,
            "errors": w.errors,
            "p50_us": w.p50_ns / 1_000,
            "p99_us": w.p99_ns / 1_000,
            "max_us": w.max_ns / 1_000,
        },
        "validator": validator,
        "drift": drift,
    })
    .to_string()
}

fn up_json(s: &ValidatorSample) -> serde_json::Value {
    json!({
        "state": output::state(s).to_lowercase(),
        "epoch": s.epoch,
        "slot": s.network_slot,
        "last_vote": s.my_last_vote,
        "vote_lag": s.vote_lag,
        "credits": s.credits,
        "delinquent": s.delinquent,
        "healthy": s.healthy,
    })
}

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Stopwatch since start — timezone-free for humans. JSON keeps absolute ts.
fn elapsed(started: Instant) -> String {
    let secs = started.elapsed().as_secs();
    if secs >= 3600 {
        format!("+{}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60)
    } else {
        format!("+{:02}:{:02}", secs / 60, secs % 60)
    }
}
