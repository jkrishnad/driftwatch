# driftwatch — Plan

> An eBPF tool that watches a Solana validator's disk I/O at the kernel level and
> warns when it starts dragging vote credits toward the SFDP cutoff — naming the
> cause before it costs delegation.
>
> Box-level cause → credit-level effect → early warning → named.
> By **sonar Labs**.

---

## 1. Thesis (why this is worth building)

Existing validator monitors are **single-layer**: either box health (node-exporter,
disk dashboards) or credit/delinquency (validators.app, stakewiz). Nobody lines the
two layers up on one timeline and says _"your disk p99 rose 4 min before your credits
started slipping — that's the cause."_

driftwatch's value is the **join + lead-signal naming**, not either probe alone.

**Differentiator (the moat):** correlated cause naming, not just two dashboards.

---

## 2. Architecture

```
                 ┌─────────────────────── validator box ───────────────────────┐
   kernel        │   block_rq_issue ─┐                                         │
   (dumb)        │                   ├─► HashMap<(dev,sector), start_ns>       │
                 │   block_rq_complete ─► latency = now - start ─► RINGBUF ──┐ │
                 │                                                           │ │
   userspace     │   disk task ◄── DiskEvent ────────────────────────────────┘ │
   (brains)      │       │                                                     │
                 │   rpc task ──► getVoteAccounts / getEpochInfo / getHealth   │
                 │       │                                                     │
                 │   correlator (rolling window) ─► trend + lead-signal logic  │
                 │       │                                                     │
                 │   alert engine (SFDP thresholds) ─► verdict                 │
                 │       │                                                     │
                 │   output: live status line | drift alert | JSON-per-window  │
                 └─────────────────────────────────────────────────────────────┘
```

**The split:** kernel = stamp / subtract / emit (tiny, no judgment).
Daemon = collect / correlate / alert (all the brains).

**The causal chain (the heart of the design):**

```
disk p99 rises   →   vote-slot lag grows   →   credits slip
  (eBPF, fast)        (RPC, fast)               (RPC, slow — the SFDP outcome)
```

**Vote-slot lag is the middle term that makes correlation tractable.** Disk events
are microseconds; credits move per-epoch — too far apart to line up directly. Vote
lag moves fast _like disk does_, so we correlate **disk ↔ vote-lag** (both fast, same
timescale) and let credits confirm the chain afterward. This is the key fix over the
naïve "disk vs credits" approach.

`vote_lag = current_slot (getEpochInfo) − lastVote (getVoteAccounts)`.

---

## 3. Crate layout

| Crate               | Role                                                                                | Status                           |
| ------------------- | ----------------------------------------------------------------------------------- | -------------------------------- |
| `driftwatch-ebpf`   | `no_std` tracepoint program. block_rq_issue + block_rq_complete, HashMap + ringbuf. | scaffold fires; needs real logic |
| `driftwatch-common` | shared `DiskEvent` struct (kernel ↔ user). `no_std`, `user` feature for aya `Pod`.  | empty                            |
| `driftwatch`        | daemon: `rpc`, `disk`, `correlate`, `verdict`, `output` modules.                    | attaches tracepoint, logs only   |
| `xtask`             | SSH/rsync deploy to Lima `ebpf` VM.                                                 | **done** — build/run/setup work  |

---

## 4. Data contracts

**`DiskEvent`** (kernel → user, in `driftwatch-common`, `#[repr(C)]`, `Pod`):

```
dev:       u32   // block device id (major:minor) — for volume filtering
sector:    u64   // request sector (debug / key echo)
latency_ns:u64   // complete_ts - issue_ts
issue_ns:  u64   // kernel monotonic ts at issue (for timeline align)
rw:        u8    // read=0 / write=1
```

**`ValidatorSample`** (RPC → correlator, daemon-only):

```
// fast signals (polled every few seconds, own node only)
current_slot, last_vote_slot,
vote_lag_slots (= current_slot - last_vote_slot),   // the fast middle term
delinquent: bool, balance_lamports,

// slow signals (refreshed once per epoch, full-cluster call)
epoch, my_credits, my_credits_delta,
peer_median_credits, my_ratio (= my_credits / peer_median)   // the SFDP gate
```

**`WindowRecord`** (correlator → output, the JSON-per-window line):

```
window_start, window_end,
disk: { p50_ns, p99_ns, max_ns, iops, queue_depth },
credit: { ratio, delta_rate, trend },
verdict: { state, lead_signal, projected_breach_epoch }
```

---

## 5. Milestones (order, not schedule)

Each milestone is independently demoable and retires a specific risk.

### M0 — Hook reality check ✅ (DONE)

Confirm `block_rq_issue` fires on the box before writing anything. _Verify-before-eBPF._

