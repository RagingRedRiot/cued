//! The store: SQLite via sqlx (DESIGN.md §5.3).
//!
//! Definitions are documents (graph/schedule/env/policies as JSON columns on
//! `jobs`); runtime state is relational (`runs`, `step_runs`,
//! `notifications`) with the cursor flattened into queryable columns. The
//! min-heap is *derived* from this store at startup — the store is the single
//! source of truth. Migrations are embedded and run only by the daemon,
//! before the socket opens (§5.3); the write-ordering invariant that all
//! writes must respect is §3.3: `begin_step` is its step 1 (persist intent
//! before spawning), `finish_step` its step 2 (persist outcome before
//! re-arming) — each a single transaction.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};

use crate::config::Retention;
use crate::model::{
    Approval, ApprovalState, CatchUp, DeliveryReceipt, ExpiryReason, Graph, HeldReason, Hooks, Job,
    JobId, JobSource, JobSpec, JobStatus, NotifySpec, Policies, RunId, RunStatus, Schedule, StepId,
};
use crate::proto::{JobRun, LogAttempt, RunEntry, RunQuery};

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// jobs.policies is one JSON document holding both the policy knobs and the
/// §3.5 hooks — they travel together everywhere a definition does.
#[derive(Serialize, Deserialize)]
struct PoliciesDoc {
    policies: Policies,
    hooks: Hooks,
}

/// A step due (or due later) to run: what the daemon's heap holds (§5.2).
#[derive(Debug, Clone)]
pub struct DueStep {
    pub job: JobId,
    pub run: RunId,
    pub step: StepId,
    pub at: Timestamp,
}

/// Where the cursor lands when a step closes (§3.3 step 2).
#[derive(Debug, Clone)]
pub enum NextCursor {
    /// A sleep-edge was taken: the run stays live, frozen absolute target.
    Waiting { step: StepId, at: Timestamp },
    /// The run is over: Done or Failed (Held arrives with reconciliation).
    Terminal {
        status: RunStatus,
        fail_reason: Option<String>,
    },
}

/// Which claim a write made *after* `begin_step` belongs to: the step that
/// was claimed and the attempt number the claim returned (§3.3 step 1).
///
/// A cursor reading `running` on some step is not proof that this attempt
/// still owns it. `cued retry` rewinds a run **in place** — same run id,
/// same step, new epoch and a new attempt (§3.4) — so a write held up
/// behind a failing store can come back to a cursor that now belongs to a
/// newer, live attempt. Attempt numbers only ever go up (they keep counting
/// across manual retries), so "this attempt is the step's newest" is the
/// generation check that tells the two apart.
#[derive(Debug, Clone, Copy)]
pub struct Claim<'a> {
    pub step: &'a str,
    pub attempt: u32,
}

/// The `runs` predicate for "this claim still owns the cursor". Binds, in
/// order: step, job, run, step, attempt. Correlated on `runs` so it can be
/// appended to an UPDATE of the row itself.
const OWNS_CURSOR: &str = "AND cursor_kind = 'running' AND cursor_step = ?
             AND (SELECT MAX(attempt) FROM step_runs
                  WHERE job_id = ? AND run_id = ? AND step_id = ?) = ?";

/// What closing a step did: whether it moved the run on at all, and what the
/// §4.2 queue released in the same commit if it ended one.
#[derive(Debug)]
pub struct StepClosed {
    pub advanced: bool,
    pub drained: Option<(RunId, Timestamp)>,
}

/// Everything §3.3 step 2 commits in one transaction.
pub struct StepClose<'a> {
    pub job: JobId,
    /// Where a run of this job starts — needed only if this close ends the
    /// run and the §4.2 queue has a firing to release into a new one.
    pub entry_step: &'a str,
    pub run: RunId,
    pub step: &'a str,
    pub attempt: u32,
    pub ended_at: &'a Timestamp,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// Index of the transition edge that matched; None = derived End (§3.2).
    pub outcome_edge: Option<u32>,
    pub next: NextCursor,
    /// Durable enqueues riding the same transaction (§3.2, §3.5): a Notify
    /// step's payload and/or terminal lifecycle hooks — so "the step closed"
    /// and "the notifications exist" are one fact.
    pub notifications: Vec<NotifySpec>,
}

/// One undelivered row from the §3.5 queue.
#[derive(Debug, Clone)]
pub struct PendingNotification {
    pub id: i64,
    pub spec: NotifySpec,
}

/// One §4.2 firing, as `record_firing` commits it. The daemon composes
/// this from the catch_up and overlap decisions; the store just makes the
/// combination atomic.
#[derive(Debug)]
pub struct Firing<'a> {
    pub job: JobId,
    pub entry_step: &'a str,
    /// The firing instant this is *claiming* — the job's stored
    /// `next_fire_at` must still equal it, or the firing isn't ours (§4.2).
    pub claiming: &'a Timestamp,
    /// How many §4.1 firings this consumes: every due instant it covers,
    /// whether that instant runs, is skipped, or coalesces into the queue
    /// slot. All of them fired.
    pub consumed: u32,
    /// Create the Run representing this instant ("each firing creates a
    /// Run"). None = this firing produced no runnable work (skip/queue).
    pub run_at: Option<&'a Timestamp>,
    /// One row covering a coalesced range of skipped instants:
    /// (earliest, latest, how many). Earliest None = a single instant.
    pub skipped: Option<(Option<&'a Timestamp>, &'a Timestamp, u32)>,
    /// §4.2 Queue: coalesce this instant into the job's single pending slot.
    pub queue_at: Option<&'a Timestamp>,
    /// The §4.2 re-arm target. None = schedule exhausted (until/count).
    pub next_fire_at: Option<&'a Timestamp>,
    pub now: &'a Timestamp,
}

/// One `cued list` row's worth of a job (§10.3), pre-join with its latest run.
#[derive(Debug)]
pub struct JobOverview {
    pub id: JobId,
    pub name: Option<String>,
    pub status: JobStatus,
    pub approval: Option<Approval>,
    pub source: JobSource,
    pub expired_at: Option<Timestamp>,
    pub expiry_reason: Option<ExpiryReason>,
    pub graph: Graph,
    pub next_fire_at: Option<Timestamp>,
    pub last_run: Option<RunOverview>,
}

#[derive(Debug)]
pub struct RunOverview {
    pub id: RunId,
    pub status: RunStatus,
    pub scheduled_for: Timestamp,
    pub ended_at: Option<Timestamp>,
    /// The frozen wait target while the cursor is Waiting.
    pub cursor_at: Option<Timestamp>,
    pub fail_reason: Option<String>,
}

/// An Active job with nothing left to do: nothing armed or queued, not
/// awaiting approval, and no run still in flight. The one rule for "this
/// job is done" — `finish_exhausted_jobs` acts on it, and `job_run` reports
/// it before that sweep comes round. Expects `jobs` in scope.
const EXHAUSTED: &str = "jobs.status = 'active'
    AND jobs.next_fire_at IS NULL AND jobs.queued_at IS NULL
    AND (jobs.approval IS NULL OR json_extract(jobs.approval, '$.state') = 'approved')
    AND NOT EXISTS (SELECT 1 FROM runs WHERE runs.job_id = jobs.id AND runs.cursor_kind != 'done')";

/// The result of trying to record a §4.2 firing.
#[derive(Debug, PartialEq, Eq)]
pub enum Fired {
    /// The firing was ours. `run` is the Run it created, if any, and
    /// `drained` the run the §4.2 queue released in the same commit.
    Recorded {
        run: Option<RunId>,
        drained: Option<(RunId, Timestamp)>,
    },
    /// The job moved on without us: cancelled, paused, or another heap
    /// entry for this same instant got there first. Nothing was written.
    Superseded,
}

/// What one §10.2 sweep removed. The run list is what the caller needs to
/// delete the matching per-attempt log directories (§2.1).
#[derive(Debug, Default)]
pub struct GcOutcome {
    pub runs: Vec<(JobId, RunId)>,
    pub jobs: Vec<JobId>,
}

/// How long SQLite's own busy handler waits for a lock before failing. With
/// one writer connection this should only ever cover a WAL checkpoint or a
/// reader catching the WAL mid-restart — milliseconds. Provisional (§12).
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a write waits in line for the one writer connection. The line
/// is the backpressure: a burst of fires queues here in FIFO order instead
/// of racing for SQLite's write lock. Generous, because every transaction in
/// this file is SQL-only — none holds the connection across a spawn or a
/// notification — so a long wait means a genuinely stuck store.
/// Provisional (§12).
const WRITE_QUEUE_TIMEOUT: Duration = Duration::from_secs(30);

/// How many runs one §10.2 GC transaction examines. Each batch holds the
/// single writer, so this bounds how long a firing can wait behind a sweep
/// — and how much of the history is ever in memory at once. Provisional
/// (§12).
const GC_BATCH: i64 = 256;

/// Enough concurrent reads for `list`/`logs`/`show` alongside the daemon's
/// own hot-path loads. Provisional (§12).
const READERS: u32 = 4;

/// Two pools over one file. SQLite admits a single writer at a time, and a
/// deferred transaction that reads before it writes can't wait for that
/// lock — it fails at once with `SQLITE_BUSY` (or `BUSY_SNAPSHOT` if another
/// writer committed since its read). Several write connections therefore
/// turned a burst of simultaneous fires into a burst of errors. So every
/// write goes through `writer`, a pool of exactly one connection, whose
/// acquire queue is the write queue; reads use `reader`, which WAL lets run
/// alongside the writer without blocking it or being blocked.
///
/// Invariant that keeps the single writer safe: no method holds a write
/// transaction across a call back into the store, or across anything that
/// isn't SQL.
#[derive(Debug, Clone)]
pub struct Store {
    writer: SqlitePool,
    reader: SqlitePool,
}

impl Store {
    /// Open (creating if missing) and migrate. WAL for concurrent readers —
    /// the daemon is the sole writer, enforced by the §5.2 flock, which the
    /// caller must hold before calling this.
    pub async fn open(db_file: &Path) -> Result<Self> {
        let options = SqliteConnectOptions::new()
            .filename(db_file)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(BUSY_TIMEOUT)
            .foreign_keys(true);

        let writer = SqlitePoolOptions::new()
            .max_connections(1)
            .acquire_timeout(WRITE_QUEUE_TIMEOUT)
            .connect_with(options.clone().create_if_missing(true))
            .await
            .with_context(|| format!("opening store at {}", db_file.display()))?;

        // §7.5: the captured env lives in here — 0600, belt to the data
        // dir's 0700 suspenders.
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(db_file, perms)
            .with_context(|| format!("restricting permissions on {}", db_file.display()))?;

        MIGRATOR.run(&writer).await.context("running migrations")?;

        // Opened after the migrations, so no reader ever sees a half-built
        // schema. Read-only, so a write routed here by mistake fails loudly
        // instead of quietly racing the writer.
        let reader = SqlitePoolOptions::new()
            .max_connections(READERS)
            .connect_with(options.read_only(true))
            .await
            .with_context(|| format!("opening store readers at {}", db_file.display()))?;

        Ok(Self { writer, reader })
    }

    /// Return every connection and close the pools, so the last one out
    /// checkpoints the WAL — the store left tidy for an upgrade's exec.
    pub async fn close(&self) {
        self.reader.close().await;
        self.writer.close().await;
    }

    /// The writer, for tests that poke rows directly — it both reads and
    /// writes, and reads through it see everything committed.
    pub fn pool(&self) -> &SqlitePool {
        &self.writer
    }

    /// Accept a validated submission. A one-off gets its single run at
    /// once, parked on the virtual first sleep-edge (§3.2: cursor
    /// Waiting { entry, at }); a recurring job gets `next_fire_at` instead —
    /// its runs are created firing by firing (§4.2). One transaction either
    /// way. Returns the created run (None for recurring) and the first
    /// firing instant.
    pub async fn submit_job(
        &self,
        spec: &JobSpec,
        now: &Timestamp,
    ) -> Result<(JobId, Option<RunId>, Timestamp)> {
        self.submit_definition(spec, now, JobSource::Cli, false)
            .await
    }

