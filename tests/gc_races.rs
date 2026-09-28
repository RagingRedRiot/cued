//! GC ownership races and history-independent pruning (DESIGN.md §10.2).
//!
//! `tests/retention.rs` covers *what* a single, uncontended sweep prunes.
//! These cover what a sweep must never do while the rest of the daemon keeps
//! writing: delete a run that became live again after it was chosen, or
//! reuse a pruned run's id so the log directory removed on its behalf is
//! really a new run's. They also pin the retention rules across the batch
//! boundaries of a sweep that no longer reads the whole history at once.

use std::time::Duration;

use anyhow::Result;
use jiff::{SignedDuration, Timestamp};
use sqlx::Connection;
use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection};

use cued::config::Retention;
use cued::daemon::collect_garbage;
use cued::model::{
    CapturedEnv, Hooks, JobId, JobSpec, Policies, RunId, RunStatus, Schedule,
};
use cued::paths::Paths;
use cued::store::{Fired, Firing, NextCursor, StepClose, Store};
use cued::submit::single_shell_graph;

struct Harness {
    _dir: tempfile::TempDir,
    paths: Paths,
    store: Store,
}

async fn harness() -> Result<Harness> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let paths = Paths {
        data_dir: root.join("data"),
        db_file: root.join("data/cued.db"),
        lock_file: root.join("data/cued.lock"),
        logs_dir: root.join("data/logs"),
        daemon_log: root.join("data/daemon.log"),
        socket_file: root.join("cued.sock"),
        config_file: root.join("config.toml"),
    };
    std::fs::create_dir_all(&paths.logs_dir)?;
    let store = Store::open(&paths.db_file).await?;
    Ok(Harness { _dir: dir, paths, store })
}

fn spec(name: &str, schedule: Schedule) -> JobSpec {
    JobSpec {
        name: Some(name.into()),
        schedule,
        graph: single_shell_graph(vec!["/bin/true".into()]),
        cwd: "/".into(),
        env: CapturedEnv::default(),
        policies: Policies::default(),
        hooks: Hooks::default(),
    }
}

fn every_minute(anchor: &Timestamp) -> Schedule {
    Schedule::Every {
        interval: SignedDuration::from_secs(60),
        anchor: *anchor,
        until: None,
        count: None,
    }
}

/// Claim, log and close one waiting run, the way the daemon would.
async fn finish_run(h: &Harness, job: JobId, run: RunId, at: &Timestamp) -> Result<()> {
    let attempt = h.store.begin_step(job, run, "run", at).await?.expect("claim");
    let log = h.paths.step_log(job, run, "run", attempt);
    std::fs::create_dir_all(log.parent().expect("parent"))?;
    std::fs::write(&log, b"output\n")?;
    h.store
        .finish_step(StepClose {
            job,
            entry_step: "run",
            run,
            step: "run",
            attempt,
            ended_at: at,
            exit_code: Some(0),
            timed_out: false,
            outcome_edge: None,
            next: NextCursor::Terminal { status: RunStatus::Done, fail_reason: None },
            notifications: Vec::new(),
        })
        .await?;
    Ok(())
}

/// Fire a recurring job's stored instant once, creating its next run.
async fn fire_once(h: &Harness, job: JobId, now: &Timestamp) -> Result<RunId> {
    let claiming: String = sqlx::query_scalar("SELECT next_fire_at FROM jobs WHERE id = ?")
        .bind(job.0)
        .fetch_one(h.store.pool())
        .await?;
    let claiming: Timestamp = claiming.parse()?;
    let next = claiming.checked_add(SignedDuration::from_secs(60))?;
    let fired = h
        .store
        .record_firing(Firing {
            job,
            entry_step: "run",
            claiming: &claiming,
            consumed: 1,
            run_at: Some(now),
            skipped: None,
            queue_at: None,
            next_fire_at: Some(&next),
            now,
        })
        .await?;
    match fired {
        Fired::Recorded { run: Some(run), .. } => Ok(run),
        other => anyhow::bail!("firing not recorded: {other:?}"),
    }
}

