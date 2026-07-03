// One timeline: hold the latest validator sample, emit one combined line
// (or JSON object) per disk window. No judgment yet — align and show.
// Trend + lead-signal detection lands here later.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde_json::json;
use tokio::sync::mpsc::Receiver;

use crate::disk::{WindowStats, compact_stats};
use crate::output;
use crate::rpc::{Sample, ValidatorSample};

/// The disk window is the heartbeat: one output line per window, carrying
/// whatever the validator looked like at that moment.
pub async fn combine(
    mut disk_rx: Receiver<WindowStats>,
    mut rpc_rx: Receiver<Sample>,
    json_mode: bool,
) {
    let started = Instant::now();
    let mut latest: Option<Sample> = None;
    loop {
        tokio::select! {
            s = rpc_rx.recv() => match s {
                Some(s) => latest = Some(s),
                None => return,
            },
            w = disk_rx.recv() => match w {
                Some(w) => emit(&w, latest.as_ref(), json_mode, started),
                None => return,
            },
        }
    }
}

fn emit(w: &WindowStats, v: Option<&Sample>, json_mode: bool, started: Instant) {
    if json_mode {
        println!("{}", to_json(w, v));
    } else {
        let val = match v {
            Some(s) => output::compact(s),
            None => "validator: no sample yet".into(),
        };
        println!("{} | {} || {}", elapsed(started), compact_stats(w), val);
    }
}

fn to_json(w: &WindowStats, v: Option<&Sample>) -> String {
    let validator = match v {
        Some(Sample::Up(s)) => up_json(s),
        Some(Sample::Down { reason }) => json!({ "state": "down", "reason": reason }),
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