    pub async fn submit_definition(
        &self,
        spec: &JobSpec,
        now: &Timestamp,
        source: JobSource,
        require_approval: bool,
    ) -> Result<(JobId, Option<RunId>, Timestamp)> {
        crate::submit::validate(spec)?;
        if require_approval && let Schedule::Once { at } = spec.schedule {
            ensure!(
                *now < at,
                "approval deadline has passed — reschedule the job"
            );
        }
        let approval = require_approval
            .then(|| -> Result<Approval> {
                Ok(Approval {
                    state: ApprovalState::Pending,
                    definition_hash: spec.definition_hash()?,
                    approved_at: None,
                })
            })
            .transpose()?;
        let first_fire = match &spec.schedule {
            Schedule::Once { at } => *at,
            recurring => crate::schedule::next_fire(recurring, now)?
                .context("schedule has no future firing")?,
        };

        let policies_doc = PoliciesDoc {
            policies: spec.policies.clone(),
            hooks: spec.hooks.clone(),
        };

        // §4.2: recurring cadence lives on the job (re-armed at fire time);
        // a one-off's single instant lives on its run's cursor instead.
        let next_fire_at =
            (spec.schedule.is_recurring() || require_approval).then(|| to_ts(&first_fire));

        let mut tx = self.writer.begin().await?;
        // A pending job past its deadline has already expired and released its
        // name (§7.6), whether or not a sweep has materialized that yet.
        expire_all_in(&mut tx, now).await?;
        let inserted = sqlx::query(
            "INSERT INTO jobs (name, status, schedule, next_fire_at, graph, cwd, env, policies, created_at, approval, source)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(spec.name.as_deref())
        .bind(JobStatus::Active.as_str())
        .bind(serde_json::to_string(&spec.schedule)?)
        .bind(next_fire_at)
        .bind(serde_json::to_string(&spec.graph)?)
        .bind(&spec.cwd)
        .bind(serde_json::to_string(&spec.env)?)
        .bind(serde_json::to_string(&policies_doc)?)
        .bind(to_ts(now))
        .bind(approval.as_ref().map(serde_json::to_string).transpose()?)
        .bind(source_text(source))
        .execute(&mut *tx)
        .await;

        let inserted = match inserted {
            Ok(done) => done,
            // §2: name is unique among *live* jobs (partial index) — surface
            // the collision as the user-facing rule, not a constraint error.
            Err(sqlx::Error::Database(db)) if db.is_unique_violation() => {
                bail!(
                    "a live job named {:?} already exists — cancel it or pick another name",
                    spec.name.as_deref().unwrap_or("")
                )
            }
            Err(other) => return Err(other).context("inserting job"),
        };
        let job = JobId(inserted.last_insert_rowid());

        let run = if spec.schedule.is_recurring() || require_approval {
            None
        } else {
            let run = RunId(1);
            sqlx::query(
                "INSERT INTO runs (job_id, id, scheduled_for, status, cursor_kind, cursor_step, cursor_at)
                 VALUES (?, ?, ?, ?, 'waiting', ?, ?)",
            )
            .bind(job.0)
            .bind(run.0)
            .bind(to_ts(&first_fire))
            .bind(RunStatus::Pending.as_str())
            .bind(&spec.graph.entry)
            .bind(to_ts(&first_fire))
            .execute(&mut *tx)
            .await
            .context("inserting initial run")?;
            Some(run)
        };

        if require_approval {
            // Deliberately no command, env values, or user-provided name in this
            // automatic notification. The private review is `cued approve`.
            sqlx::query("INSERT INTO notifications (job_id, title, body, created_at) VALUES (?, ?, ?, ?)")
                .bind(job.0).bind("cued: Pending approval")
                .bind(format!("Job {job} scheduled for {first_fire}; review and approve with `cued approve {job}`. Approval binds stored fields, not script contents."))
                .bind(to_ts(now)).execute(&mut *tx).await?;
        }
        tx.commit().await.context("committing submit")?;
        Ok((job, run, first_fire))
    }

    pub async fn load_job(&self, id: JobId) -> Result<Job> {
        let row = sqlx::query("SELECT * FROM jobs WHERE id = ?")
            .bind(id.0)
            .fetch_optional(&self.reader)
            .await?
            .with_context(|| format!("no such job {id}"))?;
        decode_job(&row)
    }

    /// Check and materialize expiry under the same writer serialization as approval.
    pub async fn expire_pending(&self, now: &Timestamp) -> Result<()> {
        let mut tx = self.writer.begin().await?;
        expire_all_in(&mut tx, now).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Retire an exhausted active definition even if it never got a run (for
    /// example, catch_up=skip with an `until` that elapsed during approval).
    /// Paused jobs retain their lifecycle; pending definitions cannot end here.
    pub async fn finish_exhausted_jobs(&self) -> Result<()> {
        sqlx::query(&format!(
            "UPDATE jobs SET status = 'done' WHERE {EXHAUSTED}"
        ))
        .execute(&self.writer)
        .await?;
        Ok(())
    }

    /// Compare the exact reviewed definition inside the transaction; a late
    /// approval also durably expires the job before returning the rejection.
    pub async fn approve(
        &self,
        id: JobId,
        reviewed: [u8; 32],
        now: &Timestamp,
    ) -> Result<(Option<RunId>, Option<Timestamp>)> {
        self.approve_with_clock(id, reviewed, || *now).await
    }

    /// Read the daemon's clock after acquiring the writer, so time waiting in
    /// the write queue cannot extend the authorization window.
    pub async fn approve_with_clock(
        &self,
        id: JobId,
        reviewed: [u8; 32],
        clock: impl FnOnce() -> Timestamp,
    ) -> Result<(Option<RunId>, Option<Timestamp>)> {
        let mut tx = self.writer.begin().await?;
        let now = &clock();
        let job = load_job_in(&mut tx, id).await?;
        if expire_in(&mut tx, &job, now).await? {
            tx.commit().await?;
            bail!("job {id} expired — reschedule it");
        }
        ensure!(job.status.is_live(), "job {id} has ended — reschedule it");
        let approval = job
            .approval
            .as_ref()
            .context("job does not require approval")?;
        ensure!(
            approval.state == ApprovalState::Pending,
            "job is already approved"
        );
        let hash = job.definition().definition_hash()?;
        ensure!(
            hash == reviewed,
            "definition changed since review — run `cued approve {id}` again"
        );
        let approval = Approval {
            state: ApprovalState::Approved,
            definition_hash: hash,
            approved_at: Some(*now),
        };
        let mut run = None;
        let next = match &job.schedule {
            Schedule::Once { at } => {
                let existing: Option<i64> = sqlx::query_scalar(
                    "SELECT id FROM runs WHERE job_id = ? ORDER BY id DESC LIMIT 1",
                )
                .bind(id.0)
                .fetch_optional(&mut *tx)
                .await?;
                let id_run = RunId(existing.unwrap_or(1));
                if existing.is_none() {
                    sqlx::query("INSERT INTO runs (job_id, id, scheduled_for, status, cursor_kind, cursor_step, cursor_at) VALUES (?, ?, ?, 'pending', 'waiting', ?, ?)")
                    .bind(id.0).bind(id_run.0).bind(to_ts(at)).bind(&job.graph.entry).bind(to_ts(at))
                    .execute(&mut *tx).await?;
                }
                run = Some(id_run);
                Some(*at)
            }
            schedule => {
                let first: Option<String> =
                    sqlx::query_scalar("SELECT next_fire_at FROM jobs WHERE id = ?")
                        .bind(id.0)
                        .fetch_one(&mut *tx)
                        .await?;
                let first = first.as_deref().map(from_ts).transpose()?;
                match first {
                    Some(first) if first <= *now => match job.policies.catch_up {
                        CatchUp::RunOnce => {
                            Some(crate::schedule::due_instants(schedule, &first, now, None)?.0)
                        }
                        CatchUp::Skip => crate::schedule::next_fire(schedule, now)?,
                    },
                    first => first,
                }
            }
        };
        sqlx::query("UPDATE jobs SET approval = ?, next_fire_at = ? WHERE id = ?")
            .bind(serde_json::to_string(&approval)?)
            .bind(if run.is_some() {
                None
            } else {
                next.as_ref().map(to_ts)
            })
            .bind(id.0)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok((run, next))
    }

    /// Every run parked on a sleep-edge — what the daemon re-arms its heap
    /// from at startup (§5.3: the heap is derived state).
    pub async fn waiting_runs(&self) -> Result<Vec<DueStep>> {
        let rows = sqlx::query(
            "SELECT job_id, id, cursor_step, cursor_at FROM runs WHERE cursor_kind = 'waiting' AND EXISTS (SELECT 1 FROM jobs WHERE jobs.id = runs.job_id AND (approval IS NULL OR json_extract(approval, '$.state') = 'approved'))",
        )
        .fetch_all(&self.reader)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(DueStep {
                    job: JobId(row.get("job_id")),
                    run: RunId(row.get("id")),
                    step: row.get("cursor_step"),
                    at: from_ts(row.get("cursor_at"))?,
                })
            })
            .collect()
    }

    /// §3.3 step 1 — persist intent, then act: cursor → Running plus the new
    /// `StepRun` row, committed BEFORE the caller spawns anything. Returns
    /// the attempt number (appends across manual retries, §3.4) — or None
    /// when the run is no longer waiting on this step: the cursor check
    /// makes stale or duplicate heap entries harmless (never a double
    /// spawn), so re-arming never has to be exactly-once.
    pub async fn begin_step(
        &self,
        job: JobId,
        run: RunId,
        step: &str,
        now: &Timestamp,
    ) -> Result<Option<u32>> {
        self.begin_step_checked(job, run, step, now, None).await
    }

    pub async fn begin_step_checked(
        &self,
        job: JobId,
        run: RunId,
        step: &str,
        now: &Timestamp,
        expected_approval: Option<[u8; 32]>,
    ) -> Result<Option<u32>> {
        let mut tx = self.writer.begin().await?;
        let definition = load_job_in(&mut tx, job).await?;
        if !definition.can_start() || !matches_review(&definition, expected_approval) {
            return Ok(None);
        }
        let attempt: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(attempt), 0) + 1 FROM step_runs
             WHERE job_id = ? AND run_id = ? AND step_id = ?",
        )
        .bind(job.0)
        .bind(run.0)
        .bind(step)
        .fetch_one(&mut *tx)
        .await?;

        let claimed = sqlx::query(
            "UPDATE runs SET status = ?, cursor_kind = 'running', cursor_step = ?,
                    cursor_at = NULL, started_at = COALESCE(started_at, ?)
             WHERE job_id = ? AND id = ? AND cursor_kind = 'waiting' AND cursor_step = ?",
        )
        .bind(RunStatus::Running.as_str())
        .bind(step)
        .bind(to_ts(now))
        .bind(job.0)
        .bind(run.0)
        .bind(step)
        .execute(&mut *tx)
        .await?;
        if claimed.rows_affected() == 0 {
            return Ok(None);
        }

        sqlx::query(
            "INSERT INTO step_runs (job_id, run_id, step_id, attempt, started_at, epoch)
             VALUES (?, ?, ?, ?, ?,
                     (SELECT epoch FROM runs WHERE job_id = ? AND id = ?))",
        )
        .bind(job.0)
        .bind(run.0)
        .bind(step)
        .bind(attempt)
        .bind(to_ts(now))
        .bind(job.0)
        .bind(run.0)
        .execute(&mut *tx)
        .await?;

