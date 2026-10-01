//! The CLI ↔ daemon wire protocol (DESIGN.md §5.1): newline-delimited JSON,
//! one request → one reply, over the §7.3 peer-cred-authenticated socket.
//! Boring on purpose. No streaming — `cued logs` reads files directly.

use std::path::PathBuf;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::model::{
    Approval, ExpiryReason, Job, JobId, JobSource, JobSpec, JobStatus, RunId, RunStatus, StepId,
};

/// Bumped on incompatible changes; the daemon rejects mismatches with
/// `Response::ProtoMismatch` so the CLI can print the fix (§5.1).
///
/// **Still 1, and stays 1 until there is a released version whose wire
/// format we intend to keep speaking.** Pre-release every change is
/// breaking, so incrementing on each one would say nothing — the number
/// would climb while meaning exactly as much as it does now. A stale daemon
/// during development is a dev-loop problem with a dev-loop fix (restart
/// it), not a compatibility boundary worth recording forever.
///
/// The handshake itself stays wired up, so the mechanism is ready the day
/// there is something to be compatible *with*.
pub const PROTO_VERSION: u32 = 1;

/// The longest an upgrade may drain (`RequestBody::Upgrade::wait_secs`).
/// Enforced by the daemon; the CLI checks it too, to say so politely.
pub const MAX_UPGRADE_WAIT_SECS: u64 = 24 * 3600;

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub proto: u32,
    pub body: RequestBody,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "cmd")]
pub enum RequestBody {
    /// Liveness / version check (used by auto-spawn's connect retry, §5.2).
    Ping,
    /// A validated-shape job definition; the daemon runs the §6.3 gate and
    /// assigns the id. Boxed: a graph dwarfs every other request.
    Submit { spec: Box<JobSpec> },
    /// Separate verb: stale daemons must reject rather than ignore a gate.
    SubmitDefinition {
        spec: Box<JobSpec>,
        source: JobSource,
        require_approval: bool,
    },
    /// Hash of the exact definition reviewed before interactive confirmation.
    Approve {
        job: String,
        definition_hash: [u8; 32],
    },
    /// The §10.3 view. Default scope: live jobs + runs ended in the last
    /// 24h (Held always shown); `all` = everything retained.
    List { all: bool },
    /// §3.4: resume a Held run from where it parked. `job` is an id or a
    /// live job's name (§2), like every job reference.
    Continue { job: String },
    /// §3.4: re-run a terminal or Held run, rewinding it in place; `from`
    /// overrides the restart step.
    Retry { job: String, from: Option<String> },
    /// §4.2: stop re-arming; in-flight runs continue.
    Pause { job: String },
    /// §4.2: re-arm to the next future instant (never back-fills).
    Resume { job: String },
    /// §4.2: stop re-arming for good and terminate any live run (§2.2).
    Cancel { job: String },
    /// §6: the job's canonical definition, for inspection and TOML export.
    Show { job: String },
    /// §10.2: enforce retention now rather than waiting for the daily tick.
    Gc,
    /// §2.1: which attempts a run has, and how they went. Deliberately not
    /// the output itself — see `LogAttempt`.
    Logs {
        job: String,
        run: Option<i64>,
        step: Option<String>,
        attempt: Option<u32>,
    },
    /// §5.2 upgrade: finish the running steps, then re-exec the binary this
    /// daemon was started from, keeping its PID, locks and listening socket.
    ///
    /// **Frozen shape, exempt from the proto check.** A new CLI has to be
    /// able to ask an old daemon to become new, so this request (and its
    /// three replies) must keep decoding across every future proto bump.
    /// Never rename or reshape it; add a new verb instead.
    Upgrade {
        /// How long to let running steps finish before giving up.
        wait_secs: u64,
        /// On timeout, interrupt what is still running (§3.4 Case 2)
        /// instead of abandoning the upgrade.
        force: bool,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "result")]
