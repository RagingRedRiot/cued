//! History-scale measurements (DESIGN.md §10.2) — not a pass/fail test.
//!
//! Seeds a store with a large retained history, then times the per-firing
//! write path, the read paths `list` and `has_live_run`, and GC — both a
//! sweep that prunes nothing (retention wider than the history, like the
//! September soak) and a sweep that prunes almost all of it while firings
//! keep arriving. GC's peak-RSS contribution is measured by resetting the
//! process's peak (`/proc/self/clear_refs`, 5) right before the sweep.
//!
//! Ignored by default. Run explicitly, one size per process:
//!
//! ```sh
//! CUED_SCALE_RUNS_PER_JOB=20000 cargo test --release --test history_scale \
//!     -- --ignored --nocapture
//! ```
//!
//! Prints one JSON object on a line starting `SCALE_RESULT `.

use std::time::{Duration, Instant};

use anyhow::Result;
use jiff::{SignedDuration, Timestamp};

use cued::config::Retention;
use cued::model::{CapturedEnv, Hooks, JobId, JobSpec, Policies, RunStatus, Schedule};
use cued::store::{Fired, Firing, NextCursor, StepClose, Store};
use cued::submit::single_shell_graph;

const JOBS: i64 = 5;

fn spec(name: &str, anchor: &Timestamp) -> JobSpec {
    JobSpec {
        name: Some(name.into()),
        schedule: Schedule::Every {
            interval: SignedDuration::from_secs(1),
            anchor: *anchor,
            until: None,
            count: None,
        },
        graph: single_shell_graph(vec!["/bin/true".into()]),
        cwd: "/".into(),
        env: CapturedEnv::default(),
        policies: Policies::default(),
        hooks: Hooks::default(),
    }
}

fn status_kib(field: &str) -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").expect("status");
    status
        .lines()
        .find_map(|line| line.strip_prefix(field))
        .and_then(|rest| rest.trim().trim_end_matches(" kB").trim().parse().ok())
        .unwrap_or(0)
}

fn reset_peak() {
    // Linux ≥ 4.0: "5" resets VmHWM to the current RSS.
    let _ = std::fs::write("/proc/self/clear_refs", "5");
}

fn summary(mut micros: Vec<u128>) -> serde_json::Value {
    if micros.is_empty() {
        return serde_json::Value::Null;
    }
    micros.sort_unstable();
    let pick = |q: f64| micros[((micros.len() - 1) as f64 * q).round() as usize];
    serde_json::json!({
        "n": micros.len(), "median_us": pick(0.5), "p95_us": pick(0.95),
        "p99_us": pick(0.99), "max_us": micros[micros.len() - 1],
    })
}

/// One firing's worth of store writes: claim the instant, claim the step,
/// close it — what the daemon commits for a one-step recurring run.
async fn fire_cycle(store: &Store, job: JobId, now: &Timestamp) -> Result<u128> {
    let started = Instant::now();
    let claiming: Option<String> = sqlx::query_scalar("SELECT next_fire_at FROM jobs WHERE id = ?")
        .bind(job.0)
        .fetch_one(store.pool())
        .await?;
    let claiming: Timestamp = claiming.expect("armed").parse()?;
    let next = claiming.checked_add(SignedDuration::from_secs(1))?;
    let fired = store
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
    let Fired::Recorded { run: Some(run), .. } = fired else {
        anyhow::bail!("firing not recorded: {fired:?}");
    };
    let attempt = store.begin_step(job, run, "run", now).await?.expect("claim");
    store
        .finish_step(StepClose {
            job,
            entry_step: "run",
            run,
            step: "run",
            attempt,
            ended_at: now,
            exit_code: Some(0),
            timed_out: false,
            outcome_edge: None,
            next: NextCursor::Terminal { status: RunStatus::Done, fail_reason: None },
            notifications: Vec::new(),
        })
        .await?;
    Ok(started.elapsed().as_micros())
}

