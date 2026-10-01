//! The canonical model: Jobs, Runs, Steps, Transitions, Schedules.
//!
//! Everything a front-end produces (CLI sugar, TOML files) desugars into
//! these types (DESIGN.md §6); the store serializes them (§5.3: the graph,
//! schedule, env, and policies are JSON documents on the `jobs` row, while
//! runs / step attempts are relational). Definitions here mirror
//! DESIGN.md §2–§4 — when the two disagree, the design doc wins.
//!
//! **Every instant here is a `Timestamp` — UTC, no zone attached.** A zone is
//! a presentation and authoring concern, handled at the edges (§9): input is
//! resolved against the submitter's zone, or one they name, and output is
//! rendered back into the reader's. Carrying `Zoned` inside meant every
//! instant dragged a zone that was usually decorative and occasionally wrong
//! — `Every`'s anchor kept one "purely for display", and timestamps read
//! back from the store were stamped with whatever zone the *daemon* happened
//! to be in. A bare `Timestamp` cannot be rendered without naming a zone,
//! which puts the conversion where it can be seen.
//!
//! The exception is `Calendar`, whose zone is not decoration but meaning:
//! "every day at 09:00 in New York" is a rule, not an instant, and moves
//! relative to UTC across a DST transition.

use std::collections::BTreeMap;

use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Identifiers (§5.3: sequential, user-facing — rendered "j7" / "j7.r3")
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct JobId(pub i64);

/// Per-job run sequence number; a run is addressed as (JobId, RunId).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RunId(pub i64);

/// User-given step label, unique within a job (§3.1).
pub type StepId = String;

impl std::fmt::Display for JobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "j{}", self.0)
    }
}

impl std::fmt::Display for RunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "r{}", self.0)
    }
}

// ---------------------------------------------------------------------------
// Job — the definition (§2, §3.1)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: JobId,
    /// Human label; unique among live jobs; accepted anywhere an id is (§2).
    pub name: Option<String>,
    pub schedule: Schedule,
    pub status: JobStatus,
    // Required on the wire: an old daemon must never look ungated by default.
    pub approval: Option<Approval>,
    pub source: JobSource,
    pub expired_at: Option<Timestamp>,
    pub expiry_reason: Option<ExpiryReason>,
    pub graph: Graph,
    /// Captured at submit time (§2.1); secrets policy in §7.5.
    pub cwd: String,
    pub env: CapturedEnv,
    pub policies: Policies,
    pub hooks: Hooks,
    pub created_at: Timestamp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Active,
    Paused,
    Done,
    Cancelled,
    Expired,
}

impl JobStatus {
    /// Live = non-terminal (§2): the jobs `cued list` shows by default and
    /// the scope of the name-uniqueness rule.
    pub fn is_live(self) -> bool {
        matches!(self, Self::Active | Self::Paused)
    }