/// Insert `count` terminal runs directly, ended at `ended(index)`.
async fn seed_done_runs(
    h: &Harness,
    job: JobId,
    ids: std::ops::RangeInclusive<i64>,
    ended: impl Fn(i64) -> Timestamp,
) -> Result<()> {
    let mut tx = h.store.pool().begin().await?;
    for id in ids {
        let at = ended(id).to_string();
        sqlx::query(
            "INSERT INTO runs (job_id, id, scheduled_for, started_at, ended_at, status, cursor_kind)
             VALUES (?, ?, ?, ?, ?, 'done', 'done')",
        )
        .bind(job.0)
        .bind(id)
        .bind(&at)
        .bind(&at)
        .bind(&at)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO step_runs (job_id, run_id, step_id, attempt, started_at, ended_at, exit_code)
             VALUES (?, ?, 'run', 1, ?, ?, 0)",
        )
        .bind(job.0)
        .bind(id)
        .bind(&at)
        .bind(&at)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn run_ids(store: &Store, job: JobId) -> Vec<i64> {
    sqlx::query_scalar("SELECT id FROM runs WHERE job_id = ? ORDER BY id")
        .bind(job.0)
        .fetch_all(store.pool())
        .await
        .expect("runs")
}

/// `cued retry` makes a `done` run live again, in place (§3.4). A sweep that
/// chose that run while it was still `done` must not delete it afterwards —
/// not its row, not its attempts, not its logs.
///
/// The interleaving is forced without a production hook: a second SQLite
/// connection takes the write lock and applies `rewind_run`'s own update
/// *uncommitted*. WAL readers never block, so the sweep reads the committed
/// (still `done`) state and then has to wait for the write lock; the rewind
/// commits while it waits. Every read a sweep does is lock-free, so a sweep
/// that is still running after the settle period is waiting on a write.
#[tokio::test(flavor = "multi_thread")]
async fn a_run_retried_during_a_sweep_is_not_collected() -> Result<()> {
    let h = harness().await?;
    let now = Timestamp::now();
    let old = now.checked_sub(SignedDuration::from_hours(24 * 60))?;
    let (job, run, _) = h
        .store
        .submit_job(&spec("retried", Schedule::Once { at: old }), &old)
        .await?;
    let run = run.expect("a one-off gets its run at submit");
    finish_run(&h, job, run, &old).await?;
    let log_dir = h.paths.run_log_dir(job, run);
    assert!(log_dir.exists());

    let mut rival = SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(&h.paths.db_file),
    )
    .await?;
    sqlx::query("BEGIN IMMEDIATE").execute(&mut rival).await?;
    // Exactly what `Store::rewind_run` commits.
    sqlx::query(
        "UPDATE runs SET status = 'pending', cursor_kind = 'waiting', cursor_step = 'run',
                cursor_at = ?, ended_at = NULL, fail_reason = NULL, held_reason = NULL,
                epoch = epoch + 1
         WHERE job_id = ? AND id = ?",
    )
    .bind(now.to_string())
    .bind(job.0)
    .bind(run.0)
    .execute(&mut rival)
    .await?;
    sqlx::query("UPDATE jobs SET status = 'active' WHERE id = ?")
        .bind(job.0)
        .execute(&mut rival)
        .await?;

    let sweep = {
        let (store, paths) = (h.store.clone(), h.paths.clone());
        tokio::spawn(async move {
            collect_garbage(&store, &paths, &Retention { days: 30, runs_per_job: 20 }, &now)
                .await
        })
    };
    // Well inside the store's 5s busy timeout.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!sweep.is_finished(), "the sweep should be waiting for the write lock");
    sqlx::query("COMMIT").execute(&mut rival).await?;
    let outcome = sweep.await??;

    assert!(outcome.runs.is_empty(), "a live run was collected: {:?}", outcome.runs);
    assert_eq!(run_ids(&h.store, job).await, [run.0], "the retried run's row is gone");
    let attempts: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM step_runs WHERE job_id = ? AND run_id = ?")
            .bind(job.0)
            .bind(run.0)
            .fetch_one(h.store.pool())
            .await?;
    assert_eq!(attempts, 1, "the retried run's attempt history is gone");
    assert!(log_dir.exists(), "the retried run's logs were removed");

    // And the retry still claims: the daemon's next step is `begin_step`.
    assert!(h.store.begin_step(job, run, "run", &now).await?.is_some());
    Ok(())
}