        tx.commit().await.context("committing begin_step")?;
        Ok(Some(attempt as u32))
    }

    /// §3.3 step 2 — the step's outcome, the chosen edge, the new cursor,
    /// and any Notify enqueue, in one transaction, committed BEFORE the
    /// caller re-arms the heap.
    ///
    /// Returns false when the run went terminal underneath the step — which
    /// is what `cued cancel` does to a live run (§4.2). The attempt's own row
    /// is still closed (it really did end, and the audit trail should say
    /// so), but the cursor is left alone and the hooks don't fire: a
    /// cancelled run must not be walked forward by the step it was cancelled
    /// during, and must not announce a success or failure nobody is waiting
    /// on. Same conditional-write discipline as `begin_step`'s claim, plus
    /// `Claim` ownership (`OWNS_CURSOR`): the cursor must still be `running`
    /// this step *for this attempt*. That makes a close that is retried
    /// after it already landed a no-op, and stops a close held up behind a
    /// failing store from ending a newer attempt of the same step — the
    /// run's own record. The attempt row above is written either way: that
    /// attempt really did run and really did end, whoever owns the cursor
    /// now.
    pub async fn finish_step(&self, close: StepClose<'_>) -> Result<StepClosed> {
        self.finish_step_checked(close, None).await
    }

    pub async fn finish_step_checked(
        &self,
        close: StepClose<'_>,
        expected_approval: Option<[u8; 32]>,
    ) -> Result<StepClosed> {
        let mut tx = self.writer.begin().await?;

        sqlx::query(
            "UPDATE step_runs SET ended_at = ?, exit_code = ?, timed_out = ?, outcome_edge = ?
             WHERE job_id = ? AND run_id = ? AND step_id = ? AND attempt = ?",
        )
        .bind(to_ts(close.ended_at))
        .bind(close.exit_code)
        .bind(close.timed_out)
        .bind(close.outcome_edge)
        .bind(close.job.0)
        .bind(close.run.0)
        .bind(close.step)
        .bind(close.attempt as i64)
        .execute(&mut *tx)
        .await?;

        let definition = load_job_in(&mut tx, close.job).await?;
        if !definition.approval_valid() || !matches_review(&definition, expected_approval) {
            tx.commit().await?;
            return Ok(StepClosed {
                advanced: false,
                drained: None,
            });
        }
        let advanced = match &close.next {
            NextCursor::Waiting { step, at } => {
                sqlx::query(&format!(
                    "UPDATE runs SET status = ?, cursor_kind = 'waiting', cursor_step = ?, cursor_at = ?
                     WHERE job_id = ? AND id = ? {OWNS_CURSOR}"
                ))
                .bind(RunStatus::Waiting.as_str())
                .bind(step)
                .bind(to_ts(at))
                .bind(close.job.0)
                .bind(close.run.0)
                .bind(close.step)
                .bind(close.job.0)
                .bind(close.run.0)
                .bind(close.step)
                .bind(close.attempt as i64)
                .execute(&mut *tx)
                .await?
            }
            NextCursor::Terminal {
                status,
                fail_reason,
            } => {
                sqlx::query(&format!(
                    "UPDATE runs SET status = ?, cursor_kind = 'done', cursor_step = NULL,
                            cursor_at = NULL, ended_at = ?, fail_reason = ?
                     WHERE job_id = ? AND id = ? {OWNS_CURSOR}"
                ))
                .bind(status.as_str())
                .bind(to_ts(close.ended_at))
                .bind(fail_reason.as_deref())
                .bind(close.job.0)
                .bind(close.run.0)
                .bind(close.step)
                .bind(close.job.0)
                .bind(close.run.0)
                .bind(close.step)
                .bind(close.attempt as i64)
                .execute(&mut *tx)
                .await?
            }
        }
        .rows_affected()
            > 0;

        if !advanced {
            // The step row is closed; the run is somebody else's now.
            tx.commit().await.context("committing finish_step")?;
            return Ok(StepClosed {
                advanced: false,
                drained: None,
            });
        }

        // A run that just ended frees the §4.2 queue, and that release
        // belongs in this commit rather than after it.
        let drained = if matches!(close.next, NextCursor::Terminal { .. }) {
            let drained = Self::drain_queued_in(&mut tx, close.job, close.entry_step).await?;
            finish_job_if_exhausted(&mut tx, close.job).await?;
            drained
        } else {
            None
        };

        for spec in &close.notifications {
            sqlx::query(
                "INSERT INTO notifications (job_id, run_id, title, body, created_at)
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(close.job.0)
            .bind(close.run.0)
            .bind(&spec.title)
            .bind(&spec.body)
            .bind(to_ts(close.ended_at))
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await.context("committing finish_step")?;
        Ok(StepClosed {
            advanced: true,
            drained,
        })
    }

    /// Is this claim still the live one — i.e. is the cursor the
    /// `Running{step}` that `begin_step` returned *this* attempt for?
    ///
    /// Asked once more immediately before a process is spawned. The claim
    /// and the §2.3 registry entry cannot be made in one atomic act — one
    /// is a store commit, the other is in-memory — so between them there is
    /// a moment when a `cued cancel` finds no handle to signal and the
    /// process starts anyway, after the user was told the run was
    /// terminated. This is the second look that closes it: past this point
    /// the handle is registered, so a cancel reaches the attempt.
    ///
    /// Ownership, not just the step name: if this check had to be retried,
    /// the cursor it finds may be a newer attempt's (`Claim`), and spawning
    /// against that would put two processes on one step.
    pub async fn claim_is_current(&self, job: JobId, run: RunId, claim: Claim<'_>) -> Result<bool> {
        self.claim_is_current_checked(job, run, claim, None).await
    }

    pub async fn claim_is_current_checked(
        &self,
        job: JobId,
        run: RunId,
        claim: Claim<'_>,
        expected_approval: Option<[u8; 32]>,
    ) -> Result<bool> {
        let definition = self.load_job(job).await?;
        // Ownership and approval only, not `can_start`. The job was Active
        // when `begin_step` claimed this attempt; a `cued pause` landing
        // since then must let the step run (§4.2: a step already running
        // when you pause keeps running), because this is not a claim that
        // can be taken back — the cursor already says `running`, and a
        // `false` here leaves it there with no process and nothing left to
        // close it. `cued cancel` needs no status check: it moves the cursor,
        // which `OWNS_CURSOR` sees.
        if !definition.approval_valid() || !matches_review(&definition, expected_approval) {
            return Ok(false);
        }
        let owned: Option<i64> = sqlx::query_scalar(&format!(
            "SELECT 1 FROM runs WHERE job_id = ? AND id = ? {OWNS_CURSOR}"
        ))
        .bind(job.0)
        .bind(run.0)
        .bind(claim.step)
        .bind(job.0)
        .bind(run.0)
        .bind(claim.step)
        .bind(claim.attempt as i64)
        .fetch_optional(&self.reader)
        .await?;
        Ok(owned.is_some())
    }

    /// Close one attempt's row without touching the run's cursor — used
    /// when something outside the step decided the run's fate (§3.2's
    /// deadline). The attempt really did run and really did stop; the audit
    /// trail should say so even though the graph never got to route it.
    pub async fn close_attempt(
        &self,
        job: JobId,
        run: RunId,
        step: &str,
        attempt: u32,
        ended_at: &Timestamp,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE step_runs SET ended_at = ?
             WHERE job_id = ? AND run_id = ? AND step_id = ? AND attempt = ? AND ended_at IS NULL",
        )
        .bind(to_ts(ended_at))
        .bind(job.0)
        .bind(run.0)
        .bind(step)
        .bind(attempt as i64)
        .execute(&self.writer)
        .await?;
        Ok(())
    }

    /// How many times a step has run within this run — the §3.2 visit
    /// counter that both `max_visits` and `Backoff` read. Scoped to the
    /// current rewind epoch: a manual `cued retry` resets it (§3.4) while
    /// the attempt history stays.
    pub async fn visit_count(&self, job: JobId, run: RunId, step: &str) -> Result<u32> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM step_runs
             WHERE job_id = ? AND run_id = ? AND step_id = ?
               AND epoch = (SELECT epoch FROM runs WHERE job_id = ? AND id = ?)",
        )
        .bind(job.0)
        .bind(run.0)
        .bind(step)
        .bind(job.0)
        .bind(run.0)
        .fetch_one(&self.reader)
        .await?;
        Ok(count as u32)
    }

    /// The newest attempt number for a step in this run (0 = never started).
    pub async fn latest_attempt(&self, job: JobId, run: RunId, step: &str) -> Result<u32> {
        let attempt: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(attempt), 0) FROM step_runs
             WHERE job_id = ? AND run_id = ? AND step_id = ?",
        )
        .bind(job.0)
        .bind(run.0)
        .bind(step)
        .fetch_one(&self.reader)
        .await?;
        Ok(attempt as u32)
    }

    // -----------------------------------------------------------------------
    // Reconciliation & manual control (§3.4)
    // -----------------------------------------------------------------------

    /// Runs whose cursor still says `Running` — found at startup, that means
    /// the step's process was killed with the daemon (§3.4 Case 2).
    pub async fn interrupted_runs(&self) -> Result<Vec<(JobId, RunId, StepId)>> {
        let rows =
            sqlx::query("SELECT job_id, id, cursor_step FROM runs WHERE cursor_kind = 'running'")
                .fetch_all(&self.reader)
                .await?;
        Ok(rows
            .into_iter()
            .map(|row| {
                (
                    JobId(row.get("job_id")),
                    RunId(row.get("id")),
                    row.get("cursor_step"),
                )
            })
            .collect())
    }

    /// §3.4 Case 2, `Hold`: park the run for a human, and enqueue the
    /// `on_hold` notification in the same transaction — parked and silent
    /// must be impossible. The killed attempt's row stays open (no
    /// ended_at): its true fate is unknown, and the record says so.
    ///
    /// `claim` is `Some` when the caller is parking a step it claimed: the
    /// park then has to prove ownership like any other post-claim write, or
    /// an attempt whose retries ran out parks the *newer* attempt that has
    /// since taken the cursor — and that one's process is still running.
    /// `None` (reconciliation, a step that failed before it was ever
    /// claimed) keeps the plain live-run guard.
    #[allow(clippy::too_many_arguments)] // one call shape, three call sites
    pub async fn hold_run(
        &self,
        job: JobId,
        run: RunId,
        step: &str,
        reason: HeldReason,
        notify: &NotifySpec,
        now: &Timestamp,
        claim: Option<Claim<'_>>,
    ) -> Result<()> {
        let mut tx = self.writer.begin().await?;
        // Only a live run can be parked: one `cued cancel` ended while its
        // step's close was failing stays Cancelled, and one already Held
        // doesn't announce itself twice.
        let guard = match claim {
            Some(_) => OWNS_CURSOR,
            None => "AND cursor_kind IN ('waiting', 'running')",
        };
        let sql = format!(
            "UPDATE runs SET status = ?, cursor_kind = 'held', cursor_step = ?,
                    cursor_at = NULL, held_reason = ?
             WHERE job_id = ? AND id = ? {guard}"
        );
        let mut query = sqlx::query(&sql)
            .bind(RunStatus::Held.as_str())
            .bind(step)
            .bind(held_reason_text(reason))
            .bind(job.0)
            .bind(run.0);
        if let Some(claim) = claim {
            query = query
                .bind(claim.step)
                .bind(job.0)
                .bind(run.0)
                .bind(claim.step)
                .bind(claim.attempt as i64);
        }
        let parked = query.execute(&mut *tx).await?.rows_affected() > 0;
        if !parked {
            return Ok(());
        }
        sqlx::query(
            "INSERT INTO notifications (job_id, run_id, title, body, created_at)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(job.0)
        .bind(run.0)
        .bind(&notify.title)
        .bind(&notify.body)
        .bind(to_ts(now))
        .execute(&mut *tx)
        .await?;
        tx.commit().await.context("committing hold_run")?;
        Ok(())
    }

    /// §3.4 Case 1, `Abandon`: the wait expired while nobody was looking and
    /// the job opted out of running late.
    pub async fn mark_missed(
        &self,
        job: JobId,
        run: RunId,
        entry_step: &str,
        now: &Timestamp,
        enqueue: Option<&NotifySpec>,
    ) -> Result<Option<(RunId, Timestamp)>> {
        let mut tx = self.writer.begin().await?;
        sqlx::query(
            "UPDATE runs SET status = ?, cursor_kind = 'done', cursor_step = NULL,
                    cursor_at = NULL, ended_at = ?
             WHERE job_id = ? AND id = ?",
        )
        .bind(RunStatus::Missed.as_str())
        .bind(to_ts(now))
        .bind(job.0)
        .bind(run.0)
        .execute(&mut *tx)
        .await?;
        // Abandoned is still ended, so the §4.2 queue is free — in this
        // commit, for the reason `drain_queued_in` gives.
        let drained = Self::drain_queued_in(&mut tx, job, entry_step).await?;
        finish_job_if_exhausted(&mut tx, job).await?;
        if let Some(spec) = enqueue {
            sqlx::query(
                "INSERT INTO notifications (job_id, run_id, title, body, created_at)
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(job.0)
            .bind(run.0)
            .bind(&spec.title)
            .bind(&spec.body)
            .bind(to_ts(now))
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await.context("committing mark_missed")?;
        Ok(drained)
    }

    /// When the run's clock started — §3.2's deadline is measured from
    /// here, "so a late catch-up start doesn't eat the budget". `None`
    /// before the first step claims the run.
    pub async fn run_started_at(&self, job: JobId, run: RunId) -> Result<Option<Timestamp>> {
        let started: Option<String> =
            sqlx::query_scalar("SELECT started_at FROM runs WHERE job_id = ? AND id = ?")
                .bind(job.0)
                .bind(run.0)
                .fetch_optional(&self.reader)
                .await?
                .flatten();
        started.as_deref().map(from_ts).transpose()
    }

    /// §3.2: the run outlived its `deadline`. Ends it `Failed` with the
    /// reason on record and fires `on_failure`, in one transaction.
    ///
    /// Conditional like every other terminal write: if the run already went
    /// terminal — `cued cancel` got there first — the deadline has nothing
    /// left to end, and must not overwrite what the user asked for with a
    /// failure they didn't.
    ///
    /// `claim`, as in `hold_run`: `Some` when a claimed step is what ran out
    /// of budget, so a held-up deadline can't end a newer attempt's run. That
    /// attempt's own row is closed in the same transaction either way.
    pub async fn fail_deadline(
        &self,
        job: JobId,
        run: RunId,
        entry_step: &str,
        now: &Timestamp,
        enqueue: Option<&NotifySpec>,
        claim: Option<Claim<'_>>,
    ) -> Result<Option<Option<(RunId, Timestamp)>>> {
        let mut tx = self.writer.begin().await?;
        let guard = match claim {
            Some(_) => OWNS_CURSOR,
            None => "AND cursor_kind != 'done'",
        };
        let sql = format!(
            "UPDATE runs SET status = ?, cursor_kind = 'done', cursor_step = NULL,
                    cursor_at = NULL, ended_at = ?, fail_reason = 'deadline'
             WHERE job_id = ? AND id = ? {guard}"
        );
        let mut query = sqlx::query(&sql)
            .bind(RunStatus::Failed.as_str())
            .bind(to_ts(now))
            .bind(job.0)
            .bind(run.0);
        if let Some(claim) = claim {
            query = query
                .bind(claim.step)
                .bind(job.0)
                .bind(run.0)
                .bind(claim.step)
                .bind(claim.attempt as i64);
        }
        let ended = query.execute(&mut *tx).await?.rows_affected() > 0;

        // The claiming attempt really did stop, whoever owns the run now —
        // record that in this same commit. As a second write it left a
        // window, and a crash in it, where the run read `Failed: deadline`
        // while the attempt that was killed for it still looked unfinished.
        if let Some(claim) = claim {
            sqlx::query(
                "UPDATE step_runs SET ended_at = ?
                 WHERE job_id = ? AND run_id = ? AND step_id = ? AND attempt = ?
                   AND ended_at IS NULL",
            )
            .bind(to_ts(now))
            .bind(job.0)
            .bind(run.0)
            .bind(claim.step)
            .bind(claim.attempt as i64)
            .execute(&mut *tx)
            .await?;
        }

        if !ended {
            tx.commit().await.context("committing deadline")?;
            return Ok(None);
        }

        let drained = Self::drain_queued_in(&mut tx, job, entry_step).await?;
        finish_job_if_exhausted(&mut tx, job).await?;
        if let Some(spec) = enqueue {
            sqlx::query(
                "INSERT INTO notifications (job_id, run_id, title, body, created_at)
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(job.0)
            .bind(run.0)
            .bind(&spec.title)
            .bind(&spec.body)
            .bind(to_ts(now))
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await.context("committing deadline")?;
        Ok(Some(drained))
    }

    /// §3.4 Case 2, `Retry` (gated by restart_safe): put the interrupted
    /// step back on a runnable Waiting cursor. Deliberately does NOT bump
    /// the epoch — automatic retries keep counting against max_visits, so a
    /// crash loop still hits a bound; only a human `retry` resets it.
    pub async fn rearm_interrupted(
        &self,
        job: JobId,
        run: RunId,
        step: &str,
        at: &Timestamp,
    ) -> Result<()> {
        let mut tx = self.writer.begin().await?;
        ensure!(
            load_job_in(&mut tx, job).await?.approval_valid(),
            "Pending approval — cannot rearm"
        );
        sqlx::query(
            "UPDATE runs SET status = ?, cursor_kind = 'waiting', cursor_step = ?, cursor_at = ?
             WHERE job_id = ? AND id = ?",
        )
        .bind(RunStatus::Waiting.as_str())
        .bind(step)
        .bind(to_ts(at))
        .bind(job.0)
        .bind(run.0)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// `cued continue` (§3.4): resume a Held run from where it parked.
    /// Returns the step to re-arm. No epoch bump — continuing is not a reset.
    pub async fn resume_held(&self, job: JobId, run: RunId, at: &Timestamp) -> Result<StepId> {
        let mut tx = self.writer.begin().await?;
        ensure!(
            load_job_in(&mut tx, job).await?.approval_valid(),
            "Pending approval — approve the stored definition first"
        );
        let row =
            sqlx::query("SELECT cursor_kind, cursor_step FROM runs WHERE job_id = ? AND id = ?")
                .bind(job.0)
                .bind(run.0)
                .fetch_optional(&mut *tx)
                .await?
                .with_context(|| format!("no such run {job}.{run}"))?;
        let kind: String = row.get("cursor_kind");
        ensure!(
            kind == "held",
            "{job}.{run} isn't held (it's {kind}) — `continue` only resumes held runs"
        );
        let step: StepId = row.get("cursor_step");

        sqlx::query(
            "UPDATE runs SET status = ?, cursor_kind = 'waiting', cursor_at = ?, held_reason = NULL
             WHERE job_id = ? AND id = ?",
        )
        .bind(RunStatus::Waiting.as_str())
        .bind(to_ts(at))
        .bind(job.0)
        .bind(run.0)
        .execute(&mut *tx)
        .await?;
        tx.commit().await.context("committing resume_held")?;
        Ok(step)
    }

    /// `cued retry` (§3.4): rewind a terminal or Held run IN PLACE — same
    /// run id, cursor back to `step`, runnable now. The epoch bump is what
    /// resets max_visits/Backoff counting while attempts keep appending. A
    /// finished one-shot's job comes back to Active for the duration.
    pub async fn rewind_run(
        &self,
        job: JobId,
        run: RunId,
        step: &str,
        at: &Timestamp,
    ) -> Result<()> {
        let mut tx = self.writer.begin().await?;
        let definition = load_job_in(&mut tx, job).await?;
        ensure!(
            definition.status != JobStatus::Expired && definition.approval_valid(),
            "Pending approval or expired — cannot retry"
        );
        let kind: String =
            sqlx::query_scalar("SELECT cursor_kind FROM runs WHERE job_id = ? AND id = ?")
                .bind(job.0)
                .bind(run.0)
                .fetch_optional(&mut *tx)
                .await?
                .with_context(|| format!("no such run {job}.{run}"))?;
        ensure!(
            kind == "held" || kind == "done",
            "{job}.{run} is still live — `retry` re-runs terminal or held runs"
        );

        // §4.2 allows a recurring job at most one live run — there is no
        // `Concurrent` in v1 — and §3.4 has the *next firing* apply the
        // overlap policy against a retried run, which presumes the retried
        // run is the live one. Rewinding while another is still going would
        // make two, quietly, by a route no policy governs.
        let live: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM runs
             WHERE job_id = ? AND id != ? AND cursor_kind != 'done'
             ORDER BY id LIMIT 1",
        )
        .bind(job.0)
        .bind(run.0)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(other) = live {
            bail!(
                "{job}.r{other} is still live — cancel or wait for it before                  retrying {job}.{run}; a job runs one at a time (§4.2)"
            );
        }

        sqlx::query(
            "UPDATE runs SET status = ?, cursor_kind = 'waiting', cursor_step = ?, cursor_at = ?,
                    ended_at = NULL, fail_reason = NULL, held_reason = NULL, epoch = epoch + 1
             WHERE job_id = ? AND id = ?",
        )
        .bind(RunStatus::Pending.as_str())
        .bind(step)
        .bind(to_ts(at))
        .bind(job.0)
        .bind(run.0)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE jobs SET status = ? WHERE id = ? AND status IN ('done', 'cancelled')")
            .bind(JobStatus::Active.as_str())
            .bind(job.0)
            .execute(&mut *tx)
            .await?;
        tx.commit().await.context("committing rewind_run")?;
        Ok(())
    }

    /// The latest run of a job, with where its cursor sits — what
    /// continue/retry target (§3.4: "targets the latest run").
    /// What `cued continue` / `cued retry` act on: "the latest run" (§3.4),
    /// where latest skips the rows that never were runs.
    ///
    /// A `Skipped` row records a firing the `overlap` or `catch_up` policy
    /// declined (§4.2) — and a single row can stand for a whole coalesced
    /// range of them. It has no attempts and no step to restart from, so
    /// rewinding one means starting the graph from its entry while erasing
    /// the record of what was skipped. On a recurring job it is also
    /// routinely the *newest* row, so plain "latest" pointed `retry` at it
    /// in preference to the failed run the user actually meant.
    ///
    /// `Missed` stays eligible: that is one abandoned run, and "run it
    /// anyway" is a coherent thing to ask for.
    pub async fn latest_run_cursor(&self, job: JobId) -> Result<(RunId, String, Option<StepId>)> {
        let row = sqlx::query(
            "SELECT id, cursor_kind, cursor_step FROM runs
             WHERE job_id = ? AND status != 'skipped'
             ORDER BY id DESC LIMIT 1",
        )
        .bind(job.0)
        .fetch_optional(&self.reader)
        .await?
        .with_context(|| format!("job {job} has no run to act on"))?;
        Ok((
            RunId(row.get("id")),
            row.get("cursor_kind"),
            row.get("cursor_step"),
        ))
    }

    // -----------------------------------------------------------------------
    // The notification queue (§3.5) — enqueue lives in the step-close /
    // hold / missed transactions; these are the delivery half.
    // -----------------------------------------------------------------------

    /// The backlog, oldest first (the partial index covers this).
    pub async fn undelivered_notifications(&self) -> Result<Vec<PendingNotification>> {
        let rows = sqlx::query(
            "SELECT id, title, body FROM notifications WHERE delivered_at IS NULL ORDER BY id",
        )
        .fetch_all(&self.reader)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| PendingNotification {
                id: row.get("id"),
                spec: NotifySpec {
                    title: row.get("title"),
                    body: row.get("body"),
                },
            })
            .collect())
    }

    /// Record a confirmed display — only ever after the transport
    /// acknowledged it (§3.5: never mark before the send is confirmed).
    pub async fn mark_delivered(
        &self,
        id: i64,
        receipt: Option<&DeliveryReceipt>,
        now: &Timestamp,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE notifications SET delivered_at = ?, delivery_server = ?, delivery_id = ?
             WHERE id = ?",
        )
        .bind(to_ts(now))
        .bind(receipt.map(|receipt| receipt.server.as_str()))
        .bind(receipt.map(|receipt| i64::from(receipt.id)))
        .bind(id)
        .execute(&self.writer)
        .await?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Recurrence (§4.2)
    // -----------------------------------------------------------------------

    /// Record one §4.2 firing atomically: optionally the new Run (parked on
    /// its virtual first sleep-edge), optionally a compacted skipped-range
    /// row, optionally the queue slot, and always the re-arm target —
    /// "re-arm at fire time, not completion."
    ///
    /// **The re-arm is a conditional claim, and it goes first.** §3.3 gives
    /// `Arm::Step` exactly this: `begin_step` only advances a cursor that
    /// still says what it expects, so a stale or duplicated heap entry
    /// claims nothing. A firing needs the same guarantee and had none — it
    /// re-read the job's status outside this transaction and then wrote
    /// unconditionally. Two consequences followed:
    ///
    /// - A `cued cancel` committing between that read and this write was
    ///   undone: the firing reinstated `next_fire_at` on a cancelled job and
    ///   left a pending run behind it.
    /// - Two heap entries for one instant — which `cued pause` followed by
    ///   `cued resume` produces, since the pause leaves the original arm in
    ///   place — both fired, and both emitted the *next* arm. Duplicates
    ///   doubled every cycle rather than cancelling out.
    ///
    /// Claiming `next_fire_at` makes a firing exactly-once by construction:
    /// only the arm that still matches the store gets to act.
    pub async fn record_firing(&self, firing: Firing<'_>) -> Result<Fired> {
        self.record_firing_checked(firing, None).await
    }

    pub async fn record_firing_checked(
        &self,
        firing: Firing<'_>,
        expected_approval: Option<[u8; 32]>,
    ) -> Result<Fired> {
        let mut tx = self.writer.begin().await?;
        let definition = load_job_in(&mut tx, firing.job).await?;
        if !definition.can_start() || !matches_review(&definition, expected_approval) {
            return Ok(Fired::Superseded);
        }

        // The claim, before anything is written. `status = 'active'` covers
        // a pause (which leaves `next_fire_at` alone); the instant match
        // covers a duplicate arm and a cancel (which clears it).
        let claimed = sqlx::query(
            "UPDATE jobs SET next_fire_at = ?, fired = fired + ?
             WHERE id = ? AND status = 'active' AND next_fire_at = ?",
        )
        .bind(firing.next_fire_at.map(to_ts))
        // The budget is spent in the very statement that claims the instant,
        // so a firing that isn't ours spends nothing.
        .bind(i64::from(firing.consumed))
        .bind(firing.job.0)
        .bind(to_ts(firing.claiming))
        .execute(&mut *tx)
        .await?;
        if claimed.rows_affected() == 0 {
            return Ok(Fired::Superseded);
        }

        // The skip row first, so a coalesced range of older instants gets
        // the smaller run id than the run that actually executes.
        if let Some((from, to, count)) = firing.skipped {
            let id: i64 = next_run_id(&mut tx, firing.job).await?;
            sqlx::query(
                "INSERT INTO runs (job_id, id, scheduled_for, status, cursor_kind,
                                   ended_at, skipped_from, skipped_count)
                 VALUES (?, ?, ?, ?, 'done', ?, ?, ?)",
            )
            .bind(firing.job.0)
            .bind(id)
            .bind(to_ts(to))
            .bind(RunStatus::Skipped.as_str())
            .bind(to_ts(firing.now))
            .bind(from.map(to_ts))
            .bind(count)
            .execute(&mut *tx)
            .await?;
        }

        let mut created = None;
        if let Some(at) = firing.run_at {
            let id: i64 = next_run_id(&mut tx, firing.job).await?;
            sqlx::query(
                "INSERT INTO runs (job_id, id, scheduled_for, status, cursor_kind, cursor_step, cursor_at)
                 VALUES (?, ?, ?, ?, 'waiting', ?, ?)",
            )
            .bind(firing.job.0)
            .bind(id)
            .bind(to_ts(at))
            .bind(RunStatus::Pending.as_str())
            .bind(firing.entry_step)
            .bind(to_ts(at))
            .execute(&mut *tx)
            .await?;
            created = Some(RunId(id));
        }

        if let Some(at) = firing.queue_at {
            // Overwriting IS the §4.2 coalescing: one slot, latest instant.
            sqlx::query("UPDATE jobs SET queued_at = ? WHERE id = ?")
                .bind(to_ts(at))
                .bind(firing.job.0)
                .execute(&mut *tx)
                .await?;
        }

        // In the same commit (see `drain_queued_in`): queueing and draining
        // are one decision, and splitting them left the second recoverable
        // only by restart.
        let drained = Self::drain_queued_in(&mut tx, firing.job, firing.entry_step).await?;

        // A pure-skip final firing ends the job right here — there will be
        // no run-terminal event to notice the exhaustion later.
        finish_job_if_exhausted(&mut tx, firing.job).await?;

        tx.commit().await.context("committing firing")?;
        Ok(Fired::Recorded {
            run: created,
            drained,
        })
    }

    /// Enqueue a notification outside any particular run (§3.5) — for the
    /// daemon telling the user about the *job* rather than about a run.
    pub async fn enqueue_job_notification(
        &self,
        job: JobId,
        spec: &NotifySpec,
        now: &Timestamp,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO notifications (job_id, run_id, title, body, created_at)
             VALUES (?, NULL, ?, ?, ?)",
        )
        .bind(job.0)
        .bind(&spec.title)
        .bind(&spec.body)
        .bind(to_ts(now))
        .execute(&self.writer)
        .await?;
        Ok(())
    }

    /// The §4.2 drain, inside a caller's transaction.
    ///
    /// Draining has to commit *with* whatever freed the queue, not after it.
    /// Two commits meant a window where the first had landed and the second
    /// had not: a retry of the firing then found its instant already
    /// consumed (`Superseded`) and a retry of the step found its cursor
    /// already terminal, so neither could finish the job, and the queued run
    /// waited for a restart. One transaction has no such window — it either
    /// all happened or none of it did, and the retry re-claims cleanly.
    async fn drain_queued_in(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        job: JobId,
        entry_step: &str,
    ) -> Result<Option<(RunId, Timestamp)>> {
        if !load_job_in(tx, job).await?.can_start() {
            return Ok(None);
        }
        let queued: Option<String> = sqlx::query_scalar("SELECT queued_at FROM jobs WHERE id = ?")
            .bind(job.0)
            .fetch_optional(&mut **tx)
            .await?
            .flatten();
        let Some(at_text) = queued else {
            return Ok(None);
        };
        let live: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM runs WHERE job_id = ? AND cursor_kind != 'done'",
        )
        .bind(job.0)
        .fetch_one(&mut **tx)
        .await?;
        if live > 0 {
            // §4.2: a Held run blocks the queue indefinitely — deliberately.
            return Ok(None);
        }
        let paused: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE id = ? AND status = 'paused'")
                .bind(job.0)
                .fetch_one(&mut **tx)
                .await?;
        if paused > 0 {
            return Ok(None);
        }

        let at = from_ts(&at_text)?;
        let id: i64 = next_run_id(tx, job).await?;
        sqlx::query(
            "INSERT INTO runs (job_id, id, scheduled_for, status, cursor_kind, cursor_step, cursor_at)
             VALUES (?, ?, ?, ?, 'waiting', ?, ?)",
        )
        .bind(job.0)
        .bind(id)
        .bind(&at_text)
        .bind(RunStatus::Pending.as_str())
        .bind(entry_step)
        .bind(&at_text)
        .execute(&mut **tx)
        .await?;
        sqlx::query("UPDATE jobs SET queued_at = NULL WHERE id = ?")
            .bind(job.0)
            .execute(&mut **tx)
            .await?;
        Ok(Some((RunId(id), at)))
    }

    /// Turn the queued firing into a Run — only when the slot is filled AND
    /// nothing is live. Called both when a run ends and after queueing (the
    /// two sides of the finish/fire race); the conditions make it
    /// idempotent, so calling it twice can't create two runs.
    pub async fn drain_queued(
        &self,
        job: JobId,
        entry_step: &str,
    ) -> Result<Option<(RunId, Timestamp)>> {
        let mut tx = self.writer.begin().await?;
        let drained = Self::drain_queued_in(&mut tx, job, entry_step).await?;
        tx.commit().await.context("committing queue drain")?;
        Ok(drained)
    }

    /// Is any run of this job non-terminal (running, waiting, or held)?
    /// The §4.2 overlap question.
    pub async fn has_live_run(&self, job: JobId) -> Result<bool> {
        let live: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM runs WHERE job_id = ? AND cursor_kind != 'done'",
        )
        .bind(job.0)
        .fetch_one(&self.reader)
        .await?;
        Ok(live > 0)
    }

    /// Firing instants on record — each run counts 1, a coalesced skip row
    /// counts its whole range. What the §4.1 `count` cap is measured in.
    /// Firings this job has consumed, ever — the measure §4.1's `count` caps.
    ///
    /// A **durable counter**, not a sum over rows. Rows are the wrong shape
    /// for this in two separate ways:
    ///
    /// - The §4.2 queue slot is one slot however many firings coalesce into
    ///   it. Counting rows missed them entirely; counting the slot as *one*
    ///   missed every overwrite after the first, so a long run under
    ///   `overlap = queue` still re-armed for ever at any cap above three —
    ///   three being the boundary that happens to terminate anyway.
    /// - §10.2's GC deletes rows. A count derived from them therefore *falls*
    ///   as history is pruned, handing a capped job fresh firings it had
    ///   already spent.
    ///
    /// Incremented inside the same conditional claim that re-arms the job
    /// (`record_firing`), by however many instants that firing consumed, so
    /// a firing that isn't ours increments nothing.
    pub async fn fired_count(&self, job: JobId) -> Result<u32> {
        let fired: i64 = sqlx::query_scalar("SELECT fired FROM jobs WHERE id = ?")
            .bind(job.0)
            .fetch_optional(&self.reader)
            .await?
            .unwrap_or(0);
        Ok(fired as u32)
    }

    /// Active recurring jobs and their §4.2 re-arm targets — the recurring
    /// half of the startup heap rebuild (§5.3).
    pub async fn recurring_arms(&self) -> Result<Vec<(JobId, Timestamp)>> {
        let rows = sqlx::query(
            "SELECT id, next_fire_at FROM jobs
             WHERE status = 'active' AND next_fire_at IS NOT NULL AND (approval IS NULL OR json_extract(approval, '$.state') = 'approved')",
        )
        .fetch_all(&self.reader)
        .await?;
        rows.into_iter()
            .map(|row| Ok((JobId(row.get("id")), from_ts(row.get("next_fire_at"))?)))
            .collect()
    }

    /// Jobs with a filled queue slot — reconciliation drains any whose run
    /// ended while the daemon was down.
    pub async fn queued_jobs(&self) -> Result<Vec<JobId>> {
        let rows = sqlx::query("SELECT id FROM jobs WHERE queued_at IS NOT NULL")
            .fetch_all(&self.reader)
            .await?;
        Ok(rows.into_iter().map(|row| JobId(row.get("id"))).collect())
    }

    /// §4.2 `cued pause`: stop re-arming; an in-flight run continues.
    pub async fn set_paused(&self, job: JobId) -> Result<()> {
        let mut tx = self.writer.begin().await?;
        let status = job_status(&job_status_row(&mut tx, job).await?)?;
        match status {
            JobStatus::Active => {}
            JobStatus::Paused => bail!("job {job} is already paused"),
            JobStatus::Done | JobStatus::Cancelled | JobStatus::Expired => {
                bail!("job {job} already ended — nothing to pause")
            }
        }
        sqlx::query("UPDATE jobs SET status = ? WHERE id = ?")
            .bind(JobStatus::Paused.as_str())
            .bind(job.0)
            .execute(&mut *tx)
            .await?;
        tx.commit().await.context("committing pause")?;
        Ok(())
    }

    /// §4.2 `cued cancel`: the job stops re-arming and every live run is
    /// marked `Cancelled`. Returns the runs that were live, so the caller can
    /// reach their processes through the §2.3 registry.
    ///
    /// The store write is the authority and lands first — "persist intent,
    /// then act" (§3.3) applies to stopping as much as to starting. The
    /// process teardown that follows is the §2.2 sequence and takes up to
    /// `kill_grace`; a daemon that died inside that window would leave an
    /// orphan, which is the same uncleanly-killed-daemon case §2.2 already
    /// declares out of scope.
    pub async fn cancel_job(&self, job: JobId, now: &Timestamp) -> Result<Vec<RunId>> {
        self.cancel_job_with_clock(job, || *now).await
    }

    /// Expiry is evaluated at mutation time, after acquiring the database writer.
    /// Waiting behind GC must not let cancellation overwrite an elapsed expiry.
    pub async fn cancel_job_with_clock(
        &self,
        job: JobId,
        clock: impl FnOnce() -> Timestamp,
    ) -> Result<Vec<RunId>> {
        let mut tx = self.writer.begin_with("BEGIN IMMEDIATE").await?;
        let now = &clock();
        // Expiry at `now >= deadline` wins over a cancel sharing the writer;
        // keep its durable time and cause rather than overwriting them.
        let definition = load_job_in(&mut tx, job).await?;
        if expire_in(&mut tx, &definition, now).await? {
            tx.commit().await?;
            bail!(
                "job {job} already ended (it's {:?}) — nothing to cancel",
                JobStatus::Expired
            );
        }
        let status = job_status(&job_status_row(&mut tx, job).await?)?;
        ensure!(
            status.is_live(),
            "job {job} already ended (it's {status:?}) — nothing to cancel"
        );

        let live: Vec<i64> = sqlx::query_scalar(
            "SELECT id FROM runs WHERE job_id = ? AND cursor_kind != 'done' ORDER BY id",
        )
        .bind(job.0)
        .fetch_all(&mut *tx)
        .await?;

        sqlx::query(
            "UPDATE runs SET status = ?, cursor_kind = 'done', cursor_step = NULL,
                    cursor_at = NULL, held_reason = NULL, ended_at = ?
             WHERE job_id = ? AND cursor_kind != 'done'",
        )
        .bind(RunStatus::Cancelled.as_str())
        .bind(to_ts(now))
        .bind(job.0)
        .execute(&mut *tx)
        .await?;

        // Stop re-arming: no next firing, and drop anything coalesced into
        // the §4.2 queue slot — it was never a run, and cancel means cancel.
        sqlx::query(
            "UPDATE jobs SET status = ?, next_fire_at = NULL, queued_at = NULL WHERE id = ?",
        )
        .bind(JobStatus::Cancelled.as_str())
        .bind(job.0)
        .execute(&mut *tx)
        .await?;

        tx.commit().await.context("committing cancel")?;
        Ok(live.into_iter().map(RunId).collect())
    }

    /// §4.2 `cued resume`: re-arm to the next FUTURE instant only — resume
    /// never back-fills, so the caller computes `next_fire_at` from now.
    ///
    /// Runs that came due while paused are moved up to `now`: the pause was
    /// the user saying "not yet", not the machine missing its moment, so the
    /// time spent paused must not count against §3.4 Case 1's `missed_wait`.
    /// Left on their old targets, a pause longer than the missed grace would
    /// have a `missed_wait = abandon` run marked Missed the instant it was
    /// resumed. Downtime *after* the resume still counts, from here.
    pub async fn set_resumed(
        &self,
        job: JobId,
        next_fire_at: Option<&Timestamp>,
        now: &Timestamp,
    ) -> Result<()> {
        let mut tx = self.writer.begin().await?;
        ensure!(
            load_job_in(&mut tx, job).await?.approval_valid(),
            "Pending approval — approve before resuming"
        );
        let status = job_status(&job_status_row(&mut tx, job).await?)?;
        ensure!(
            status == JobStatus::Paused,
            "job {job} isn't paused (it's {status:?})"
        );
        // The queue slot goes with the pause. Whatever was coalesced into it
        // fired before the pause, so keeping it would make `resume` run a
        // past occurrence — "resume never back-fills" (§4.2) has to mean the
        // queue as well as the schedule.
        sqlx::query("UPDATE jobs SET status = ?, next_fire_at = ?, queued_at = NULL WHERE id = ?")
            .bind(JobStatus::Active.as_str())
            .bind(next_fire_at.map(to_ts))
            .bind(job.0)
            .execute(&mut *tx)
            .await?;

        // Compared as instants, not as the stored text.
        // The redundant `!= 'done'` lets the planner use `runs_live`
        // rather than walking the job's whole history.
        let waiting = sqlx::query(
            "SELECT id, cursor_at FROM runs WHERE job_id = ? AND cursor_kind != 'done' AND cursor_kind = 'waiting'",
        )
        .bind(job.0)
        .fetch_all(&mut *tx)
        .await?;
        for row in waiting {
            if from_ts(row.get("cursor_at"))? >= *now {
                continue;
            }
            sqlx::query(
                "UPDATE runs SET cursor_at = ? WHERE job_id = ? AND id = ? AND cursor_kind = 'waiting'",
            )
            .bind(to_ts(now))
            .bind(job.0)
            .bind(row.get::<i64, _>("id"))
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await.context("committing resume")?;
        Ok(())
    }

    /// §2: every command accepts a job id (`j7`, or bare `7`) or a live
    /// job's name. Names are only unique among LIVE jobs, so terminal jobs
    /// must be addressed by id.
    pub async fn resolve_job(&self, reference: &str) -> Result<JobId> {
        let digits = reference.strip_prefix('j').unwrap_or(reference);
        if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
            let id: i64 = digits.parse().context("job id out of range")?;
            let exists: Option<i64> = sqlx::query_scalar("SELECT id FROM jobs WHERE id = ?")
                .bind(id)
                .fetch_optional(&self.reader)
                .await?;
            return exists
                .map(JobId)
                .with_context(|| format!("no such job j{id}"));
        }
        let found: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM jobs WHERE name = ? AND status IN ('active', 'paused')",
        )
        .bind(reference)
        .fetch_optional(&self.reader)
        .await?;
        found.map(JobId).with_context(|| {
            format!("no live job named {reference:?} (terminal jobs: use the j<n> id)")
        })
    }

    /// §10.2 retention: prune terminal runs that are older than
    /// `retention.days` **or** beyond `retention.runs_per_job`, whichever
    /// bites first — a union, so a recurring job can't accumulate
    /// unboundedly between sweeps and a quiet one still ages out.
    ///
    /// Only runs whose cursor is `done` are candidates. A `Held` run is
    /// waiting on a human decision (§3.4) and a `Waiting`/`Running` one is
    /// live; none of them may be collected out from under their owner, no
    /// matter how old. That is also why the age test alone isn't enough to
    /// bound the store: a job parked in Held forever keeps its history, by
    /// design.
    ///
    /// Returns what was pruned so the caller can remove the matching log
    /// directories — the store doesn't know where those live (§2.1).
    pub async fn gc(&self, retention: &Retention, now: &Timestamp) -> Result<GcOutcome> {
        let mut outcome = GcOutcome::default();
        self.gc_into(retention, now, &mut outcome).await?;
        Ok(outcome)
    }

    /// `gc`, recording each batch in `outcome` as it commits — so a sweep
    /// that fails part-way still reports the rows it did delete, and the
    /// caller can remove their logs rather than leak them.
    ///
    /// Two things shape this, both learned the hard way:
    ///
    /// - **Candidates are chosen inside the transaction that deletes them.**
    ///   Choosing from a reader snapshot first and deleting later let a
    ///   `cued retry` in between (which makes a `done` run live again, in
    ///   place) lose its run, attempts and logs to a decision about the run
    ///   it used to be. `BEGIN IMMEDIATE` takes the write lock before the
    ///   first read, so the choice and the delete see the same database.
    /// - **Nothing reads the whole history.** The sweep used to load every
    ///   run into memory to count and date them — a transient allocation
    ///   proportional to *retained* history, paid on every `cued gc` even
    ///   when nothing was due, which ratcheted the daemon's RSS up as the
    ///   store grew. It then deleted everything in one transaction, holding
    ///   the single writer long enough for firings queued behind it to time
    ///   out. Now each rule walks an index in `GC_BATCH`-sized transactions,
    ///   and firings interleave between them.
    pub async fn gc_into(
        &self,
        retention: &Retention,
        now: &Timestamp,
        outcome: &mut GcOutcome,
    ) -> Result<()> {
        self.expire_pending(now).await?;
        let cutoff = now
            .checked_sub(SignedDuration::from_hours(24 * i64::from(retention.days)))
            .context("retention window out of range")?;
        self.gc_by_age(&cutoff, outcome).await?;
        self.gc_by_count(retention.runs_per_job, outcome).await?;
        self.gc_orphan_jobs(&cutoff, outcome).await
    }

    /// The age rule: terminal runs that ended before `cutoff`, oldest first
    /// along `runs_gc`.
    ///
    /// The index orders stored *text*, and text order is time order only
    /// between different seconds (see `to_ts`). So the index bounds the walk
    /// at the second after the cutoff — every instant before the cutoff
    /// prints below that — and the parsed instant decides each row. Rows the
    /// walk passes over without deleting (a live cursor with an end time
    /// on record, or the cutoff's own second) are stepped past by
    /// `(ended_at, rowid)`, so they are read once per sweep, not once per
    /// batch.
    async fn gc_by_age(&self, cutoff: &Timestamp, outcome: &mut GcOutcome) -> Result<()> {
        let bound = to_ts(&Timestamp::from_second(cutoff.as_second() + 1)?);
        let mut after: (String, i64) = (String::new(), 0);
        loop {
            let mut tx = self.writer.begin_with("BEGIN IMMEDIATE").await?;
            let rows = sqlx::query(
                "SELECT rowid, job_id, id, cursor_kind, ended_at FROM runs
                 WHERE ended_at < ?1 AND ended_at >= ?2 AND (ended_at > ?2 OR rowid > ?3)
                 ORDER BY ended_at, rowid
                 LIMIT ?4",
            )
            .bind(&bound)
            .bind(&after.0)
            .bind(after.1)
            .bind(GC_BATCH)
            .fetch_all(&mut *tx)
            .await?;
            let Some(last) = rows.last() else {
                return Ok(());
            };
            after = (last.get("ended_at"), last.get("rowid"));
            let full = rows.len() as i64 == GC_BATCH;

            let mut doomed = Vec::new();
            for row in &rows {
                // Compared as parsed instants, never as text — see `to_ts`.
                if row.get::<&str, _>("cursor_kind") == "done"
                    && from_ts(row.get("ended_at"))? < *cutoff
                {
                    doomed.push((JobId(row.get("job_id")), RunId(row.get("id"))));
                }
            }
            delete_runs_in(&mut tx, &doomed).await?;
            tx.commit().await.context("committing gc batch")?;
            outcome.runs.extend(doomed);
            if !full {
                return Ok(());
            }
        }
    }

    /// The count rule: for each job, terminal runs with at least
    /// `runs_per_job` newer rows of any kind — live and skipped rows count
    /// as newer, as they always have. Jobs are visited one at a time along
    /// the primary key; the boundary is re-read inside every deleting
    /// transaction, so a firing between batches only ever moves it the
    /// safe way.
    async fn gc_by_count(&self, runs_per_job: u32, outcome: &mut GcOutcome) -> Result<()> {
        let keep = i64::from(runs_per_job);
        let mut previous = 0_i64;
        loop {
            let job: Option<i64> =
                sqlx::query_scalar("SELECT MIN(job_id) FROM runs WHERE job_id > ?")
                    .bind(previous)
                    .fetch_one(&self.reader)
                    .await?;
            let Some(job) = job else {
                return Ok(());
            };
            previous = job;

            // Cheap check from a reader first: most jobs are within their
            // count, and a write transaction per job would serialize the
            // sweep against every firing for nothing.
            if count_boundary(&self.reader, job, keep).await?.is_none() {
                continue;
            }
            loop {
                let mut tx = self.writer.begin_with("BEGIN IMMEDIATE").await?;
                let Some(boundary) = count_boundary(&mut *tx, job, keep).await? else {
                    break;
                };
                let ids: Vec<i64> = sqlx::query_scalar(
                    "SELECT id FROM runs
                     WHERE job_id = ? AND id <= ? AND cursor_kind = 'done'
                     ORDER BY id LIMIT ?",
                )
                .bind(job)
                .bind(boundary)
                .bind(GC_BATCH)
                .fetch_all(&mut *tx)
                .await?;
                let full = ids.len() as i64 == GC_BATCH;
                let doomed: Vec<_> = ids.into_iter().map(|id| (JobId(job), RunId(id))).collect();
                delete_runs_in(&mut tx, &doomed).await?;
                tx.commit().await.context("committing gc batch")?;
                outcome.runs.extend(doomed);
                if !full {
                    break;
                }
            }
        }
    }

    /// §10.2: "one-shot jobs whose run is pruned are pruned with it;
    /// recurring jobs persist until cancelled." Both fall out of one
    /// rule — a job that has ended and has no runs left is history with
    /// no history in it — and that rule can never touch a live job.
    async fn gc_orphan_jobs(&self, cutoff: &Timestamp, outcome: &mut GcOutcome) -> Result<()> {
        let mut tx = self.writer.begin_with("BEGIN IMMEDIATE").await?;
        let orphans: Vec<(i64, Option<String>)> = sqlx::query_as(
            "SELECT id, expired_at FROM jobs
             WHERE status IN ('done', 'cancelled', 'expired')
               AND NOT EXISTS (SELECT 1 FROM runs WHERE runs.job_id = jobs.id)",
        )
        .fetch_all(&mut *tx)
        .await?;
        let mut doomed = Vec::new();
        for (job, expired_at) in orphans {
            // An expiry stays visible for the retention window (§7.6).
            if expired_at
                .as_deref()
                .map(from_ts)
                .transpose()?
                .is_some_and(|at| at > *cutoff)
            {
                continue;
            }
            sqlx::query("DELETE FROM notifications WHERE job_id = ?")
                .bind(job)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM jobs WHERE id = ?")
                .bind(job)
                .execute(&mut *tx)
                .await?;
            doomed.push(JobId(job));
        }
        tx.commit().await.context("committing gc")?;
        outcome.jobs.extend(doomed);
        Ok(())
    }

    /// §6 `cued show`: the stored definition plus the two scheduling
    /// columns that aren't part of it — `next_fire_at` and the §4.2 queue
    /// slot both live on the row rather than in the document (§5.3).
    pub async fn job_detail(
        &self,
        id: JobId,
    ) -> Result<(Job, Option<Timestamp>, Option<Timestamp>)> {
        let job = self.load_job(id).await?;
        let row = sqlx::query("SELECT next_fire_at, queued_at FROM jobs WHERE id = ?")
            .bind(id.0)
            .fetch_one(&self.reader)
            .await?;
        let next_fire_at = row
            .get::<Option<&str>, _>("next_fire_at")
            .map(from_ts)
            .transpose()?;
        let queued_at = row
            .get::<Option<&str>, _>("queued_at")
            .map(from_ts)
            .transpose()?;
        Ok((job, next_fire_at, queued_at))
    }

    /// The §2.1 log manifest: which run to show and what attempts it has.
    ///
    /// "Latest run" deliberately means the latest run that *ran something*.
    /// A recurring job's newest row is often a `Skipped` or `Missed` one,
    /// which has no attempts and no output — resolving to it would answer
    /// `cued logs nightly` with silence while the logs the user wants sit
    /// one row back. An explicit `--run` still addresses any row.
    pub async fn log_manifest(
        &self,
        job: JobId,
        run: Option<i64>,
        step: Option<&str>,
        attempt: Option<u32>,
    ) -> Result<(RunId, Vec<LogAttempt>)> {
        let run = match run {
            Some(id) => {
                let exists: Option<i64> =
                    sqlx::query_scalar("SELECT id FROM runs WHERE job_id = ? AND id = ?")
                        .bind(job.0)
                        .bind(id)
                        .fetch_optional(&self.reader)
                        .await?;
                RunId(exists.with_context(|| format!("no such run {job}.r{id}"))?)
            }
            None => {
                let latest: Option<i64> = sqlx::query_scalar(
                    "SELECT COALESCE(
                       (SELECT MAX(run_id) FROM step_runs WHERE job_id = ?1),
                       (SELECT MAX(id) FROM runs WHERE job_id = ?1))",
                )
                .bind(job.0)
                .fetch_one(&self.reader)
                .await?;
                RunId(latest.with_context(|| format!("job {job} has no runs"))?)
            }
        };

        // Liveness comes from the cursor, never from an open `ended_at`:
        // §3.4 leaves a killed attempt's row open on purpose, so an absent
        // end time means "running" and "interrupted" alike. Only the cursor
        // separates them.
        let cursor: Option<(String, Option<String>)> =
            sqlx::query_as("SELECT cursor_kind, cursor_step FROM runs WHERE job_id = ? AND id = ?")
                .bind(job.0)
                .bind(run.0)
                .fetch_optional(&self.reader)
                .await?;
        let executing = match &cursor {
            Some((kind, step)) if kind == "running" => step.clone(),
            _ => None,
        };

        // Sequential execution (§3) means started_at order is the order the
        // steps ran; attempt breaks ties for a step retried within a run.
        let rows = sqlx::query(
            "SELECT step_id, attempt, started_at, ended_at, exit_code, timed_out
             FROM step_runs WHERE job_id = ? AND run_id = ?
             ORDER BY started_at, attempt",
        )
        .bind(job.0)
        .bind(run.0)
        .fetch_all(&self.reader)
        .await?;

        let mut attempts = Vec::new();
        for row in rows {
            let step_id: String = row.get("step_id");
            let ended_at = row
                .get::<Option<&str>, _>("ended_at")
                .map(from_ts)
                .transpose()?;
            let entry = LogAttempt {
                // An open row for the step the cursor is on: this is the
                // one actually executing. An open row for anything else was
                // interrupted and never resumed.
                running: ended_at.is_none() && executing.as_deref() == Some(step_id.as_str()),
                step: step_id,
                attempt: row.get::<i64, _>("attempt") as u32,
                started_at: from_ts(row.get("started_at"))?,
                ended_at,
                exit_code: row.get("exit_code"),
                timed_out: row.get::<i64, _>("timed_out") != 0,
            };
            // Few attempts per run, so filtering here beats building the
            // SQL dynamically for two optional predicates.
            if step.is_some_and(|want| want != entry.step) {
                continue;
            }
            if attempt.is_some_and(|want| want != entry.attempt) {
                continue;
            }
            attempts.push(entry);
        }
        Ok((run, attempts))
    }

    /// `cued wait`'s poll: where the job stands and the run `query` asks
    /// for. Read-only.
    ///
    /// `Latest` and `After` pass over `Skipped` rows. They record firings
    /// that didn't execute, and they are written with ids above the run
    /// they deferred to — after a live run under `overlap = skip`, and just
    /// before the executing run in a catch-up — so taking one as "the run"
    /// would report a skip while the real run is still going. `Exact`
    /// returns the row asked for, whatever it is.
    ///
    /// Returns the reply without step results, and whether the job awaits
    /// approval (so may be due to expire).
    pub async fn job_run(&self, job: JobId, query: RunQuery) -> Result<(JobRun, bool)> {
        // `last_id` is the high-water mark `next_run_id` allocates from, not
        // the rows that remain: retention can prune a job's newest runs,
        // and a pruned run must not look like one not created yet.
        let row = sqlx::query(&format!(
            "SELECT status, next_fire_at, queued_at IS NOT NULL AS queued,
                    next_fire_at IS NOT NULL OR queued_at IS NOT NULL AS armed,
                    COALESCE(json_extract(approval, '$.state') = 'pending', 0) AS pending,
                    {EXHAUSTED} AS exhausted,
                    MAX(run_seq, (SELECT COALESCE(MAX(id), 0) FROM runs
                                  WHERE job_id = jobs.id)) AS last_id
             FROM jobs WHERE id = ?"
        ))
        .bind(job.0)
        .fetch_one(&self.reader)
        .await?;
        let stored = job_status(row.get("status"))?;
        let pending = stored.is_live() && row.get::<i64, _>("pending") != 0;
        // Paused keeps its arming (resume re-arms from it), so the same test
        // holds for it as for Active.
        let more = stored.is_live() && (row.get::<i64, _>("armed") != 0 || pending);
        // An exhausted job stays Active until `finish_exhausted_jobs` marks
        // it Done, by the same `EXHAUSTED` rule. Report what it is about to be.
        let status = if row.get::<i64, _>("exhausted") != 0 {
            JobStatus::Done
        } else {
            stored
        };
        let queued = row.get::<i64, _>("queued") != 0;
        let next_fire_at = row
            .get::<Option<&str>, _>("next_fire_at")
            .map(from_ts)
            .transpose()?;

        const COLUMNS: &str = "id, status, ended_at, fail_reason, cursor_kind, cursor_at";
        let run = match query {
            RunQuery::Latest => {
                sqlx::query(&format!(
                    "SELECT {COLUMNS} FROM runs WHERE job_id = ? AND status != 'skipped'
                     ORDER BY id DESC LIMIT 1"
                ))
                .bind(job.0)
                .fetch_optional(&self.reader)
                .await?
            }
            RunQuery::Exact(id) => {
                sqlx::query(&format!(
                    "SELECT {COLUMNS} FROM runs WHERE job_id = ? AND id = ?"
                ))
                .bind(job.0)
                .bind(id)
                .fetch_optional(&self.reader)
                .await?
            }
            RunQuery::After(id) => {
                sqlx::query(&format!(
                    "SELECT {COLUMNS} FROM runs WHERE job_id = ? AND id > ? AND status != 'skipped'
                     ORDER BY id LIMIT 1"
                ))
                .bind(job.0)
                .bind(id)
                .fetch_optional(&self.reader)
                .await?
            }
        };

        let mut quiet_until = None;
        let run = match run {
            Some(row) => {
                if row.get::<&str, _>("cursor_kind") == "waiting" {
                    quiet_until = row
                        .get::<Option<&str>, _>("cursor_at")
                        .map(from_ts)
                        .transpose()?;
                }
                Some(RunEntry {
                    id: RunId(row.get("id")),
                    status: run_status(row.get("status"))?,
                    ended_at: row
                        .get::<Option<&str>, _>("ended_at")
                        .map(from_ts)
                        .transpose()?,
                    fail_reason: row.get("fail_reason"),
                })
            }
            None => {
                // A queued firing starts the moment the run ahead of it
                // ends, not at the next scheduled instant.
                if more && !pending && !queued {
                    quiet_until = next_fire_at;
                }
                None
            }
        };
        let reply = JobRun {
            job,
            status,
            more,
            run,
            last_id: row.get("last_id"),
            quiet_until,
            steps: None,
        };
        Ok((reply, pending))
    }

    /// The `cued list` view (§10.3): every job joined with its latest run;
    /// default scope is live jobs plus runs that ended in the last 24h
    /// (Held always shown), `all` shows everything retained.
    pub async fn list_overview(&self, all: bool, now: &Timestamp) -> Result<Vec<JobOverview>> {
        let rows = sqlx::query(
            "SELECT j.id AS job_id, j.name, j.status AS job_status, j.graph, j.next_fire_at, j.approval, j.source, j.expired_at, j.expiry_reason,
                    r.id AS run_id, r.status AS run_status, r.scheduled_for, r.ended_at,
                    r.cursor_at, r.fail_reason
             FROM jobs j
             LEFT JOIN runs r
               ON r.job_id = j.id
              AND r.id = (SELECT MAX(r2.id) FROM runs r2 WHERE r2.job_id = j.id)
             ORDER BY j.id",
        )
        .fetch_all(&self.reader)
        .await?;

        let mut jobs = Vec::new();
        for row in rows {
            let status = job_status(row.get("job_status"))?;
            let last_run = match row.get::<Option<i64>, _>("run_id") {
                Some(run_id) => Some(RunOverview {
                    id: RunId(run_id),
                    status: run_status(row.get("run_status"))?,
                    scheduled_for: from_ts(row.get("scheduled_for"))?,
                    ended_at: row
                        .get::<Option<&str>, _>("ended_at")
                        .map(from_ts)
                        .transpose()?,
                    cursor_at: row
                        .get::<Option<&str>, _>("cursor_at")
                        .map(from_ts)
                        .transpose()?,
                    fail_reason: row.get("fail_reason"),
                }),
                None => None,
            };

            let recent_or_held = last_run.as_ref().is_some_and(|run| {
                run.status == RunStatus::Held
                    || run
                        .ended_at
                        .as_ref()
                        .is_some_and(|ended| now.duration_since(*ended).as_secs() < 24 * 3600)
            });
            let expired_at = row
                .get::<Option<&str>, _>("expired_at")
                .map(from_ts)
                .transpose()?;
            let recently_expired =
                expired_at.is_some_and(|at| now.duration_since(at).as_secs() < 24 * 3600);
            if !(all || status.is_live() || recent_or_held || recently_expired) {
                continue;
            }

            jobs.push(JobOverview {
                id: JobId(row.get("job_id")),
                name: row.get("name"),
                status,
                approval: row
                    .get::<Option<&str>, _>("approval")
                    .map(serde_json::from_str)
                    .transpose()?,
                source: parse_source(row.get("source"))?,
                expired_at,
                expiry_reason: row
                    .get::<Option<&str>, _>("expiry_reason")
                    .map(serde_json::from_str)
                    .transpose()?,
                graph: serde_json::from_str(row.get("graph")).context("bad stored graph")?,
                next_fire_at: row
                    .get::<Option<&str>, _>("next_fire_at")
                    .map(from_ts)
                    .transpose()?,
                last_run,
            });
        }
        Ok(jobs)
    }
}

