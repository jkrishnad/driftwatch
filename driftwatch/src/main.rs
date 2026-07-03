mod correlate;
mod disk;
mod output;
mod rpc;

use std::time::Duration;

use anyhow::Result;
use aya::programs::TracePoint;
use clap::{Parser, Subcommand};
use log::debug;
use tokio::signal;

#[derive(Parser)]
#[command(
    name = "driftwatch",
    about = "eBPF disk profiler + validator RPC context"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Poll the validator's RPC and print a live status line. No eBPF.
    Poll {
        /// Validator JSON-RPC endpoint.
        #[arg(long, default_value = "http://127.0.0.1:8899")]
        rpc: String,
        /// Vote account pubkey to track (auto-discovered on test-validator).
        #[arg(long)]
        vote: Option<String>,
        /// Seconds between polls.
        #[arg(long, default_value_t = 2)]
        interval: u64,
    },
    /// Run the eBPF disk-latency profiler (block_rq_issue -> block_rq_complete).
    /// Linux only, needs root.
    Watch {
        /// Only trace this block device, as "major:minor" (e.g. 259:0 — find
        /// yours with `lsblk`). Default: all devices.
        #[arg(long)]
        dev: Option<String>,
        /// Seconds per summary window.
        #[arg(long, default_value_t = 3)]
        window: u64,
        /// Also print every raw event (debug firehose).
        #[arg(long)]
        raw: bool,
    },
    /// The joined tool: disk profiler + RPC poller in one process, one timeline.
    /// One combined line (or JSON object) per disk window.
    Run {
        /// Block device to trace, "major:minor" (the ledger volume).
        #[arg(long)]
        dev: Option<String>,
        /// Seconds per window (the timeline heartbeat).
        #[arg(long, default_value_t = 3)]
        window: u64,
        /// Validator JSON-RPC endpoint.
        #[arg(long, default_value = "http://127.0.0.1:8899")]
        rpc: String,
        /// Vote account pubkey (auto-discovered on test-validator).
        #[arg(long)]
        vote: Option<String>,
        /// Seconds between RPC polls.
        #[arg(long, default_value_t = 2)]
        interval: u64,
        /// Emit JSON objects instead of human lines.
        #[arg(long)]
        json: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    match Cli::parse().cmd {
        Cmd::Poll {
            rpc,
            vote,
            interval,
        } => poll(rpc, vote, interval).await,
        Cmd::Watch { dev, window, raw } => watch(dev, window, raw).await,
        Cmd::Run {
            dev,
            window,
            rpc,
            vote,
            interval,
            json,
        } => run(dev, window, rpc, vote, interval, json).await,
    }
}

/// "259:0" -> kernel dev_t encoding (major << 20 | minor), same as the
/// tracepoint's dev field.
fn parse_dev(s: &str) -> Result<u32> {
    let (major, minor) = s
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("--dev wants major:minor, e.g. 259:0"))?;

    let major: u32 = major.trim().parse()?;
    let minor: u32 = minor.trim().parse()?;
    Ok((major << 20) | minor)
}

/// The RPC poll loop. Ask, print, repeat. Ctrl-C to stop.
async fn poll(rpc_url: String, vote: Option<String>, interval: u64) -> Result<()> {
    let mut poller = rpc::RpcPoller::new(&rpc_url);
    if let Some(pk) = vote {
        poller = poller.with_vote_pubkey(pk);
    }

    println!("driftwatch — polling {rpc_url} every {interval}s (Ctrl-C to stop)\n");
    let mut ticker = tokio::time::interval(Duration::from_secs(interval));

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                // Never fails: an unreachable RPC arrives as a DOWN sample,
                // same stream as OK ones — outages are data, not stderr noise.
                let sample = poller.sample().await;
                println!("{}", output::status_line(&sample));
            }
            _ = signal::ctrl_c() => {
                println!("\nstopping.");
                return Ok(());
            }
        }
    }
}

/// Live eBPF handle + the two maps the daemon reads. Keep the Ebpf alive —
/// dropping it detaches the programs.
type Profiler = (
    aya::Ebpf,
    aya::maps::RingBuf<aya::maps::MapData>,
    aya::maps::PerCpuArray<aya::maps::MapData, u64>,
);