- ✅ tracepoint attaches, logs scroll, build/deploy flow proven.

### M1 — RPC poller alone (no eBPF) — `driftwatch/src/rpc.rs`

Plain userspace Rust, no eBPF. Ask the validator how it's doing on a loop; store the
latest answers. Working tool immediately; proves the validator-layer half.

- **Two-speed polling:**
  - _fast loop_ (every few s): `getVoteAccounts` **filtered to own `votePubkey`** (cheap —
    returns just our account) for `lastVote` + delinquency + balance; `getEpochInfo`
    for `current_slot`. Compute `vote_lag = current_slot − lastVote`. `getHealth` as a
    cheap liveness ping.
  - _slow loop_ (once per epoch): full `getVoteAccounts` (all validators) → peer-median
    credits → `my_ratio` for the SFDP gate. This is the only heavy call; do it rarely.
- **Add vote-lag now — it's free** and it's the fast middle term M4 depends on.
- Output: `epoch 612 | slot 264.3M | lag 2 | credits 412,901 | ratio 98.4% | OK`.
- **Risk retired:** RPC shape, payload weight, peer-relative ratio math, lag math.

### M2 — Disk eBPF probe standalone

Block-layer latency events reaching userspace over a **ringbuf**. Just print them.

- ebpf: HashMap keyed `(dev, sector)` on `block_rq_issue`; on `block_rq_complete`
  look up, subtract, push `DiskEvent` to ringbuf, delete entry.
- daemon `disk` module: consume ringbuf, `println!` latency.
- **Volume filter:** resolve ledger/accounts path → block `dev`; drop events from
  other devices (in-kernel if possible, else daemon-side first cut).
- **Risk retired:** the one real-risk part — kernel→user latency pipeline. _eBPF risk dies here._

### M3 — Join the two

Feed both streams into the correlator's rolling window. Emit the combined JSON line
(`WindowRecord`). No judgment yet — just aligned data on one timeline.

### M4 — Trend detection + lead-signal logic

"Disk p99 rose **first**, then vote lag grew **right after**" → candidate alert.

- Correlate **disk ↔ vote-lag** — both fast, same timescale, so this is now a normal
  two-signal lead detection, not the old µs-vs-epoch impossibility.
- Credits/ratio (slow) confirm afterward that the chain was real; they are not the
  thing being time-aligned.
- Lead detection = ordering of inflection points: disk inflection precedes lag inflection.

### M5 — SFDP thresholds into the alert engine

Fire the alert with the named cause; check against the SFDP floor.

- SFDP floor is **relative to cluster** (≈97% of peer benchmark), so `my_ratio` from
  M1 is the input, not an absolute credit number.
- Output: drift alert naming disk as the lead cause when the disk→lag chain fires
  while ratio sits near the floor.
- **v1 scope:** no breach _projection_ — that overpromises. Alert on the observed
  chain + proximity to floor, not a forecast.

### M6 — Real box under load

Run on the validator under real load, induce disk pressure with **`fio`**, capture a
real trace of the disk→lag chain firing, write it up. This is the proof artifact / demo.

---

## 6. Risks & mitigations

| Risk                                               | Severity     | Mitigation                                                                                               |
| -------------------------------------------------- | ------------ | -------------------------------------------------------------------------------------------------------- |
| `block_rq_*` is system-wide, not per-volume        | high         | filter by `dev` (path→major:minor); fold into M2                                                         |
| Time-scale mismatch (µs disk vs per-epoch credits) | ~~high~~ med | **mitigated:** correlate disk ↔ vote-lag (both fast); credits only confirm. Vote-lag is the middle term. |
| SFDP floor is cluster-relative not absolute        | med          | compute peer-median ratio in M1, threshold on ratio                                                      |
| `getVoteAccounts` payload heavy (all validators)   | med          | fast loop filters to own `votePubkey` (cheap); full call only once per epoch                             |
| ringbuf drops under high IOPS                      | med          | size ringbuf generously; count drops; aggregate in-kernel if needed                                      |
| tracepoint field layout varies by kernel           | low          | tracepoints are stable ABI; verified on this box (M0)                                                    |

---

## 7. Current status

- ✅ Workspace scaffolded (aya template), 4 crates.
- ✅ Tracepoint `block/block_rq_issue` attaches and logs on the Lima `ebpf` VM.
- ✅ `xtask` deploy flow: `setup` / `sync` / `build` / `run [-- args]` / `check` / `ssh`.
- ✅ **M1 done — RPC poller live** (`driftwatch poll`): fast-loop getHealth /
  getEpochInfo / getVoteAccounts, vote-lag math, epochCredits parse, vote-account
  auto-discovery, live status line (`OK/LAGGING/DELINQUENT/UNHEALTHY`).
  Verified against solana-test-validator (Mac host) from the VM via `192.168.5.2:8899`.
