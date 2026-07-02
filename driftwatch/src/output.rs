// Turn a ValidatorSample into a human-readable status line.
// Later this grows a JSON-per-window emitter and the drift alert.

use crate::rpc::{Sample, ValidatorSample};

/// Format one poll as a single status line.
/// e.g: epoch 0 | slot 1,084 | last vote 1,083 | lag 1 | credits 4,201 | OK
/// or:  DOWN | getEpochInfo: request failed: ...
pub fn status_line(sample: &Sample) -> String {
    match sample {
        Sample::Up(s) => up_line(s),
        Sample::Down { reason } => format!("DOWN | {reason}"),
    }
}

fn up_line(s: &ValidatorSample) -> String {
    let state = if !s.healthy {
        "UNHEALTHY"
    } else if s.delinquent {
        "DELINQUENT"
    } else if s.vote_lag > 150 {
        // rough "falling behind" heuristic; tuned properly later
        "LAGGING"
    } else {
        "OK"
    };

    format!(
        "epoch {} | slot {} | last vote {} | lag {} | credits {} | {}",
        s.epoch,
        commas(s.network_slot),
        commas(s.my_last_vote),
        s.vote_lag,
        commas(s.credits),
        state,
    )
}

/// Thousands separators, no external crate. Comma before every group of 3 from the right.
fn commas(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    let len = digits.len();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}