/// Run ids are a per-job sequence (§5.3: `j7.r3`). If pruning every run of
/// a still-active job handed the next firing `r1` again, the id would name
/// two different runs — and `collect_garbage` removes the pruned runs' log
/// directories *after* its commit, so a firing that lands in between gets
/// its fresh logs deleted as the old run's. This drives exactly those
/// three steps in that order.
#[tokio::test]
async fn pruned_run_ids_are_never_reused() -> Result<()> {
    let h = harness().await?;
    let now = Timestamp::now();
    let old = now.checked_sub(SignedDuration::from_hours(24 * 60))?;
    let (job, _, _) = h.store.submit_job(&spec("quiet", every_minute(&old)), &old).await?;
    seed_done_runs(&h, job, 1..=3, |_| old).await?;

    let retention = Retention { days: 30, runs_per_job: 20 };
    let pruned = h.store.gc(&retention, &now).await?;
    assert_eq!(pruned.runs.len(), 3, "all three aged-out runs: {:?}", pruned.runs);

    // A firing between the commit and the directory removal.
    let fresh = fire_once(&h, job, &now).await?;
    let attempt = h.store.begin_step(job, fresh, "run", &now).await?.expect("claim");
    let log = h.paths.step_log(job, fresh, "run", attempt);
    std::fs::create_dir_all(log.parent().expect("parent"))?;
    std::fs::write(&log, b"new output\n")?;

    // `collect_garbage`'s second half, for the rows it just pruned.
    for (job, run) in &pruned.runs {
        let _ = std::fs::remove_dir_all(h.paths.run_log_dir(*job, *run));
    }
    assert!(log.exists(), "the new run's log was removed as a pruned run's ({fresh})");
    assert_eq!(fresh, RunId(4), "a pruned run id was handed out again");
    Ok(())
}

/// The sweep works in batches now, so the count rule has to hold across
/// batch boundaries and for every job, and the union with the age rule has
/// to come out the same as the whole-history pass it replaced.
#[tokio::test]
async fn batched_sweep_keeps_exactly_the_newest_runs_of_every_job() -> Result<()> {
    let h = harness().await?;
    let now = Timestamp::now();
    let anchor = now.checked_sub(SignedDuration::from_hours(24 * 90))?;
    let (busy, _, _) = h.store.submit_job(&spec("busy", every_minute(&anchor)), &anchor).await?;
    let (quiet, _, _) = h.store.submit_job(&spec("quiet", every_minute(&anchor)), &anchor).await?;

    // 2,500 recent runs for one job; a handful of old ones for the other.
    seed_done_runs(&h, busy, 1..=2500, |_| now).await?;
    seed_done_runs(&h, quiet, 1..=5, |_| anchor).await?;
    // A live run in the middle of the busy job's history is never a candidate.
    sqlx::query("UPDATE runs SET cursor_kind = 'held', status = 'held', cursor_step = 'run' WHERE job_id = ? AND id = 7")
        .bind(busy.0)
        .execute(h.store.pool())
        .await?;

    let outcome = collect_garbage(
        &h.store,
        &h.paths,
        &Retention { days: 30, runs_per_job: 10 },
        &now,
    )
    .await?;

    let mut expected: Vec<i64> = vec![7];
    expected.extend(2491..=2500);
    assert_eq!(run_ids(&h.store, busy).await, expected);
    assert!(run_ids(&h.store, quiet).await.is_empty(), "the quiet job's old runs age out");
    assert_eq!(outcome.runs.len(), 2500 - 11 + 5);
    let orphaned: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM step_runs WHERE NOT EXISTS
           (SELECT 1 FROM runs WHERE runs.job_id = step_runs.job_id AND runs.id = step_runs.run_id)",
    )
    .fetch_one(h.store.pool())
    .await?;
    assert_eq!(orphaned, 0, "attempt rows outlived their runs");

    // A second sweep finds nothing left to do.
    let again = h.store.gc(&Retention { days: 30, runs_per_job: 10 }, &now).await?;
    assert!(again.runs.is_empty() && again.jobs.is_empty(), "{again:?}");
    Ok(())
}

/// §10.2's age test compares parsed instants, never text: `to_ts` prints
/// fractional seconds only when non-zero, so within one second text order is
/// not time order ("…:05Z" sorts after "…:05.7Z"). Pin both directions at
/// the cutoff's own second.
#[tokio::test]
async fn the_age_cutoff_is_exact_within_its_second() -> Result<()> {
    let h = harness().await?;
    let now: Timestamp = "2026-09-26T12:00:05.5Z".parse()?;
    let days = 30;
    let cutoff = now.checked_sub(SignedDuration::from_hours(24 * days))?;
    let whole_second: Timestamp = "2026-08-27T12:00:05Z".parse()?;
    let later_fraction: Timestamp = "2026-08-27T12:00:05.7Z".parse()?;
    assert!(whole_second < cutoff && cutoff < later_fraction);

    let anchor = now.checked_sub(SignedDuration::from_hours(24 * 90))?;
    let (job, _, _) = h.store.submit_job(&spec("edge", every_minute(&anchor)), &anchor).await?;
    seed_done_runs(&h, job, 1..=2, |id| if id == 1 { whole_second } else { later_fraction })
        .await?;

    let outcome = h
        .store
        .gc(&Retention { days: days as u32, runs_per_job: 100 }, &now)
        .await?;
    assert_eq!(outcome.runs, [(job, RunId(1))], "only the run that ended before the cutoff");
    assert_eq!(run_ids(&h.store, job).await, [2]);
    Ok(())
}