    /// Read back what `as_str` wrote. Keep the two in step.
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "active" => Self::Active,
            "paused" => Self::Paused,
            "done" => Self::Done,
            "cancelled" => Self::Cancelled,
            "expired" => Self::Expired,
            _ => return None,
        })
    }

    /// The stored and displayed spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Done => "done",
            Self::Cancelled => "cancelled",
            Self::Expired => "expired",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalState {
    Pending,
    Approved,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Approval {
    pub state: ApprovalState,
    pub definition_hash: [u8; 32],
    pub approved_at: Option<Timestamp>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobSource {
    #[default]
    Cli,
    Mcp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExpiryReason {
    ScheduledAtPassed,
    ApprovalTtlElapsed,
}

pub const PENDING_APPROVAL_TTL: SignedDuration = SignedDuration::from_hours(7 * 24);

impl Job {
    pub fn definition(&self) -> JobSpec {
        JobSpec {
            name: self.name.clone(),
            schedule: self.schedule.clone(),
            graph: self.graph.clone(),
            cwd: self.cwd.clone(),
            env: self.env.clone(),
            policies: self.policies.clone(),
            hooks: self.hooks.clone(),
        }
    }

    pub fn approval_valid(&self) -> bool {
        self.approval.as_ref().is_none_or(|a| {
            a.state == ApprovalState::Approved
                && self
                    .definition()
                    .definition_hash()
                    .is_ok_and(|hash| hash == a.definition_hash)
        })
    }

    pub fn can_start(&self) -> bool {
        self.status == JobStatus::Active && self.approval_valid()
    }

    pub fn approval_deadline(&self) -> anyhow::Result<(Timestamp, ExpiryReason)> {
        Ok(match self.schedule {
            Schedule::Once { at } => (at, ExpiryReason::ScheduledAtPassed),
            _ => (
                self.created_at.checked_add(PENDING_APPROVAL_TTL)?,
                ExpiryReason::ApprovalTtlElapsed,
            ),
        })
    }

    pub fn display_status(&self) -> String {
        display_status(self.status, self.approval.as_ref())
    }

    /// Steps whose action is a notification: they spawn no process, so an
    /// attempt with no exit code means "notified", not "killed" (§2.2).
    pub fn notify_steps(&self) -> std::collections::BTreeSet<StepId> {
        self.graph
            .steps
            .iter()
            .filter(|(_, step)| matches!(step.action, Action::Notify { .. }))
            .map(|(id, _)| id.clone())
            .collect()
    }
}

pub fn display_status(status: JobStatus, approval: Option<&Approval>) -> String {
    let lifecycle = status.as_str().to_string();
    match approval.map(|a| a.state) {
        Some(ApprovalState::Pending) if status.is_live() => {
            format!("Pending approval ({lifecycle})")
        }
        Some(ApprovalState::Approved) => format!("{lifecycle} (approved)"),
        _ => lifecycle,
    }
}

/// A job as a front-end submits it (§6): everything but the identity and
/// lifecycle fields the daemon assigns at accept time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSpec {
    pub name: Option<String>,
    pub schedule: Schedule,
    pub graph: Graph,
    pub cwd: String,
    pub env: CapturedEnv,
    pub policies: Policies,
    pub hooks: Hooks,
}

impl JobSpec {
    /// Versioned, deterministic canonical JSON: object keys sorted recursively by
    /// serde_json's BTreeMap representation; arrays retain semantic order.
    pub fn definition_hash(&self) -> anyhow::Result<[u8; 32]> {
        use sha2::{Digest, Sha256};
        let canonical = serde_json::to_value(self)?;
        let mut digest = Sha256::new();
        digest.update(b"cued-definition-v1\0");
        digest.update(serde_json::to_vec(&canonical)?);
        Ok(digest.finalize().into())
    }
}

/// The step graph: stored as one JSON document on the job row (§5.3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Graph {
    /// Where each run begins.
    pub entry: StepId,
    pub steps: BTreeMap<StepId, Step>,
}

/// Environment captured at submit, with denylist-stripped names recorded so
/// `cued show` can answer "why did the 3am run miss $DEPLOY_TOKEN?" (§7.5).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CapturedEnv {
    pub vars: BTreeMap<String, String>,
    pub stripped: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Policies {
    /// §3.4 Case 1: an overdue wait between steps.
    pub missed_wait: MissedWait,
    /// §3.4 Case 2: a step killed mid-run.
    pub on_interrupt: OnInterrupt,
    /// §4.2: firings the schedule missed entirely (machine off).
    pub catch_up: CatchUp,
    /// §4.2: a firing arriving while the previous run is live.
    pub overlap: Overlap,
    /// Per-run wall-clock cap from run start; loop safety (§3.2).
    pub deadline: Option<SignedDuration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissedWait {
    #[default]
    RunAsap,
    Abandon,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnInterrupt {
    #[default]
    Hold,
    Fail,
    Retry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatchUp {
    #[default]
    RunOnce,
    Skip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Overlap {
    #[default]
    Skip,
    Queue,
}

/// Lifecycle hooks are Notify-only (§3.5). `on_hold` is default ON with an
/// auto-generated message (`None` here = use the auto message).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Hooks {
    pub on_hold: Option<NotifySpec>,
    pub on_failure: Option<NotifySpec>,
    pub on_success: Option<NotifySpec>,
    pub on_missed: Option<NotifySpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotifySpec {
    pub title: String,
    pub body: String,
}

/// What a notification server acknowledged for one display (§3.5). The ID is
/// only meaningful within the server lifetime named by `server` (its unique
/// bus name): a restarted server hands out the same IDs again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryReceipt {
    pub server: String,
    pub id: u32,
}

// ---------------------------------------------------------------------------
// Schedule (§4.1)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Schedule {
    /// The §2 one-off.
    Once { at: Timestamp },
    /// Fixed cadence in elapsed real time: anchor, anchor+i, anchor+2i, …
    /// DST-blind by design; civil-time cadences belong to `Calendar`.
    Every {
        interval: SignedDuration,
        anchor: Timestamp,
        #[serde(default)]
        until: Option<Timestamp>,
        #[serde(default)]
        count: Option<u32>,
    },
    /// Civil wall-clock rule ("every day 09:00") — zone rules in §9.
    Calendar {
        spec: CalendarSpec,
        /// The submitting user's IANA zone (§9): "every day 9am" means 9am
        /// on THIS wall clock, across DST, forever.
        zone: String,
        #[serde(default)]
        until: Option<Timestamp>,
        #[serde(default)]
        count: Option<u32>,
    },
}

impl Schedule {
    pub fn is_recurring(&self) -> bool {
        matches!(self, Self::Every { .. } | Self::Calendar { .. })
    }

    /// The optional §4.1 firing cap ("…or after this many firings").
    pub fn count(&self) -> Option<u32> {
        match self {
            Self::Once { .. } => None,
            Self::Every { count, .. } | Self::Calendar { count, .. } => *count,
        }
    }
}

/// The v1 calendar tier: daily / weekly / monthly-by-date (§9.2).
/// Nth-weekday and cron are deferred, additive extensions (§12).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CalendarSpec {
    /// "every day 09:00"
    Daily { at: jiff::civil::Time },
    /// "every mon,wed,fri 9am" (with `weekdays` / `weekends` sugar)
    Weekly {
        days: Vec<Weekday>,
        at: jiff::civil::Time,
    },
    /// "every month on 1,15 at 9am"; short months clamp (§9.2).
    Monthly {
        days: Vec<MonthDay>,
        at: jiff::civil::Time,
    },
}

/// Ours rather than jiff's so the serde form (part of the graph JSON / TOML
/// surface) is under our control; convert to `jiff::civil::Weekday` at the
/// point of computation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Weekday {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
    Sun,
}

impl From<Weekday> for jiff::civil::Weekday {
    fn from(day: Weekday) -> Self {
        match day {
            Weekday::Mon => Self::Monday,
            Weekday::Tue => Self::Tuesday,
            Weekday::Wed => Self::Wednesday,
            Weekday::Thu => Self::Thursday,
            Weekday::Fri => Self::Friday,
            Weekday::Sat => Self::Saturday,
            Weekday::Sun => Self::Sunday,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MonthDay {
    /// 1–31; fires on the last day of months that lack it (clamp, §9.2).
    Day(u8),
    /// "on last" — the honest spelling of end-of-month.
    Last,
}

// ---------------------------------------------------------------------------
// Steps, transitions, waits (§3.1)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub action: Action,
    /// Optional per-step overrides; else inherit from the job (§3.1).
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: Option<BTreeMap<String, String>>,
    /// Step-level wall-clock cap; expiry = TERM → kill_grace → KILL (§2.2).
    #[serde(default)]
    pub timeout: Option<SignedDuration>,
    #[serde(default)]
    pub kill_grace: Option<SignedDuration>,
    /// Evaluated top-to-bottom, FIRST match wins (§3.2).
    #[serde(default)]
    pub transitions: Vec<Transition>,
    /// Loop bound if the step is re-entered; per run (§3.1).
    #[serde(default)]
    pub max_visits: Option<u32>,
    /// Opt-in: permits automatic Retry on interrupt (§3.4 Case 2).
    #[serde(default)]
    pub restart_safe: bool,
    /// Per-step override of the job's §3.4 Case-1 policy ("this one step is
    /// time-critical"); None = inherit.
    #[serde(default)]
    pub missed_wait: Option<MissedWait>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// The chore. argv is canonical; single-string front-ends desugar to
    /// ["/bin/sh", "-c", s] (§2.2).
    Shell { argv: Vec<String> },
    /// The reminder / nudge; "succeeds" once durably enqueued (§3.5).
    Notify { title: String, body: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transition {
    pub when: Condition,
    pub then: Effect,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Condition {
    /// Catch-all / fallthrough.
    Always,
    ExitEq(i32),
    ExitNe(i32),
    ExitIn(Vec<i32>),
    Stdout(OutputMatch),
    Stderr(OutputMatch),
    /// The step hit its timeout.
    TimedOut,
    /// Generic; the only conditions meaningful for a Notify step (§3.1).
    Succeeded,
    Failed,
    /// AND *within* one edge; OR is multiple edges. The deliberate ceiling —
    /// no full boolean expression tree (§3.2).
    All(Vec<Condition>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputMatch {
    Contains(String),
    Regex(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    Goto {
        step: StepId,
        #[serde(default)]
        after: Option<Wait>,
    },
    /// Terminate the whole run.
    End { outcome: Outcome },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Success,
    Failure,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Wait {
    In(SignedDuration),
    Until(Timestamp),
    /// Growing delays: start × factor^(v−1) capped at max, where v is the
    /// target step's visit count — the same counter max_visits uses (§3.2).
    Backoff {
        start: SignedDuration,
        factor: f64,
        max: SignedDuration,
    },
}

// ---------------------------------------------------------------------------
// Run — one execution, carrying all runtime state (§2, §3.3)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: RunId,
    pub job_id: JobId,
    /// The firing instant this run represents.
    pub scheduled_for: Timestamp,
    pub started_at: Option<Timestamp>,
    pub ended_at: Option<Timestamp>,
    pub status: RunStatus,
    pub cursor: Cursor,
    /// deadline | max_visits | … — set when status is Failed (§3.2).
    pub fail_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Pending,
    Running,
    Waiting,
    Held,
    Done,
    Failed,
    Missed,
    /// Overlap- or catch-up-skipped firing; one row can cover a coalesced
    /// range (§4.2, §5.3).
    Skipped,
    Cancelled,
}

impl RunStatus {
    /// Final as far as a waiter is concerned: Done, Failed and the rest
    /// are over, and Held parks until a human (or agent) acts.
    pub fn is_settled(self) -> bool {
        !matches!(self, Self::Pending | Self::Running | Self::Waiting)
    }

    /// Read back what `as_str` wrote. Keep the two in step.
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "pending" => Self::Pending,
            "running" => Self::Running,
            "waiting" => Self::Waiting,
            "held" => Self::Held,
            "done" => Self::Done,
            "failed" => Self::Failed,
            "missed" => Self::Missed,
            "skipped" => Self::Skipped,
            "cancelled" => Self::Cancelled,
            _ => return None,
        })
    }

    /// The stored and displayed spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Held => "held",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Missed => "missed",
            Self::Skipped => "skipped",
            Self::Cancelled => "cancelled",
        }
    }
}

/// PERSISTED runtime position — flattened into columns on `runs` so
/// reconciliation is a query over cursor state (§3.3, §5.3).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cursor {
    /// Between steps; safe to resume eagerly (§3.4 Case 1).
    Waiting {
        step: StepId,
        at: Timestamp,
    },
    /// Found at startup = killed mid-run; side effects unknown (§3.4 Case 2).
    Running {
        step: StepId,
        started_at: Timestamp,
    },
    /// Awaiting manual `cued continue` / `cued retry`.
    Held {
        step: StepId,
        reason: HeldReason,
    },
    Done,
}

/// Why a run is parked. §3.3 kept this an enum so park-reasons could be
/// added without overloading a bool — this is that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeldReason {
    /// The step's process was killed by a daemon/machine shutdown (§3.4
    /// Case 2).
    Interrupted,
    /// The daemon kept failing to run the step — a store error, say — and
    /// exhausted its retry budget (§3.3). Same unknown-side-effects shape as
    /// `Interrupted`: whether anything ran is exactly what we can't tell, so
    /// it parks the same way rather than guessing.
    Errored,
}

impl HeldReason {
    /// The phrase the auto-generated on_hold message uses (§3.5).
    pub fn describe(self) -> &'static str {
        match self {
            Self::Interrupted => "machine or daemon restarted",
            Self::Errored => "the daemon repeatedly failed to run it",
        }
    }
}

/// One row per attempt — audit, `cued list`/`logs` timing, and the anchor
/// that freezes the next relative delay into an absolute target (§3.3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepRun {
    pub job_id: JobId,
    pub run_id: RunId,
    pub step_id: StepId,
    pub attempt: u32,
    pub started_at: Timestamp,
    pub ended_at: Option<Timestamp>,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// Index of the transition edge that matched, if any.
    pub outcome_edge: Option<u32>,
}
