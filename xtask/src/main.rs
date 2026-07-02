use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

// Lima VM "ebpf" — port is dynamic (changes on restart), so we use Lima's own
// ssh.config file which Lima keeps up-to-date automatically.
const SSH_CONFIG: &str = "/Users/jayakrishna/.lima/ebpf/ssh.config";
const SSH_HOST: &str = "lima-ebpf";
const LOCAL_DIR: &str = "/Users/jayakrishna/Documents/svm/driftwatch";
// Mac home is mounted read-only inside the VM, so we sync to the VM's own
// writable home directory instead of building in-place.
const REMOTE_DIR: &str = "/home/jayakrishna.linux/driftwatch";

#[derive(Parser)]
#[command(
    name = "driftwatch",
    about = "Build & run the aya tracepoint program in a local Lima VM over SSH"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// One-time: install rust nightly + bpf-linker + build deps in the VM.
    Setup,
    /// rsync code to the VM and build it there (eBPF built automatically by build script).
    Build,
    /// Sync + build + run driftwatch in the VM. Ctrl-C to stop.
    /// Args after `--` go to the binary, e.g: cargo xtask run -- poll
    Run {
        /// Arguments passed through to the driftwatch binary (default: watch).
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// Run clippy in the VM.
    Check,
    /// Just push local code to the VM (no build).
    Sync,
    /// Open an interactive shell in the VM.
    Ssh,
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Setup => setup()?,
        Cmd::Sync => sync()?,
        Cmd::Build => {
            sync()?;
            remote(&format!("cd {REMOTE_DIR} && cargo build --release"))?;
        }
        Cmd::Run { args } => {
            sync()?;
            let bin_args = if args.is_empty() {
                "watch".to_string()
            } else {
                args.join(" ")
            };
            // Clear stale tracepoint links from a previous unclean exit, then
            // build and run. sudo needed for `watch` (eBPF load); harmless for `poll`.
            remote(&format!(
                "cd {REMOTE_DIR} && \
                 sudo bpftool link list 2>/dev/null \
                   | awk -F: '/tracepoint/{{print $1}}' \
                   | xargs -r -I{{}} sudo bpftool link detach id {{}} 2>/dev/null; \
                 cargo build --release -p driftwatch && \
                 sudo RUST_LOG=info ./target/release/driftwatch {bin_args}"
            ))?;
        }
        Cmd::Check => {
            sync()?;
            remote(&format!("cd {REMOTE_DIR} && cargo clippy --all-targets"))?;
        }
        Cmd::Ssh => {
            // Interactive: hand the terminal over to ssh via Lima's config.
            run("ssh", &["-F", SSH_CONFIG, "-t", SSH_HOST])?;
        }
    }
    Ok(())
}

/// Install the eBPF toolchain in the VM (idempotent).
fn setup() -> Result<()> {
    eprintln!("[xtask] Installing toolchain in Lima VM {SSH_HOST} (first run is slow)...");
    // aarch64 Ubuntu VM — bpf-linker has no prebuilt aarch64 binary so we
    // install from source via cargo install. That build links against the
    // system LLVM we install here (llvm-dev), avoiding the dlopen/linker-script
    // issue that bit us when using binstall on x86 with rustup's bundled LLVM.
    remote(
        "set -eux; \
         export DEBIAN_FRONTEND=noninteractive; \
         sudo apt-get update -qq; \
         sudo apt-get install -y --fix-missing build-essential pkg-config libelf-dev \
           clang llvm llvm-dev libclang-dev curl rsync; \
         if ! command -v rustup >/dev/null; then \
           curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y; \
         fi; \
         source \"$HOME/.cargo/env\"; \
         rustup toolchain install stable --profile default; \
         rustup toolchain install nightly --profile default --component rust-src; \
         rustup default nightly; \
         command -v bpf-linker >/dev/null || cargo install bpf-linker",
    )
    .context("VM toolchain install failed")?;
    eprintln!("[xtask] Setup complete. Try: cargo xtask run");
    Ok(())
}

/// Push local code to the VM, excluding build artifacts and git.
fn sync() -> Result<()> {
    eprintln!("[driftwatch] Syncing {LOCAL_DIR} -> {SSH_HOST}:{REMOTE_DIR}");
    run(
        "rsync",
        &[
            "-az",
            "--delete",
            "--exclude",
            "target/",
            "--exclude",
            ".git/",
            "-e",
            &format!("ssh -F {SSH_CONFIG}"),
            &format!("{LOCAL_DIR}/"),
            &format!("{SSH_HOST}:{REMOTE_DIR}/"),
        ],
    )
    .context("rsync to VM failed")
}

/// Run a command in the VM over SSH.
/// Explicitly sources ~/.cargo/env because bash -lc doesn't always pick it up
/// when SSH has no PTY (stdin not a terminal in a subprocess context).
fn remote(script: &str) -> Result<()> {
    let with_env = format!(
        "source \"$HOME/.cargo/env\" 2>/dev/null || true; {}",
        script
    );
    let quoted = format!("bash -lc '{}'", with_env.replace('\'', r"'\''"));
    run("ssh", &["-F", SSH_CONFIG, "-t", SSH_HOST, &quoted])
}

fn run(program: &str, args: &[&str]) -> Result<()> {
    eprintln!("[xtask] $ {program} {}", args.join(" "));
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("failed to spawn `{program}` (installed and on PATH?)"))?;
    if !status.success() {
        match status.code() {
            Some(code) => bail!("`{program}` exited with status {code}"),
            None => bail!("`{program}` was terminated by a signal"),
        }
    }
    Ok(())
}
