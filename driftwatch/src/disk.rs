// The box-layer signal: consume DiskEvents off the kernel ringbuf.
// For now: print each event. Later this feeds the correlator's rolling window.

use std::time::Duration;

use anyhow::Result;
use aya::maps::{MapData, PerCpuArray, RingBuf};
use driftwatch_common::{DiskEvent, RW_READ, RW_WRITE};
use tokio::io::unix::AsyncFd;

/// Drain the ringbuf forever; wakes only when the kernel signals data.
pub async fn consume(ring: RingBuf<MapData>) -> Result<()> {
    let mut fd = AsyncFd::with_interest(ring, tokio::io::Interest::READABLE)?;
    loop {
        let mut guard = fd.readable_mut().await?;
        let ring = guard.get_inner_mut();
        while let Some(item) = ring.next() {
            // Kernel wrote a #[repr(C)] DiskEvent; ringbuf data is 8-byte aligned.
            let ev: DiskEvent = unsafe { std::ptr::read_unaligned(item.as_ptr().cast()) };
            println!("{}", line(&ev));
        }
        guard.clear_ready();
    }
}

/// Watch the kernel's drop counter; warn only when it grows. A growing count
/// means the ringbuf overflowed and our percentiles are missing events.
pub async fn watch_drops(drops: PerCpuArray<MapData, u64>) {
    let mut last: u64 = 0;
    let mut ticker = tokio::time::interval(Duration::from_secs(5));
    loop {
        ticker.tick().await;
        // One counter slot, one value per CPU — sum them.
        let total: u64 = match drops.get(&0, 0) {
            Ok(per_cpu) => per_cpu.iter().sum(),
            Err(_) => continue,
        };
        if total > last {
            eprintln!(
                "WARN: ringbuf dropped {} events (total {total})",
                total - last
            );
            last = total;
        }
    }
}

/// e.g: disk 259:0 W 4.0K 812µs
///      disk 259:0 R 128.0K 3.1ms ERR=-5
fn line(ev: &DiskEvent) -> String {
    let rw = match ev.rw {
        RW_READ => 'R',
        RW_WRITE => 'W',
        _ => 'O',
    };
    let major = ev.dev >> 20;
    let minor = ev.dev & ((1 << 20) - 1);
    let err = if ev.error != 0 {
        format!(" ERR={}", ev.error)
    } else {
        String::new()
    };
    format!(
        "disk {major}:{minor} {rw} {} {}{err}",
        size(ev.bytes),
        latency(ev.latency_ns),
    )
}

fn latency(ns: u64) -> String {
    if ns >= 1_000_000 {
        format!("{:.1}ms", ns as f64 / 1e6)
    } else {
        format!("{}µs", ns / 1_000)
    }
}

fn size(bytes: u32) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1}M", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1}K", bytes as f64 / 1024.0)
    }
}