/// Load the eBPF object, patch the device filter, attach both block tracepoints.
fn load_profiler(dev: &Option<String>) -> Result<Profiler> {
    // Bump the memlock rlimit. This is needed for older kernels that don't use the
    // new memcg based accounting, see https://lwn.net/Articles/837122/
    let rlim = libc::rlimit {
        rlim_cur: libc::RLIM_INFINITY,
        rlim_max: libc::RLIM_INFINITY,
    };
    let ret = unsafe { libc::setrlimit(libc::RLIMIT_MEMLOCK, &rlim) };
    if ret != 0 {
        debug!("remove limit on locked memory failed, ret is: {ret}");
    }

    // The eBPF object file is embedded at compile time. The volume filter is a
    // global in that object, patched before load — the kernel never sees other
    // devices' events at all.
    let target_dev = match dev {
        Some(s) => parse_dev(s)?,
        None => 0, // accept all
    };
    let mut loader = aya::EbpfLoader::new();
    loader.override_global("TARGET_DEV", &target_dev, true);
    let mut ebpf = loader.load(aya::include_bytes_aligned!(concat!(
        env!("OUT_DIR"),
        "/driftwatch"
    )))?;
    if let Err(e) = aya_log::EbpfLogger::init(&mut ebpf) {
        // Expected: the kernel program has no log statements.
        debug!("no eBPF logger: {e}");
    }

    // Attach both hooks: issue stamps the stopwatch, complete emits the event.
    for name in ["block_rq_issue", "block_rq_complete"] {
        let program: &mut TracePoint = ebpf
            .program_mut(name)
            .ok_or_else(|| anyhow::anyhow!("program {name} not found in object"))?
            .try_into()?;
        program.load()?;
        program.attach("block", name)?;
    }

    let events = aya::maps::RingBuf::try_from(
        ebpf.take_map("EVENTS")
            .ok_or_else(|| anyhow::anyhow!("EVENTS map not found"))?,
    )?;
    let drops = aya::maps::PerCpuArray::try_from(
        ebpf.take_map("DROPS")
            .ok_or_else(|| anyhow::anyhow!("DROPS map not found"))?,
    )?;
    Ok((ebpf, events, drops))
}

/// The profiler alone: windowed disk summaries to stdout.
async fn watch(dev: Option<String>, window: u64, raw: bool) -> Result<()> {
    let (_ebpf, events, drops) = load_profiler(&dev)?;
    tokio::spawn(disk::watch_drops(drops));

    match &dev {
        Some(d) => println!("driftwatch — profiling block device {d} (Ctrl-C to stop)\n"),
        None => println!("driftwatch — profiling ALL block devices (Ctrl-C to stop)\n"),
    }

    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let printer = async {
        while let Some(stats) = rx.recv().await {
            println!("{}", disk::format_stats(&stats));
        }
    };

    tokio::select! {
        res = disk::consume(events, window, raw, tx) => res,
        _ = printer => Ok(()),
        _ = signal::ctrl_c() => {
            println!("\nstopping.");
            Ok(())
        }
    }
    // _ebpf drops here -> programs detach, kernel side fully unloads.
}

/// The joined tool: profiler + poller, one timeline, one line per window.
async fn run(
    dev: Option<String>,
    window: u64,
    rpc_url: String,
    vote: Option<String>,
    interval: u64,
    json: bool,
) -> Result<()> {
    let (_ebpf, events, drops) = load_profiler(&dev)?;
    tokio::spawn(disk::watch_drops(drops));

    let mut poller = rpc::RpcPoller::new(&rpc_url);
    if let Some(pk) = vote {
        poller = poller.with_vote_pubkey(pk);
    }

    if !json {
        println!(
            "driftwatch — disk {} + validator {rpc_url}, {window}s windows (Ctrl-C to stop)\n",
            dev.as_deref().unwrap_or("ALL")
        );
    }

    let (disk_tx, disk_rx) = tokio::sync::mpsc::channel(64);
    let (rpc_tx, rpc_rx) = tokio::sync::mpsc::channel(16);
    tokio::spawn(rpc::poll_stream(poller, interval, rpc_tx));

    tokio::select! {
        res = disk::consume(events, window, false, disk_tx) => res,
        _ = correlate::combine(disk_rx, rpc_rx, json) => Ok(()),
        _ = signal::ctrl_c() => {
            if !json {
                println!("\nstopping.");
            }
            Ok(())
        }
    }
}