- ✅ Outage-as-data: failed poll emits a `DOWN` sample into the same stream as OK
  samples (chaos-tested by killing the validator mid-poll; recovery clean).
- ✅ Tracepoint field layouts verified on the VM kernel
  (`/sys/kernel/tracing/events/block/*/format`): `dev` @8 u32, `sector` @16 u64,
  `nr_sector` @24 u32, `bytes`(issue)/`error`(complete) @28, `rwbs` @34 char[10].
  Identical key-field offsets on both hooks.
- ✅ **M2 done — disk profiler raw layer live** (`driftwatch watch [--dev maj:min]`):
  both hooks attached, INFLIGHT LRU stopwatch, DiskEvent over ringbuf, in-kernel
  volume filter (TARGET_DEV global), drop counter surfaced in the daemon.
  **fio proof:** 519,764 issued writes = 519,764 captured events, 0 drops,
  ~65k events/s; p50 100µs / p99 1.9ms / max 13.4ms at iodepth 16.
  (Lesson kept: /tmp is tmpfs in the VM — benchmark against a real mount.)
- ⬜ M3–M6. Next: per-window aggregation in the daemon (p50/p99/IOPS per few
  seconds) — the summary the correlator will consume; raw per-event lines become
  debug output.

**Next action:** M2 kernel side — `DiskEvent` in `driftwatch-common`, maps
(in-flight LRU hash + ringbuf + drop counter), both hooks, then the `disk.rs` consumer.

_Peer-ratio slow loop (full-cluster `getVoteAccounts`) deferred: meaningless on a
single-node test-validator; added when pointing at a real cluster node._

---

## 8. Definition of done

On a real validator, driftwatch prints live health, and when disk latency climbs
_ahead of_ vote-lag growth while the credit ratio sits near the SFDP floor, it fires
one alert naming the disk as the lead cause — backed by a captured real-world trace.

---

## 9. Why only `block_rq_issue` + `block_rq_complete`?

The `block` subsystem exposes ~25 tracepoints, one per stage of a request's life:

```
app issues I/O
   │
   ▼
block_bio_queue        bio enters the block layer
block_getrq            a request struct is allocated
block_bio_*merge       this bio merged into an existing request
block_plug             queue plugged (batching)        ─┐ scheduler holds
block_rq_insert        request placed on scheduler queue │ requests to
block_unplug           queue unplugged                 ─┘ merge / reorder
   │
   ▼
block_rq_issue   ◄──── request HANDED TO THE DEVICE DRIVER     ⬅ START
   │                   (disk physically begins work here)
   ▼  ... hardware services the I/O ...
block_rq_complete ◄─── device reports DONE                     ⬅ END
   │
   ▼
block_io_done / block_bio_complete   bio fully finished, app unblocked
```

**We use this pair because `complete − issue` = the time the physical device spent
servicing the request — i.e. disk service latency.** That is the exact signal that
degrades when the SSD is dying, thermal-throttling, or saturated, and it's our causal
input for the credit drift. (It's the same span `biolatency` from bcc/bpftrace measures.)

**Why not the others:**

| Tracepoint(s)                                          | Measures                        | Why not (for v1)                                                                            |
| ------------------------------------------------------ | ------------------------------- | ------------------------------------------------------------------------------------------- |
| `block_bio_queue` → `complete`                         | full latency incl. queue wait   | mixes software queueing with device time — can't isolate "disk is slow" from "we over-sent" |
| `block_getrq`, `*_merge`, `plug`/`unplug`, `rq_insert` | scheduler / queue internals     | batching & merge behaviour, not device health                                               |
| `block_rq_requeue`, `block_rq_error`                   | retries / failures              | useful **later** as a hard-fault alert, not latency                                         |
| `block_io_start` / `block_io_done`                     | newer wrappers of the same span | redundant with issue/complete, less universally present                                     |

They're not wasted — they answer _different_ questions. issue↔complete is the one
pair that isolates **device service time**, which is what drives our cause→effect story.

**Deliberate future option:** queue time = `block_rq_insert → block_rq_issue`. Capturing
that second span would let us distinguish "disk is physically slow" from "we're flooding
the queue." Not needed for v1 — noted so the v1 scope is a choice, not an oversight.

**Beyond `block` (expansion menu, not skipped work):** the other kernel tracepoint
categories map to later signals — `net`/`tcp`/`napi`/`skb` for the network probe
(packet loss + jitter on gossip/TPU, e.g. `tcp_retransmit_skb`), `sched` for CPU
contention. Everything else (`btrfs`, `kvm`, `thermal`, …) is irrelevant to the
validator's vote-credit story.
