//! Retention tests (DESIGN.md §10.2): what GC prunes, and — more to the
//! point, since this is the one operation that destroys data — what it must
//! never touch.

use anyhow::Result;
use jiff::{SignedDuration, Timestamp};

use cued::config::Retention;
use cued::daemon::collect_garbage;
use cued::model::{
    CapturedEnv, HeldReason, Hooks, JobId, JobSpec, NotifySpec, Policies, RunId, RunStatus,
    Schedule,
};
use cued::paths::Paths;
use cued::store::{NextCursor, StepClose, Store};
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
    Ok(Harness {
        _dir: dir,
        paths,
        store,
    })
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

fn recurring(anchor: &Timestamp) -> Schedule {
    Schedule::Every {
        interval: SignedDuration::from_secs(60),
        anchor: *anchor,
        until: None,
        count: None,
    }
}

/// Drive one run to a terminal cursor, writing a log file for it so the
/// §2.1 bytes are there to be pruned too.
async fn finish_run(h: &Harness, job: JobId, run: RunId, at: &Timestamp) -> Result<()> {
    let attempt = h
        .store
        .begin_step(job, run, "run", at)
        .await?
        .expect("claim");
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
            next: NextCursor::Terminal {
                status: RunStatus::Done,
                fail_reason: None,
            },
            notifications: Vec::new(),
        })
        .await?;
    Ok(())
}

async fn run_ids(store: &Store, job: JobId) -> Vec<i64> {
    sqlx::query_scalar("SELECT id FROM runs WHERE job_id = ? ORDER BY id")
        .bind(job.0)
        .fetch_all(store.pool())
        .await
        .expect("runs")
}

/// §10.2: "older than retention.days OR beyond retention.runs_per_job,
/// whichever bites first" — a union. The count rule is what bounds a busy
/// recurring job between sweeps; the age rule is what eventually clears a
/// quiet one.
#[tokio::test]
async fn both_retention_rules_bite() -> Result<()> {
    let h = harness().await?;
    let now = Timestamp::now();
    let anchor = now.checked_sub(SignedDuration::from_hours(24 * 90))?;
    let (job, _, _) = h
        .store
        .submit_job(&spec("busy", recurring(&anchor)), &anchor)
        .await?;

    // Ten runs: the oldest five finished 60 days ago, the rest just now.
    for index in 1..=10 {
        let at = if index <= 5 {
            now.checked_sub(SignedDuration::from_hours(24 * 60))?
        } else {
            now
        };
        sqlx::query(
            "INSERT INTO runs (job_id, id, scheduled_for, status, cursor_kind, cursor_step, cursor_at)
             VALUES (?, ?, ?, 'pending', 'waiting', 'run', ?)",
        )
        .bind(job.0)
        .bind(index)
        .bind(at.to_string())
        .bind(at.to_string())
        .execute(h.store.pool())
        .await?;
        finish_run(&h, job, RunId(index), &at).await?;
    }

    // Age alone: 30 days, but keep plenty — only the five old ones go.
    let aged = collect_garbage(
        &h.store,
        &h.paths,
        &Retention {
            days: 30,
            runs_per_job: 100,
        },
        &now,
    )
    .await?;
    assert_eq!(
        aged.runs.len(),
        5,
        "the five 60-day-old runs: {:?}",
        aged.runs
    );
    assert_eq!(run_ids(&h.store, job).await, [6, 7, 8, 9, 10]);

    // Count alone: everything is recent now, but keep only 3.
    let counted = collect_garbage(
        &h.store,
        &h.paths,
        &Retention {
            days: 3650,
            runs_per_job: 3,
        },
        &now,
    )
    .await?;
    assert_eq!(counted.runs.len(), 2, "runs 6 and 7: {:?}", counted.runs);
    assert_eq!(
        run_ids(&h.store, job).await,
        [8, 9, 10],
        "the newest are kept"
    );
    Ok(())
}

/// The bytes have to go with the rows, or "pruned" only means "invisible"
/// and the logs directory grows forever (§2.1).
#[tokio::test]
async fn pruning_a_run_takes_its_logs_with_it() -> Result<()> {
    let h = harness().await?;
    let now = Timestamp::now();
    let old = now.checked_sub(SignedDuration::from_hours(24 * 60))?;
    let (job, run, _) = h
        .store
        .submit_job(&spec("once", Schedule::Once { at: old }), &old)
        .await?;
    let run = run.expect("a one-off gets its run at submit");
    finish_run(&h, job, run, &old).await?;

    let log = h.paths.step_log(job, run, "run", 1);
    assert!(log.exists(), "the test should have written a log");

    collect_garbage(
        &h.store,
        &h.paths,
        &Retention {
            days: 30,
            runs_per_job: 20,
        },
        &now,
    )
    .await?;

    assert!(!log.exists(), "the log file outlived its run");
    assert!(
        !h.paths.run_log_dir(job, run).exists(),
        "the run's log dir outlived it"
    );
    Ok(())
}

