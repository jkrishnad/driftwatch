mod output;
mod rpc;

use std::time::Duration;

use anyhow::Result;
use aya::programs::TracePoint;
use clap::{Parser, Subcommand};
use log::{debug, warn};
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
    /// Load the eBPF tracepoint profiler (block_rq_issue). Linux only, needs root.
    Watch,
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
        Cmd::Watch => watch().await,
    }
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

/// Load + attach the eBPF tracepoint program, stream its logs.
async fn watch() -> Result<()> {
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

    // This will include your eBPF object file as raw bytes at compile-time and load it at
    // runtime. This approach is recommended for most real-world use cases. If you would
    // like to specify the eBPF program at runtime rather than at compile-time, you can
    // reach for `Bpf::load_file` instead.
    let mut ebpf = aya::Ebpf::load(aya::include_bytes_aligned!(concat!(
        env!("OUT_DIR"),
        "/driftwatch"
    )))?;
    match aya_log::EbpfLogger::init(&mut ebpf) {
        Err(e) => {
            // This can happen if you remove all log statements from your eBPF program.
            warn!("failed to initialize eBPF logger: {e}");
        }
        Ok(logger) => {
            let mut logger =
                tokio::io::unix::AsyncFd::with_interest(logger, tokio::io::Interest::READABLE)?;
            tokio::task::spawn(async move {
                loop {
                    let mut guard = logger.readable_mut().await.unwrap();
                    guard.get_inner_mut().flush();
                    guard.clear_ready();
                }
            });
        }
    }
    let program: &mut TracePoint = ebpf.program_mut("driftwatch").unwrap().try_into()?;
    program.load()?;
    program.attach("block", "block_rq_issue")?;

    let ctrl_c = signal::ctrl_c();
    println!("Waiting for Ctrl-C...");
    ctrl_c.await?;
    println!("Exiting...");

    Ok(())
}