// ---------------------------------------------------------------------------
// Transaction helpers
// ---------------------------------------------------------------------------

/// A valid new approval must not authorize an old in-memory definition that
/// a scheduler task loaded before the definition changed and was reviewed again.
fn matches_review(job: &Job, expected: Option<[u8; 32]>) -> bool {
    expected.is_none_or(|hash| {
        job.approval
            .as_ref()
            .is_some_and(|a| a.definition_hash == hash)
    })
}

fn source_text(source: JobSource) -> &'static str {
    match source {
        JobSource::Cli => "cli",
        JobSource::Mcp => "mcp",
    }
}
fn parse_source(text: &str) -> Result<JobSource> {
    match text {
        "cli" => Ok(JobSource::Cli),
        "mcp" => Ok(JobSource::Mcp),
        _ => bail!("bad stored source"),
    }
}
fn decode_job(row: &sqlx::sqlite::SqliteRow) -> Result<Job> {
    let policies: PoliciesDoc = serde_json::from_str(row.get("policies"))?;
    Ok(Job {
        id: JobId(row.get("id")),
        name: row.get("name"),
        status: job_status(row.get("status"))?,
        schedule: serde_json::from_str(row.get("schedule"))?,
        graph: serde_json::from_str(row.get("graph"))?,
        cwd: row.get("cwd"),
        env: serde_json::from_str(row.get("env"))?,
        policies: policies.policies,
        hooks: policies.hooks,
        created_at: from_ts(row.get("created_at"))?,
        approval: row
            .get::<Option<&str>, _>("approval")
            .map(serde_json::from_str)
            .transpose()?,
        source: parse_source(row.get("source"))?,
        expired_at: row
            .get::<Option<&str>, _>("expired_at")
            .map(from_ts)
            .transpose()?,
        expiry_reason: row
            .get::<Option<&str>, _>("expiry_reason")
            .map(serde_json::from_str)
            .transpose()?,
    })
}
async fn load_job_in(tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>, id: JobId) -> Result<Job> {
    let row = sqlx::query("SELECT * FROM jobs WHERE id = ?")
        .bind(id.0)
        .fetch_optional(&mut **tx)
        .await?
        .with_context(|| format!("no such job {id}"))?;
    decode_job(&row)
}
async fn expire_all_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    now: &Timestamp,
) -> Result<()> {
    let rows = sqlx::query("SELECT * FROM jobs WHERE json_extract(approval, '$.state') = 'pending' AND status IN ('active', 'paused')")
        .fetch_all(&mut **tx).await?;
    for row in rows {
        expire_in(tx, &decode_job(&row)?, now).await?;
    }
    Ok(())
}
async fn expire_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    job: &Job,
    now: &Timestamp,
) -> Result<bool> {
    if !job.status.is_live()
        || job
            .approval
            .as_ref()
            .is_none_or(|a| a.state != ApprovalState::Pending)
    {
        return Ok(false);
    }
    let (deadline, reason) = job.approval_deadline()?;
    if *now < deadline {
        return Ok(false);
    }
    sqlx::query("UPDATE jobs SET status = 'expired', next_fire_at = NULL, queued_at = NULL, expired_at = ?, expiry_reason = ? WHERE id = ?")
        .bind(to_ts(&deadline)).bind(serde_json::to_string(&reason)?).bind(job.id.0).execute(&mut **tx).await?;
    Ok(true)
}