/// §10.2: "one-shot jobs whose run is pruned are pruned with it; recurring
/// jobs persist until cancelled." A live recurring job losing its row would
/// mean losing the schedule itself.
#[tokio::test]
async fn a_spent_one_shot_goes_but_a_live_recurring_job_stays() -> Result<()> {
    let h = harness().await?;
    let now = Timestamp::now();
    let old = now.checked_sub(SignedDuration::from_hours(24 * 60))?;

    let (one_shot, run, _) = h
        .store
        .submit_job(&spec("spent", Schedule::Once { at: old }), &old)
        .await?;
    finish_run(&h, one_shot, run.expect("run"), &old).await?;

    // A recurring job whose only run is equally ancient — but the job is
    // still active, so its definition must survive its history.
    let anchor = old;
    let (recurring_job, _, _) = h
        .store
        .submit_job(&spec("living", recurring(&anchor)), &anchor)
        .await?;
    sqlx::query(
        "INSERT INTO runs (job_id, id, scheduled_for, status, cursor_kind, cursor_step, cursor_at)
         VALUES (?, 1, ?, 'pending', 'waiting', 'run', ?)",
    )
    .bind(recurring_job.0)
    .bind(old.to_string())
    .bind(old.to_string())
    .execute(h.store.pool())
    .await?;
    finish_run(&h, recurring_job, RunId(1), &old).await?;

    let outcome = collect_garbage(
        &h.store,
        &h.paths,
        &Retention {
            days: 30,
            runs_per_job: 20,
        },
        &now,
    )
    .await?;

    assert_eq!(outcome.runs.len(), 2, "both ancient runs go");
    assert_eq!(
        outcome.jobs,
        [one_shot],
        "only the spent one-shot: {:?}",
        outcome.jobs
    );

    let surviving: Vec<i64> = sqlx::query_scalar("SELECT id FROM jobs ORDER BY id")
        .fetch_all(h.store.pool())
        .await?;
    assert_eq!(
        surviving,
        [recurring_job.0],
        "the live schedule must survive its history"
    );
    Ok(())
}

/// The protection that matters most. A `Held` run is waiting on a human
/// decision (§3.4) and a live one is still going; neither may be collected
/// out from under its owner, however old it is. Getting this wrong would
/// delete exactly the runs a person was told to come back and look at.
#[tokio::test]
async fn held_and_live_runs_are_never_collected() -> Result<()> {
    let h = harness().await?;
    let now = Timestamp::now();
    let ancient = now.checked_sub(SignedDuration::from_hours(24 * 400))?;
    let (job, _, _) = h
        .store
        .submit_job(&spec("keep", recurring(&ancient)), &ancient)
        .await?;

    // Three ancient runs, one per non-terminal cursor kind — run 1 goes in
    // `running` so `hold_run` below parks it the way reconciliation would.
    for (id, kind) in [(1, "running"), (2, "waiting"), (3, "running")] {
        sqlx::query(
            "INSERT INTO runs (job_id, id, scheduled_for, status, cursor_kind, cursor_step,
                               cursor_at, ended_at)
             VALUES (?, ?, ?, 'pending', ?, 'run', ?, ?)",
        )
        .bind(job.0)
        .bind(id)
        .bind(ancient.to_string())
        .bind(kind)
        .bind(ancient.to_string())
        // Even with an end time far in the past, a non-terminal cursor is
        // not a candidate — the cursor is what decides, not the clock.
        .bind(ancient.to_string())
        .execute(h.store.pool())
        .await?;
    }
    // Parked, with the notification that is the thing the human was told to
    // act on.
    h.store
        .hold_run(
            job,
            RunId(1),
            "run",
            HeldReason::Interrupted,
            &NotifySpec {
                title: "parked".into(),
                body: "look at me".into(),
            },
            &ancient,
            None,
        )
        .await?;

    let outcome = collect_garbage(
        &h.store,
        &h.paths,
        // As aggressive as the policy can be: keep nothing by count, and
        // treat anything over a day old as expired.
        &Retention {
            days: 1,
            runs_per_job: 0,
        },
        &now,
    )
    .await?;

    assert!(
        outcome.runs.is_empty(),
        "a non-terminal run was collected: {:?}",
        outcome.runs
    );
    assert!(
        outcome.jobs.is_empty(),
        "a job with live runs was collected"
    );
    assert_eq!(run_ids(&h.store, job).await, [1, 2, 3]);

    let pending: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM notifications WHERE delivered_at IS NULL")
            .fetch_one(h.store.pool())
            .await?;
    assert_eq!(
        pending, 1,
        "the held run's notification must survive with it"
    );
    Ok(())
}

/// `cued wait --run N` tells "not created yet" from "gone" by the job's
/// highest run id. That must come from the id high-water mark, not the
/// rows left: age-based GC can prune every run a quiet job has, and a
/// pruned run must not look like one still to come.
#[tokio::test]
async fn pruned_runs_still_count_as_created() -> Result<()> {
    use cued::proto::RunQuery;
    let h = harness().await?;
    let now = Timestamp::now();
    let anchor = now.checked_sub(SignedDuration::from_hours(24 * 90))?;
    let (job, _, _) = h
        .store
        .submit_job(&spec("quiet", recurring(&anchor)), &anchor)
        .await?;
    let old = now.checked_sub(SignedDuration::from_hours(24 * 60))?;
    for index in 1..=3 {
        sqlx::query(
            "INSERT INTO runs (job_id, id, scheduled_for, status, cursor_kind, cursor_step, cursor_at)
             VALUES (?, ?, ?, 'pending', 'waiting', 'run', ?)",
        )
        .bind(job.0)
        .bind(index)
        .bind(old.to_string())
        .bind(old.to_string())
        .execute(h.store.pool())
        .await?;
        finish_run(&h, job, RunId(index), &old).await?;
    }
    collect_garbage(
        &h.store,
        &h.paths,
        &Retention {
            days: 30,
            runs_per_job: 100,
        },
        &now,
    )
    .await?;
    assert!(run_ids(&h.store, job).await.is_empty(), "all pruned");

    let (reply, _) = h.store.job_run(job, RunQuery::Exact(2)).await?;
    assert!(reply.run.is_none());
    assert_eq!(reply.last_id, 3, "r2 was created; it's gone, not pending");
    Ok(())
}