async fn seed(store: &Store, job: JobId, runs: i64, ended: &Timestamp) -> Result<()> {
    let at = ended.to_string();
    let mut next = 1;
    while next <= runs {
        let mut tx = store.pool().begin().await?;
        for id in next..(next + 5000).min(runs + 1) {
            sqlx::query(
                "INSERT INTO runs (job_id, id, scheduled_for, started_at, ended_at, status, cursor_kind)
                 VALUES (?, ?, ?, ?, ?, 'done', 'done')",
            )
            .bind(job.0).bind(id).bind(&at).bind(&at).bind(&at)
            .execute(&mut *tx).await?;
            sqlx::query(
                "INSERT INTO step_runs (job_id, run_id, step_id, attempt, started_at, ended_at, exit_code, epoch)
                 VALUES (?, ?, 'run', 1, ?, ?, 0, 0)",
            )
            .bind(job.0).bind(id).bind(&at).bind(&at)
            .execute(&mut *tx).await?;
            // Every fourth run carries a delivered notification, as a
            // workflow's notify step or hook would leave behind.
            if id % 4 == 0 {
                sqlx::query(
                    "INSERT INTO notifications (job_id, run_id, title, body, created_at, delivered_at)
                     VALUES (?, ?, 'title', 'body', ?, ?)",
                )
                .bind(job.0).bind(id).bind(&at).bind(&at)
                .execute(&mut *tx).await?;
            }
        }
        tx.commit().await?;
        next += 5000;
    }
    Ok(())
}

async fn plan(store: &Store, sql: &str) -> Result<String> {
    let rows: Vec<(i64, i64, i64, String)> =
        sqlx::query_as(&format!("EXPLAIN QUERY PLAN {sql}")).fetch_all(store.pool()).await?;
    Ok(rows.into_iter().map(|row| row.3).collect::<Vec<_>>().join(" | "))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "measurement; run explicitly with --ignored"]