/// The id of the newest run of `job` that already has `keep` newer rows —
/// everything at or below it is beyond §10.2's count rule. None = the job
/// is within its count.
async fn count_boundary<'e, E: sqlx::SqliteExecutor<'e>>(
    executor: E,
    job: i64,
    keep: i64,
) -> Result<Option<i64>> {
    Ok(
        sqlx::query_scalar(
            "SELECT id FROM runs WHERE job_id = ? ORDER BY id DESC LIMIT 1 OFFSET ?",
        )
        .bind(job)
        .bind(keep)
        .fetch_optional(executor)
        .await?,
    )
}

/// Delete runs GC chose in this same transaction, children first — foreign
/// keys are on (§5.3). Notifications go with their run (§10.2).
///
/// Each deleted id is first folded into its job's `run_seq`, so the id
/// stays spent even if the row was written by a path that didn't advance
/// the sequence itself (a one-off's `r1`).
async fn delete_runs_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    runs: &[(JobId, RunId)],
) -> Result<()> {
    let mut highest: BTreeMap<i64, i64> = BTreeMap::new();
    for (job, run) in runs {
        let entry = highest.entry(job.0).or_default();
        *entry = (*entry).max(run.0);
    }
    for (job, run) in highest {
        sqlx::query("UPDATE jobs SET run_seq = MAX(run_seq, ?) WHERE id = ?")
            .bind(run)
            .bind(job)
            .execute(&mut **tx)
            .await?;
    }
    for (job, run) in runs {
        for statement in [
            "DELETE FROM step_runs WHERE job_id = ? AND run_id = ?",
            "DELETE FROM notifications WHERE job_id = ? AND run_id = ?",
            "DELETE FROM runs WHERE job_id = ? AND id = ?",
        ] {
            sqlx::query(statement)
                .bind(job.0)
                .bind(run.0)
                .execute(&mut **tx)
                .await?;
        }
    }
    Ok(())
}