pub enum Response {
    Pong {
        proto: u32,
    },
    /// `run` is None for a recurring job — its runs are created firing by
    /// firing (§4.2).
    Submitted {
        job: JobId,
        run: Option<RunId>,
        pending_approval: bool,
    },
    Approved {
        job: JobId,
    },
    JobList {
        jobs: Vec<JobEntry>,
    },
    /// continue/retry accepted: the run is back on the heap at this step.
    Rearmed {
        job: JobId,
        run: RunId,
        step: String,
    },
    Paused {
        job: JobId,
    },
    /// `next_at` is None when nothing remains to fire (exhausted schedule,
    /// or a one-off whose timing lives on its run).
    Resumed {
        job: JobId,
        next_at: Option<Timestamp>,
    },
    /// §10.2: what the sweep removed.
    Collected {
        runs: u32,
        jobs: u32,
    },
    /// One job's whole definition (§6). The graph, schedule, env and
    /// policies are already one document in the store (§5.3), so this is
    /// that document plus the two scheduling columns that live beside it.
    JobDetail {
        job: Box<Job>,
        next_fire_at: Option<Timestamp>,
        queued_at: Option<Timestamp>,
    },
    /// The attempts of one run, oldest first — the manifest `cued logs`
    /// renders. Execution is strictly sequential (§3), so this order *is*
    /// chronological order.
    LogManifest {
        job: JobId,
        run: RunId,
        attempts: Vec<LogAttempt>,
    },
    /// The job is cancelled; `runs` are the runs that were live and have
    /// been marked `Cancelled` (their processes, if any, are being torn
    /// down per §2.2 — the reply doesn't wait out `kill_grace`).
    JobCancelled {
        job: JobId,
        runs: Vec<RunId>,
    },
    /// "daemon (proto X) is older/newer than this CLI (proto Y) — run
    /// stop the running `cued daemon` and rerun" (§5.1).
    ProtoMismatch {
        daemon_proto: u32,
    },
    /// Upgrade (frozen, see `RequestBody::Upgrade`): drained; the daemon is
    /// re-executing `exe` as this reply is written.
    Upgrading {
        exe: PathBuf,
    },
    /// Upgrade: `exe` is already the image this daemon is running.
    UpgradeCurrent {
        exe: PathBuf,
    },
    /// Upgrade: nothing was replaced and the daemon carries on as it was.
    UpgradeAbandoned {
        reason: String,
    },
    Error {
        message: String,
    },
}

/// One `cued list` row: the job, when it next fires (or was scheduled for),
/// and where its latest run stands.
#[derive(Debug, Serialize, Deserialize)]
pub struct JobEntry {
    pub id: JobId,
    pub name: Option<String>,
    pub status: JobStatus,
    pub approval: Option<Approval>,
    pub source: JobSource,
    pub expired_at: Option<Timestamp>,
    pub expiry_reason: Option<ExpiryReason>,
    /// Human summary of the entry step's action ("./backup.sh --full").
    pub action: String,
    /// The next relevant instant: a live run's frozen wait target, else the
    /// job's re-arm target (§4.2).
    pub next_at: Option<Timestamp>,
    pub last_run: Option<RunEntry>,
}

/// One attempt's log metadata (§2.1). The captured bytes are pointedly NOT
/// here: §5.1 keeps the socket control-plane only and has `cued logs` read
/// the per-attempt file directly — they're the user's own files, and it
/// makes `-f` a plain tail instead of a streaming protocol.
#[derive(Debug, Serialize, Deserialize)]
pub struct LogAttempt {
    pub step: StepId,
    pub attempt: u32,
    pub started_at: Timestamp,
    /// When the attempt stopped. `None` does **not** mean "still going":
    /// §3.4 deliberately leaves a killed attempt's row open, because its
    /// true fate is unknown and the record should say so. Liveness is
    /// `running`, below.
    pub ended_at: Option<Timestamp>,
    /// Whether this attempt is executing *now*, taken from the run's cursor
    /// rather than inferred from `ended_at`. The cursor is what the daemon
    /// acts on, so it is the only thing that can distinguish "still going"
    /// from "was killed and never resumed". This is the attempt `-f`
    /// follows, and there is at most one: execution is sequential (§3).
    pub running: bool,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RunEntry {
    pub id: RunId,
    pub status: RunStatus,
    pub ended_at: Option<Timestamp>,
    /// §3.2: `deadline` | `max_visits` | … — why it failed, when the answer
    /// isn't "the command did". Without this the store records a reason
    /// nothing ever shows.
    pub fail_reason: Option<String>,
}