async fn history_scale() -> Result<()> {
    let per_job: i64 = std::env::var("CUED_SCALE_RUNS_PER_JOB")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(20_000);
    let dir = tempfile::tempdir()?;
    let db = dir.path().join("cued.db");
    let store = Store::open(&db).await?;
    let now = Timestamp::now();
    let anchor = now.checked_sub(SignedDuration::from_hours(1))?;
    let recent = now.checked_sub(SignedDuration::from_secs(30))?;

    let mut jobs = Vec::new();
    for index in 0..JOBS {
        let (job, _, _) = store.submit_job(&spec(&format!("job{index}"), &anchor), &anchor).await?;
        seed(&store, job, per_job, &recent).await?;
        jobs.push(job);
    }
    let seeded_rss = status_kib("VmRSS:");

    let mut cycles = Vec::new();
    for _ in 0..300 {
        cycles.push(fire_cycle(&store, jobs[0], &now).await?);
    }
    let mut live_checks = Vec::new();
    for _ in 0..1000 {
        let started = Instant::now();
        assert!(!store.has_live_run(jobs[1]).await?);
        live_checks.push(started.elapsed().as_micros());
    }
    let mut lists = Vec::new();
    for _ in 0..20 {
        let started = Instant::now();
        store.list_overview(true, &now).await?;
        lists.push(started.elapsed().as_micros());
    }

    // The soak's configuration: retention wider than the history, so the
    // sweep prunes nothing — whatever it costs is pure overhead.
    let retained = Retention { days: 3650, runs_per_job: 1_000_000 };
    reset_peak();
    let before = status_kib("VmRSS:");
    let started = Instant::now();
    let nothing = store.gc(&retained, &now).await?;
    let retained_gc_ms = started.elapsed().as_secs_f64() * 1e3;
    let retained_gc_peak_delta = status_kib("VmHWM:").saturating_sub(before);
    let retained_gc_rss_after = status_kib("VmRSS:");
    assert!(nothing.runs.is_empty());

    // A real prune, with firings for another job arriving throughout: how
    // long does any one firing wait behind the sweep?
    let bounded = Retention { days: 3650, runs_per_job: 100 };
    let firing = {
        let (store, job) = (store.clone(), jobs[2]);
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&stop);
        let handle = tokio::spawn(async move {
            // A cycle that fails (the write queue timing out behind the
            // sweep) is what the daemon would retry and eventually park —
            // count it and how long it waited, rather than stop measuring.
            let (mut waits, mut failures) = (Vec::new(), Vec::new());
            while !flag.load(std::sync::atomic::Ordering::SeqCst) {
                let started = Instant::now();
                match fire_cycle(&store, job, &now).await {
                    Ok(micros) => waits.push(micros),
                    Err(error) => failures.push(serde_json::json!({
                        "after_us": started.elapsed().as_micros(),
                        "error": format!("{error:#}"),
                    })),
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            (waits, failures)
        });
        (stop, handle)
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    reset_peak();
    let before = status_kib("VmRSS:");
    let started = Instant::now();
    let pruned = store.gc(&bounded, &now).await?;
    let bounded_gc_ms = started.elapsed().as_secs_f64() * 1e3;
    let bounded_gc_peak_delta = status_kib("VmHWM:").saturating_sub(before);
    tokio::time::sleep(Duration::from_millis(100)).await;
    firing.0.store(true, std::sync::atomic::Ordering::SeqCst);
    let (concurrent, concurrent_failures) = firing.1.await?;

    let mut small_cycles = Vec::new();
    for _ in 0..300 {
        small_cycles.push(fire_cycle(&store, jobs[0], &now).await?);
    }

    let result = serde_json::json!({
        "runs_per_job": per_job,
        "jobs": JOBS,
        "seeded_rss_kib": seeded_rss,
        "fire_cycle_large_history": summary(cycles),
        "has_live_run": summary(live_checks),
        "list_overview_all": summary(lists),
        "retained_gc_ms": retained_gc_ms,
        "retained_gc_peak_rss_delta_kib": retained_gc_peak_delta,
        "retained_gc_rss_after_kib": retained_gc_rss_after,
        "bounded_gc_ms": bounded_gc_ms,
        "bounded_gc_pruned_runs": pruned.runs.len(),
        "bounded_gc_peak_rss_delta_kib": bounded_gc_peak_delta,
        "fire_cycle_during_bounded_gc": summary(concurrent),
        "fire_cycle_failures_during_bounded_gc": concurrent_failures,
        "fire_cycle_after_prune": summary(small_cycles),
        "plan_live_count": plan(&store, "SELECT COUNT(*) FROM runs WHERE job_id = 1 AND cursor_kind != 'done'").await?,
        "plan_gc_notifications": plan(&store, "DELETE FROM notifications WHERE job_id = 1 AND run_id = 1").await?,
        "plan_retry_live_check": plan(&store, "SELECT id FROM runs WHERE job_id = 1 AND id != 3 AND cursor_kind != 'done' ORDER BY id LIMIT 1").await?,
        "plan_gc_age_walk": plan(&store, "SELECT rowid, job_id, id, cursor_kind, ended_at FROM runs WHERE ended_at < '2026-01-01T00:00:00Z' AND ended_at >= '' AND (ended_at > '' OR rowid > 0) ORDER BY ended_at, rowid LIMIT 256").await?,
        "plan_gc_count_boundary": plan(&store, "SELECT id FROM runs WHERE job_id = 1 ORDER BY id DESC LIMIT 1 OFFSET 100").await?,
        "db_bytes": std::fs::metadata(&db)?.len(),
        "wal_bytes": std::fs::metadata(dir.path().join("cued.db-wal")).map(|m| m.len()).unwrap_or(0),
    });
    println!("SCALE_RESULT {result}");
    Ok(())
}