/// The next id in the job's run sequence, never one handed out before.
///
/// `jobs.run_seq` is the durable high-water mark; the `MAX(id)` term covers
/// the one-off paths that insert their single run as `r1` directly. Taking
/// the larger of the two means GC pruning the newest rows can't rewind the
/// sequence (§10.2), however many of them it prunes.
async fn next_run_id(tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>, job: JobId) -> Result<i64> {
    sqlx::query_scalar(
        "UPDATE jobs
         SET run_seq = MAX(run_seq, (SELECT COALESCE(MAX(id), 0) FROM runs WHERE job_id = ?1)) + 1
         WHERE id = ?1
         RETURNING run_seq",
    )
    .bind(job.0)
    .fetch_optional(&mut **tx)
    .await?
    .with_context(|| format!("no such job {job}"))
}

async fn job_status_row(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    job: JobId,
) -> Result<String> {
    sqlx::query_scalar("SELECT status FROM jobs WHERE id = ?")
        .bind(job.0)
        .fetch_optional(&mut **tx)
        .await?
        .with_context(|| format!("no such job {job}"))
}

/// The one job-completion rule, every schedule shape alike: a job is Done
/// when nothing more is scheduled (`next_fire_at`), nothing is queued, and
/// no run is live. For a one-off that's simply "its run ended"; for a
/// recurring job it's §4.1's until/count exhaustion. Paused jobs never
/// match (status stays 'paused' — paused is not done).
async fn finish_job_if_exhausted(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    job: JobId,
) -> Result<()> {
    sqlx::query(
        "UPDATE jobs SET status = 'done'
         WHERE id = ? AND status = 'active'
           AND (approval IS NULL OR json_extract(approval, '$.state') = 'approved')
           AND next_fire_at IS NULL AND queued_at IS NULL
           AND NOT EXISTS (SELECT 1 FROM runs WHERE job_id = ? AND cursor_kind != 'done')",
    )
    .bind(job.0)
    .bind(job.0)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Column mappings (§5.3)
// ---------------------------------------------------------------------------

/// Timestamps are RFC 3339 UTC text (§5.3), and now round-trip as bare
/// `Timestamp`s — no zone is invented on the way out. Reading used to stamp
/// every instant with the *daemon's* zone, which was indistinguishable from
/// the submitter's only because they are always the same process's user
/// (§7.2). Rendering picks a zone explicitly, at the edge.
///
/// **Not fixed-width, and text order is not time order.** Fractional seconds
/// are printed only when non-zero, so `…09:00:00Z` (20 chars) and
/// `…09:00:00.5Z` (22) compare with the *later* instant sorting first. The
/// prefixes only collide within one second, so this has never mattered — but
/// it means no query may order or range over these columns as text. GC
/// (§10.2) is the one place that would want to, and it deliberately compares
/// parsed instants in Rust instead.
fn to_ts(at: &Timestamp) -> String {
    at.to_string()
}

fn from_ts(text: &str) -> Result<Timestamp> {
    text.parse()
        .with_context(|| format!("bad stored timestamp {text:?}"))
}

fn job_status(text: &str) -> Result<JobStatus> {
    JobStatus::parse(text).with_context(|| format!("bad stored job status {text:?}"))
}

fn held_reason_text(reason: HeldReason) -> &'static str {
    match reason {
        HeldReason::Interrupted => "interrupted",
        HeldReason::Errored => "errored",
    }
}