/// The same guarantee on the path the daemon actually takes: runs created
/// by firings, every one of them pruned (`runs_per_job = 0`), then another
/// firing.
#[tokio::test]
async fn fired_run_ids_stay_unique_after_pruning_everything() -> Result<()> {
    let h = harness().await?;
    let now = Timestamp::now();
    let anchor = now.checked_sub(SignedDuration::from_hours(1))?;
    let (job, _, _) = h.store.submit_job(&spec("drained", every_minute(&anchor)), &anchor).await?;
    for _ in 0..3 {
        let run = fire_once(&h, job, &now).await?;
        finish_run(&h, job, run, &now).await?;
    }
    let pruned =
        collect_garbage(&h.store, &h.paths, &Retention { days: 30, runs_per_job: 0 }, &now).await?;
    assert_eq!(pruned.runs.len(), 3);
    assert!(run_ids(&h.store, job).await.is_empty());
    assert_eq!(fire_once(&h, job, &now).await?, RunId(4));
    Ok(())
}

/// A sweep commits batch by batch. If a later batch fails, the rows the
/// earlier batches deleted are gone for good — so their log directories
/// must still be removed, and the failure still reported.
#[tokio::test]
async fn a_sweep_failing_part_way_still_removes_the_logs_it_pruned() -> Result<()> {
    let h = harness().await?;
    let now = Timestamp::now();
    let old = now.checked_sub(SignedDuration::from_hours(24 * 60))?;
    let (job, _, _) = h.store.submit_job(&spec("partial", every_minute(&old)), &old).await?;
    // Distinct end times give the age walk a known order: id order.
    seed_done_runs(&h, job, 1..=600, |id| {
        old.checked_add(SignedDuration::from_secs(id)).expect("in range")
    })
    .await?;
    for id in [1, 600] {
        std::fs::create_dir_all(h.paths.run_log_dir(job, RunId(id)))?;
    }
    // Disposable database only: refuse the delete of one run in the third
    // batch, as a failing disk might.
    sqlx::query(
        "CREATE TRIGGER refuse_gc BEFORE DELETE ON runs WHEN OLD.id = 550
         BEGIN SELECT RAISE(ABORT, 'injected gc failure'); END",
    )
    .execute(h.store.pool())
    .await?;

    let swept =
        collect_garbage(&h.store, &h.paths, &Retention { days: 30, runs_per_job: 20 }, &now).await;
    let error = swept.expect_err("the injected failure must surface");
    assert!(format!("{error:#}").contains("injected gc failure"), "{error:#}");
    let remaining = run_ids(&h.store, job).await;
    assert_eq!(remaining.first(), Some(&513), "two batches of 256 committed: {remaining:?}");
    assert!(!h.paths.run_log_dir(job, RunId(1)).exists(), "a pruned run's logs leaked");
    assert!(h.paths.run_log_dir(job, RunId(600)).exists(), "an unpruned run's logs went");
    Ok(())
}

/// The daemon can sweep from its daily tick and from `cued gc` at once.
/// Both see the same candidates; each run must be deleted — and reported
/// for log removal — exactly once between them.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_sweeps_split_the_work_without_overlap() -> Result<()> {
    let h = harness().await?;
    let now = Timestamp::now();
    let old = now.checked_sub(SignedDuration::from_hours(24 * 60))?;
    let (job, _, _) = h.store.submit_job(&spec("contended", every_minute(&old)), &old).await?;
    seed_done_runs(&h, job, 1..=3000, |_| old).await?;

    let retention = Retention { days: 30, runs_per_job: 20 };
    let (first, second) = tokio::join!(
        collect_garbage(&h.store, &h.paths, &retention, &now),
        collect_garbage(&h.store, &h.paths, &retention, &now),
    );
    let (first, second) = (first?, second?);
    let mut all: Vec<_> = first.runs.iter().chain(&second.runs).copied().collect();
    all.sort_by_key(|(_, run)| run.0);
    let before = all.len();
    all.dedup();
    assert_eq!(before, all.len(), "a run was reported by both sweeps");
    assert_eq!(all.len(), 3000);
    assert!(run_ids(&h.store, job).await.is_empty());
    Ok(())
}