fn run_status(text: &str) -> Result<RunStatus> {
    RunStatus::parse(text).with_context(|| format!("bad stored run status {text:?}"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::model::{Action, CapturedEnv, Step};

    fn one_shot_spec(name: Option<&str>, at: Timestamp) -> JobSpec {
        let step = Step {
            action: Action::Shell {
                argv: vec!["/bin/echo".into(), "hi".into()],
            },
            cwd: None,
            env: None,
            timeout: None,
            kill_grace: None,
            transitions: Vec::new(),
            max_visits: None,
            restart_safe: false,
            missed_wait: None,
        };
        JobSpec {
            name: name.map(String::from),
            schedule: Schedule::Once { at },
            graph: Graph {
                entry: "run".into(),
                steps: BTreeMap::from([("run".to_string(), step)]),
            },
            cwd: "/tmp".into(),
            env: CapturedEnv::default(),
            policies: Policies::default(),
            hooks: Hooks::default(),
        }
    }

    #[tokio::test]
    async fn open_creates_and_migrates() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db = dir.path().join("cued.db");
        let store = Store::open(&db).await?;

        // The schema exists and is queryable.
        let tables: Vec<(String,)> =
            sqlx::query_as("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
                .fetch_all(store.pool())
                .await?;
        let names: Vec<&str> = tables.iter().map(|(n,)| n.as_str()).collect();
        for expected in ["jobs", "runs", "step_runs", "notifications"] {
            assert!(
                names.contains(&expected),
                "missing table {expected}: {names:?}"
            );
        }

        // §7.5: the DB file itself is 0600.
        let mode = std::fs::metadata(&db)?.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "db file mode");
        Ok(())
    }

    #[tokio::test]
    async fn submit_begin_finish_round_trip() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(&dir.path().join("cued.db")).await?;
        let now = Timestamp::now();
        let at = now.checked_add(jiff::SignedDuration::from_secs(3600))?;

        let (job, run, stored_at) = store
            .submit_job(&one_shot_spec(Some("t"), at), &now)
            .await?;
        let run = run.expect("one-off creates its run at submit");
        assert_eq!(stored_at, at);

        // The run is parked on the virtual first sleep-edge (§3.2).
        let waiting = store.waiting_runs().await?;
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].job, job);
        assert_eq!(waiting[0].step, "run");
        assert_eq!(waiting[0].at, at);

        let loaded = store.load_job(job).await?;
        assert_eq!(loaded.graph.entry, "run");
        assert_eq!(loaded.status, JobStatus::Active);

        // §3.3 step 1: intent persisted → no longer waiting.
        let attempt = store
            .begin_step(job, run, "run", &now)
            .await?
            .expect("claimed");
        assert_eq!(attempt, 1);
        assert!(store.waiting_runs().await?.is_empty());
        // The CAS guard: a duplicate heap entry can't double-claim.
        assert!(store.begin_step(job, run, "run", &now).await?.is_none());

        // §3.3 step 2: outcome + terminal cursor + job Done, one transaction.
        store
            .finish_step(StepClose {
                job,
                entry_step: "run",
                run,
                step: "run",
                attempt,
                ended_at: &now,
                exit_code: Some(0),
                timed_out: false,
                outcome_edge: None,
                next: NextCursor::Terminal {
                    status: RunStatus::Done,
                    fail_reason: None,
                },
                notifications: Vec::new(),
            })
            .await?;

        let jobs = store.list_overview(false, &now).await?;
        assert_eq!(jobs.len(), 1, "just-ended run stays visible for 24h");
        assert_eq!(jobs[0].status, JobStatus::Done);
        let last = jobs[0].last_run.as_ref().unwrap();
        assert_eq!(last.status, RunStatus::Done);
        assert!(last.ended_at.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn live_name_collision_is_friendly() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(&dir.path().join("cued.db")).await?;
        let now = Timestamp::now();
        let at = now.checked_add(jiff::SignedDuration::from_secs(3600))?;

        store
            .submit_job(&one_shot_spec(Some("dup"), at), &now)
            .await?;
        let err = store
            .submit_job(&one_shot_spec(Some("dup"), at), &now)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
        Ok(())
    }
}
