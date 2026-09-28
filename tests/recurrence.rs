//! Recurrence tests (DESIGN.md §4): catch_up compaction, overlap policies,
//! the queue slot, count exhaustion, pause/resume — driving `fire_job`
//! directly with controlled instants, the same way the crash-point tests
//! drive `reconcile`.

use anyhow::Result;
use jiff::{SignedDuration, Timestamp};

use cued::daemon::{Arm, fire_job};
use cued::model::{
    CapturedEnv, CatchUp, Hooks, JobId, JobSpec, Overlap, Policies, RunStatus, Schedule,
};
use cued::store::{NextCursor, StepClose, Store};
use cued::submit::single_shell_graph;

fn every_spec(
    name: &str,
    interval_secs: i64,
    anchor: Timestamp,
    catch_up: CatchUp,
    overlap: Overlap,
    count: Option<u32>,
) -> JobSpec {
    JobSpec {
        name: Some(name.into()),
        schedule: Schedule::Every {
            interval: SignedDuration::from_secs(interval_secs),
            anchor,
            until: None,
            count,
        },
        graph: single_shell_graph(vec!["/bin/true".into()]),
        cwd: "/".into(),
        env: CapturedEnv::default(),
        policies: Policies {
            catch_up,
            overlap,
            ..Policies::default()
        },
        hooks: Hooks::default(),
    }
}

async fn temp_store() -> Result<(tempfile::TempDir, Store)> {
    let dir = tempfile::tempdir()?;
    let store = Store::open(&dir.path().join("cued.db")).await?;
    Ok((dir, store))
}

async fn runs_table(store: &Store, job: JobId) -> Vec<(i64, String, Option<i64>)> {
    sqlx::query_as(
        "SELECT id, status, skipped_count FROM runs WHERE job_id = ? ORDER BY id",
    )
    .bind(job.0)
    .fetch_all(store.pool())
    .await
    .expect("runs")
}

async fn job_row(store: &Store, job: JobId) -> (String, Option<String>, Option<String>) {
    sqlx::query_as("SELECT status, next_fire_at, queued_at FROM jobs WHERE id = ?")
        .bind(job.0)
        .fetch_one(store.pool())
        .await
        .expect("job row")
}

/// Close a run's single step cleanly, so it stops being live. Returns what
/// the §4.2 queue released in the same commit, if anything.
async fn complete_run(
    store: &Store,
    job: JobId,
    run: cued::model::RunId,
    now: &Timestamp,
) -> Result<Option<(cued::model::RunId, Timestamp)>> {
    let attempt = store.begin_step(job, run, "run", now).await?.expect("claim");
    let closed = store
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
            next: NextCursor::Terminal {
                status: RunStatus::Done,
                fail_reason: None,
            },
            notifications: Vec::new(),
        })
        .await?;
    Ok(closed.drained)
}

#[tokio::test]
async fn recurring_submit_sets_next_fire_and_no_run() -> Result<()> {
    let (_dir, store) = temp_store().await?;
    let now = Timestamp::now();
    let (job, run, first) = store
        .submit_job(&every_spec("r", 60, now, CatchUp::RunOnce, Overlap::Skip, None), &now)
        .await?;

    assert!(run.is_none(), "recurring jobs get runs firing by firing");
    // Anchor == submit time → first firing one interval later (strictly after).
    assert_eq!(
        first,
        now.checked_add(SignedDuration::from_secs(60))?
    );
    let (status, next, queued) = job_row(&store, job).await;
    assert_eq!(status, "active");
    assert!(next.is_some());
    assert!(queued.is_none());
    assert!(runs_table(&store, job).await.is_empty());
    Ok(())
}

/// §4.2 catch_up = RunOnce: a week of downtime is ONE catch-up run for the
/// most recent instant plus ONE compacted skip row — never a row per miss.
#[tokio::test]
async fn catch_up_run_once_compacts_missed_range() -> Result<()> {
    let (_dir, store) = temp_store().await?;
    let now = Timestamp::now();
    // Submitted 10 minutes ago, every 60s: instants anchor+60 … anchor+600.
    let anchor = now.checked_sub(SignedDuration::from_secs(601))?;
    let (job, _, first) = store
        .submit_job(
            &every_spec("gap", 60, anchor, CatchUp::RunOnce, Overlap::Skip, None),
            &anchor,
        )
        .await?;

    // The daemon pops the armed instant long after it (and 9 more) passed.
    let arms = fire_job(&store, job, &first, &now).await?;

    let rows = runs_table(&store, job).await;
    assert_eq!(rows.len(), 2, "one skip row + one run: {rows:?}");
    let (_, skip_status, skipped_count) = &rows[0];
    assert_eq!(skip_status, "skipped");
    assert_eq!(*skipped_count, Some(9), "instants 1..=9 compacted");
    let (_, run_status, _) = &rows[1];
    assert_eq!(run_status, "pending", "the most recent instant runs");

    // Re-armed at fire time to a future instant, and the run is armed too.
    let (_, next, _) = job_row(&store, job).await;
    assert!(next.is_some());
    assert!(arms.iter().any(|a| matches!(a, Arm::Step(_))));
    assert!(arms.iter().any(|a| matches!(a, Arm::Fire { .. })));
    Ok(())
}

/// §4.2 catch_up = Skip: the whole missed range is one record, no run.
#[tokio::test]
async fn catch_up_skip_records_and_moves_on() -> Result<()> {
    let (_dir, store) = temp_store().await?;
    let now = Timestamp::now();
    let anchor = now.checked_sub(SignedDuration::from_secs(601))?;
    let (job, _, first) = store
        .submit_job(
            &every_spec("skip", 60, anchor, CatchUp::Skip, Overlap::Skip, None),
            &anchor,
        )
        .await?;

    let arms = fire_job(&store, job, &first, &now).await?;

    let rows = runs_table(&store, job).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1, "skipped");
    assert_eq!(rows[0].2, Some(10), "all 10 due instants in one row");
    assert!(
        arms.iter().all(|a| matches!(a, Arm::Fire { .. })),
        "nothing runnable, only the re-arm: {arms:?}"
    );
    Ok(())
}

/// §4.2 overlap: a firing during a live run is Skipped by default; Queue
/// holds exactly one coalesced firing that starts when the run ends.
#[tokio::test]
async fn overlap_skip_records_and_queue_drains_after_the_run() -> Result<()> {
    let (_dir, store) = temp_store().await?;
    let now = Timestamp::now();

    // --- Skip (default): the firing becomes a Skipped row.
    let (skip_job, _, first) = store
        .submit_job(&every_spec("s", 60, now, CatchUp::RunOnce, Overlap::Skip, None), &now)
        .await?;
    fire_job(&store, skip_job, &first, &first).await?; // on time → live run 1
    let second = first.checked_add(SignedDuration::from_secs(60))?;
    fire_job(&store, skip_job, &second, &second).await?; // run 1 still live
    let rows = runs_table(&store, skip_job).await;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].1, "skipped");
    assert_eq!(rows[1].2, Some(1));

    // --- Queue: the firing parks in the slot; further ones coalesce.
    let (queue_job, _, first) = store
        .submit_job(&every_spec("q", 60, now, CatchUp::RunOnce, Overlap::Queue, None), &now)
        .await?;
    fire_job(&store, queue_job, &first, &first).await?; // live run 1
    let second = first.checked_add(SignedDuration::from_secs(60))?;
    let third = first.checked_add(SignedDuration::from_secs(120))?;
    fire_job(&store, queue_job, &second, &second).await?;
    fire_job(&store, queue_job, &third, &third).await?; // coalesces over `second`
    let (_, _, queued) = job_row(&store, queue_job).await;
    assert!(queued.is_some(), "one pending firing held");
    assert_eq!(runs_table(&store, queue_job).await.len(), 1, "no skip rows, no second run");

    // The live run ends → the queued firing becomes run 2, released by the
    // very commit that ended run 1 rather than by a second call after it
    // (codex #2: a failure between the two stranded the queued run).
    let drained = complete_run(&store, queue_job, cued::model::RunId(1), &third).await?;
    let (run, at) = drained.expect("queue drained by the commit that ended the run");
    assert_eq!(run, cued::model::RunId(2));
    assert_eq!(at, third, "coalesced to the LATEST instant");
    let (_, _, queued) = job_row(&store, queue_job).await;
    assert!(queued.is_none());
    // Idempotent: nothing left to drain.
    assert!(store.drain_queued(queue_job, "run").await?.is_none());
    Ok(())
}

/// §4.1: count exhaustion → no re-arm → the job ends Done once nothing is
/// live (here via the pure-skip path, which has no later terminal event).
#[tokio::test]
async fn count_exhaustion_finishes_the_job() -> Result<()> {
    let (_dir, store) = temp_store().await?;
    let now = Timestamp::now();
    let anchor = now.checked_sub(SignedDuration::from_secs(601))?;
    let (job, _, first) = store
        .submit_job(
            &every_spec("cap", 60, anchor, CatchUp::Skip, Overlap::Skip, Some(3)),
            &anchor,
        )
        .await?;

    let arms = fire_job(&store, job, &first, &now).await?;

    let (status, next, _) = job_row(&store, job).await;
    assert_eq!(next, None, "count cap reached → no re-arm");
    assert_eq!(status, "done", "nothing live, nothing scheduled → done");
    assert!(arms.is_empty());
    Ok(())
}

/// §4.2 pause/resume: pause stops re-arming; resume re-arms to the next
/// FUTURE instant only and refuses non-paused jobs.
#[tokio::test]
async fn pause_stops_firing_and_resume_rearms_forward() -> Result<()> {
    let (_dir, store) = temp_store().await?;
    let now = Timestamp::now();
    let (job, _, first) = store
        .submit_job(&every_spec("p", 60, now, CatchUp::RunOnce, Overlap::Skip, None), &now)
        .await?;

    assert!(store.set_resumed(job, None, &now).await.is_err(), "not paused yet");
    store.set_paused(job).await?;
    assert!(store.set_paused(job).await.is_err(), "already paused");

    // A firing that pops while paused does nothing at all.
    let arms = fire_job(&store, job, &first, &first).await?;
    assert!(arms.is_empty());
    assert!(runs_table(&store, job).await.is_empty());

    // Resume never back-fills: the caller hands it the next FUTURE instant.
    let later = now.checked_add(SignedDuration::from_secs(3600))?;
    let next = cued::schedule::next_fire(
        &every_spec("x", 60, now, CatchUp::RunOnce, Overlap::Skip, None).schedule,
        &later,
    )?
    .expect("future instant");
    store.set_resumed(job, Some(&next), &later).await?;
    let (status, next_fire, _) = job_row(&store, job).await;
    assert_eq!(status, "active");
    assert!(next_fire.is_some());
    Ok(())
}

/// An `Every` schedule carrying `until` (§4.1). Separate from `every_spec`
/// because the interaction between a bounded sequence and catch-up is
/// exactly what the two tests below pin down.
fn every_until_spec(name: &str, interval_secs: i64, anchor: Timestamp, until: Timestamp) -> JobSpec {
    JobSpec {
        name: Some(name.into()),
        schedule: Schedule::Every {
            interval: SignedDuration::from_secs(interval_secs),
            anchor,
            until: Some(until),
            count: None,
        },
        graph: single_shell_graph(vec!["/bin/true".into()]),
        cwd: "/".into(),
        env: CapturedEnv::default(),
        policies: Policies::default(),
        hooks: Hooks::default(),
    }
}

/// §4.2 catch-up is closed-form for `Every`, and `until` must not change
/// that. A sub-minute cadence plus real downtime is hundreds of thousands of
/// instants: walking them once tripped the guard, and because the failure
/// happened before the job re-armed, it repeated on every tick and every
/// restart — the job was dead, permanently, over an arithmetic shortcut that
/// had simply been skipped.
#[tokio::test]
async fn every_with_until_survives_a_long_outage() -> Result<()> {
    let (_dir, store) = temp_store().await?;
    let now = Timestamp::now();
    let anchor = now.checked_sub(SignedDuration::from_hours(72))?;
    let until = now.checked_add(SignedDuration::from_hours(24))?;

    // Every second for three days of downtime: ~259,200 missed instants.
    let (job, _, first) = store
        .submit_job(&every_until_spec("dense", 1, anchor, until), &anchor)
        .await?;

    let started = std::time::Instant::now();
    let arms = fire_job(&store, job, &first, &now).await?;
    assert!(
        started.elapsed() < std::time::Duration::from_secs(1),
        "catch-up should be arithmetic, not a walk"
    );

    // §4.2: one compacted skip row plus one catch-up run — never a row each.
    let rows = runs_table(&store, job).await;
    assert_eq!(rows.len(), 2, "one skip row + one run: {rows:?}");
    let (_, skip_status, skipped_count) = &rows[0];
    assert_eq!(skip_status, "skipped");
    assert!(
        skipped_count.is_some_and(|n| n > 250_000),
        "the whole missed range compacts into one row, got {skipped_count:?}"
    );
    assert_eq!(rows[1].1, "pending", "the most recent instant still runs");

    // Still alive: re-armed forward, with the run armed too.
    let (status, next, _) = job_row(&store, job).await;
    assert_eq!(status, "active");
    assert!(next.is_some(), "the job must re-arm, not die on the catch-up");
    assert!(arms.iter().any(|a| matches!(a, Arm::Fire { .. })));
    Ok(())
}

/// The other half of the same change: clipping at `until` is what makes the
/// closed form correct, so instants past `until` must not be counted as
/// missed firings — the sequence ends there.
#[tokio::test]
async fn until_truncates_the_missed_range() -> Result<()> {
    let (_dir, store) = temp_store().await?;
    let now = Timestamp::now();
    // Every 60s from 10 minutes ago, but the schedule ended 5 minutes ago:
    // due instants are anchor+60 … anchor+300, and nothing after.
    let anchor = now.checked_sub(SignedDuration::from_secs(600))?;
    let until = anchor.checked_add(SignedDuration::from_secs(300))?;
    let (job, _, first) = store
        .submit_job(&every_until_spec("bounded", 60, anchor, until), &anchor)
        .await?;

    fire_job(&store, job, &first, &now).await?;

    let rows = runs_table(&store, job).await;
    assert_eq!(rows.len(), 2, "one skip row + one run: {rows:?}");
    // Instants 1..=4 compact; instant 5 (anchor+300, the last before until)
    // is the one that runs. The five minutes after `until` are not firings.
    assert_eq!(rows[0].2, Some(4), "only instants up to `until` count");

    // §4.1: nothing left to schedule once `until` is behind us.
    let (_, next, _) = job_row(&store, job).await;
    assert!(next.is_none(), "an exhausted schedule re-arms to nothing");
    Ok(())
}

/// §4.2 cancel on a recurring job: re-arming stops, the coalesced queue slot
/// is dropped, live runs end `Cancelled`, and a stale heap entry that fires
/// afterwards does nothing. The queue slot matters — it holds a firing that
/// was never a run, so nothing else would ever clear it.
#[tokio::test]
async fn cancel_stops_recurring_and_drops_the_queued_firing() -> Result<()> {
    let (_dir, store) = temp_store().await?;
    let now = Timestamp::now();
    let (job, _, first) = store
        .submit_job(
            &every_spec("q", 60, now, CatchUp::RunOnce, Overlap::Queue, None),
            &now,
        )
        .await?;

    // Fired on time, so the first firing becomes a live run; the second
    // arrives while it's still live and parks in the §4.2 slot.
    fire_job(&store, job, &first, &first).await?;
    let second = first.checked_add(SignedDuration::from_secs(60))?;
    fire_job(&store, job, &second, &second).await?;
    let (_, _, queued) = job_row(&store, job).await;
    assert!(queued.is_some(), "the §4.2 slot should be filled");

    let cancelled = store.cancel_job(job, &second).await?;
    assert_eq!(cancelled.len(), 1, "the live run: {cancelled:?}");

    let (status, next, queued) = job_row(&store, job).await;
    assert_eq!(status, "cancelled");
    assert!(next.is_none(), "cancel stops re-arming");
    assert!(queued.is_none(), "a queued firing must not outlive the job");
    assert!(
        runs_table(&store, job).await.iter().all(|(_, s, _)| s != "pending"),
        "no run should still be live"
    );

    // A heap entry armed before the cancel still fires — and does nothing.
    let third = first.checked_add(SignedDuration::from_secs(120))?;
    let arms = fire_job(&store, job, &third, &third).await?;
    assert!(arms.is_empty(), "a cancelled job must not re-arm: {arms:?}");
    assert_eq!(job_row(&store, job).await.1, None);
    Ok(())
}

/// Cancel is for jobs you can still see in `cued list` (§2). Cancelling one
/// that already ended is a mistake worth naming, not a silent no-op.
#[tokio::test]
async fn cancel_rejects_a_job_that_already_ended() -> Result<()> {
    let (_dir, store) = temp_store().await?;
    let now = Timestamp::now();
    let (job, _, _) = store
        .submit_job(
            &every_spec("once", 60, now, CatchUp::Skip, Overlap::Skip, None),
            &now,
        )
        .await?;

    // Nothing is running, so this cancels the job and names no runs.
    assert!(store.cancel_job(job, &now).await?.is_empty());

    let err = store.cancel_job(job, &now).await.unwrap_err();
    assert!(err.to_string().contains("already ended"), "{err}");
    Ok(())
}

/// §2.1 log manifest: "the latest run" has to mean the latest run that
/// actually *ran* something. A recurring job's newest row is routinely a
/// Skipped or Missed one, which records a firing but has no attempts and no
/// output — resolving to it would answer `cued logs nightly` with silence
/// while the logs the user wants sit one row back.
#[tokio::test]
async fn log_manifest_resolves_past_runs_that_never_ran() -> Result<()> {
    let (_dir, store) = temp_store().await?;
    let now = Timestamp::now();
    let (job, _, first) = store
        .submit_job(
            &every_spec("logs", 60, now, CatchUp::RunOnce, Overlap::Skip, None),
            &now,
        )
        .await?;

    // Firing 1 runs and produces an attempt (complete_run claims it itself).
    fire_job(&store, job, &first, &first).await?;
    complete_run(&store, job, cued::model::RunId(1), &first).await?;

    // Firing 2 arrives while... nothing is live, so it creates run 2 — give
    // it an attempt too, then leave a Skipped row as the newest thing.
    let second = first.checked_add(SignedDuration::from_secs(60))?;
    fire_job(&store, job, &second, &second).await?;
    let run2 = cued::model::RunId(2);
    store.begin_step(job, run2, "run", &second).await?.expect("claim");

    // Run 2 is still live, so firing 3 becomes a Skipped row (overlap=Skip).
    let third = first.checked_add(SignedDuration::from_secs(120))?;
    fire_job(&store, job, &third, &third).await?;
    let rows = runs_table(&store, job).await;
    assert_eq!(rows.last().map(|(_, status, _)| status.as_str()), Some("skipped"));

    // The newest row is the skip; the manifest must land on run 2.
    let (resolved, attempts) = store.log_manifest(job, None, None, None).await?;
    assert_eq!(resolved, run2, "logs resolved to a run that never ran");
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].step, "run");

    // An explicit --run still addresses any row, skip included.
    let (skipped, attempts) = store.log_manifest(job, Some(3), None, None).await?;
    assert_eq!(skipped, cued::model::RunId(3));
    assert!(attempts.is_empty(), "a skipped firing has no attempts");

    // §10.3: addressing a run that doesn't exist is an error, not silence.
    assert!(store.log_manifest(job, Some(99), None, None).await.is_err());
    Ok(())
}

/// codex #2: `cued pause` leaves the job's existing heap entry in place, and
/// `cued resume` adds another for the same instant. Both used to fire, both
/// used to emit the *next* arm, and the duplication doubled every cycle.
///
/// The fix is the claim in `record_firing`: a firing re-arms only if the
/// store still expects exactly that instant, so the second arm finds it
/// already consumed.
#[tokio::test]
async fn two_arms_for_one_instant_fire_once() -> Result<()> {
    let (_dir, store) = temp_store().await?;
    let now = Timestamp::now();
    let (job, _, first) = store
        .submit_job(
            &every_spec("dup", 60, now, CatchUp::RunOnce, Overlap::Skip, None),
            &now,
        )
        .await?;

    // The pause/resume shape: two heap entries carrying the same instant.
    let a = fire_job(&store, job, &first, &first).await?;
    let b = fire_job(&store, job, &first, &first).await?;

    assert!(!a.is_empty(), "the first arm should do the work");
    assert!(
        b.is_empty(),
        "the duplicate arm must produce nothing — it emitted {} arm(s), which is \
         how duplicates compound",
        b.len()
    );

    let rows = runs_table(&store, job).await;
    assert_eq!(rows.len(), 1, "one instant, one run: {rows:?}");

    // And exactly one Fire arm went out, so the next cycle has one entry.
    let fires = a.iter().filter(|arm| matches!(arm, Arm::Fire { .. })).count()
        + b.iter().filter(|arm| matches!(arm, Arm::Fire { .. })).count();
    assert_eq!(fires, 1, "the heap must not grow an entry per duplicate");
    Ok(())
}

/// codex #1a: `fire_job` reads the job's status, then awaits several times
/// before committing. A `cued cancel` landing in *that window* used to be
/// undone — the firing reinstated `next_fire_at` on a cancelled job and left
/// a pending run behind it.
///
/// Driven at `record_firing` rather than through `fire_job`, deliberately:
/// calling `fire_job` after a cancel only exercises its opening status
/// check, which always caught that case. The window this covers is the one
/// *after* that check, and the guard that closes it lives here.
#[tokio::test]
async fn a_cancel_landing_mid_fire_is_not_undone() -> Result<()> {
    let (_dir, store) = temp_store().await?;
    let now = Timestamp::now();
    let (job, _, first) = store
        .submit_job(
            &every_spec("raced", 60, now, CatchUp::RunOnce, Overlap::Skip, None),
            &now,
        )
        .await?;
    let next = first.checked_add(SignedDuration::from_secs(60))?;

    // The cancel commits; the in-flight firing then tries to write.
    store.cancel_job(job, &now).await?;
    let outcome = store
        .record_firing(cued::store::Firing {
            job,
            entry_step: "run",
            claiming: &first,
            consumed: 1,
            run_at: Some(&first),
            skipped: None,
            queue_at: None,
            next_fire_at: Some(&next),
            now: &now,
        })
        .await?;

    assert_eq!(
        outcome,
        cued::store::Fired::Superseded,
        "the firing wrote to a cancelled job"
    );
    let (status, next_at, queued) = job_row(&store, job).await;
    assert_eq!(status, "cancelled");
    assert!(next_at.is_none(), "the firing reinstated next_fire_at on a cancelled job");
    assert!(queued.is_none());
    assert!(
        runs_table(&store, job).await.iter().all(|(_, s, _)| s == "cancelled"),
        "the firing left a live run behind on a cancelled job"
    );

    // A pause is the other half: it leaves next_fire_at alone, so only the
    // status check in the claim can catch it.
    let (paused_job, _, at) = store
        .submit_job(
            &every_spec("held", 60, now, CatchUp::RunOnce, Overlap::Skip, None),
            &now,
        )
        .await?;
    store.set_paused(paused_job).await?;
    let outcome = store
        .record_firing(cued::store::Firing {
            job: paused_job,
            entry_step: "run",
            claiming: &at,
            consumed: 1,
            run_at: Some(&at),
            skipped: None,
            queue_at: None,
            next_fire_at: Some(&next),
            now: &now,
        })
        .await?;
    assert_eq!(
        outcome,
        cued::store::Fired::Superseded,
        "a paused job was fired anyway"
    );
    assert!(runs_table(&store, paused_job).await.is_empty());
    Ok(())
}

/// The same guard must not fire spuriously: a normal, unraced firing still
/// claims its instant and re-arms.
#[tokio::test]
async fn an_ordinary_firing_still_claims_and_rearms() -> Result<()> {
    let (_dir, store) = temp_store().await?;
    let now = Timestamp::now();
    let (job, _, first) = store
        .submit_job(
            &every_spec("plain", 60, now, CatchUp::RunOnce, Overlap::Skip, None),
            &now,
        )
        .await?;

    let mut at = first;
    for expected in 1..=3 {
        let arms = fire_job(&store, job, &at, &at).await?;
        assert_eq!(
            runs_table(&store, job).await.len(),
            expected,
            "firing {expected} did not produce a run"
        );
        // Complete it so the next firing isn't overlap-skipped.
        complete_run(&store, job, cued::model::RunId(expected as i64), &at).await?;
        at = match arms.iter().find_map(|arm| match arm {
            Arm::Fire { at, .. } => Some(*at),
            _ => None,
        }) {
            Some(next) => next,
            None => panic!("firing {expected} did not re-arm"),
        };
    }
    Ok(())
}

/// codex #3: the §4.1 `count` budget was checked *after* the due range was
/// built, so catch-up compacted and ran instants the schedule was never
/// entitled to fire. With `count = 3` and ten instants overdue, cued
/// recorded ten firings and ran the tenth.
#[tokio::test]
async fn catch_up_cannot_spend_more_than_the_count_budget() -> Result<()> {
    let (_dir, store) = temp_store().await?;
    let now = Timestamp::now();
    // Every 60s from ten minutes ago: ten instants due, budget of three.
    let anchor = now.checked_sub(SignedDuration::from_secs(601))?;
    let (job, _, first) = store
        .submit_job(
            &every_spec("capped", 60, anchor, CatchUp::RunOnce, Overlap::Skip, Some(3)),
            &anchor,
        )
        .await?;

    fire_job(&store, job, &first, &now).await?;

    let rows = runs_table(&store, job).await;
    let recorded: i64 = rows.iter().map(|(_, _, skipped)| skipped.unwrap_or(1)).sum();
    assert_eq!(recorded, 3, "the budget was 3 firings, spent {recorded}: {rows:?}");

    // RunOnce still does its job *within* the budget: the earlier instants
    // compact into one skip row and the last of the three runs.
    assert_eq!(rows.len(), 2, "one skip row + one run: {rows:?}");
    assert_eq!(rows[0].1, "skipped");
    assert_eq!(rows[0].2, Some(2), "instants 1-2 compact");
    assert_eq!(rows[1].1, "pending", "instant 3 runs");

    // The instant that ran must be the third, not the tenth.
    let ran: String = sqlx::query_scalar(
        "SELECT scheduled_for FROM runs WHERE job_id = ? AND status = 'pending'",
    )
    .bind(job.0)
    .fetch_one(store.pool())
    .await?;
    let third = anchor.checked_add(SignedDuration::from_secs(180))?;
    assert_eq!(
        ran,
        third.to_string(),
        "ran an instant past the budget"
    );

    // §4.1: budget spent, so nothing is re-armed.
    let (_, next, _) = job_row(&store, job).await;
    assert!(next.is_none(), "an exhausted count must not re-arm");
    Ok(())
}

/// Arriving at a firing with the budget already spent must record nothing.
/// Two mechanisms hold this between them — the exhausted schedule re-arms to
/// None, so `record_firing`'s claim refuses a stale arm, and the zero-budget
/// floor in `fire_job` refuses it again. This asserts the behaviour rather
/// than either mechanism, so it stays true if the division of labour moves.
#[tokio::test]
async fn a_firing_with_no_budget_left_records_nothing() -> Result<()> {
    let (_dir, store) = temp_store().await?;
    let now = Timestamp::now();
    let (job, _, first) = store
        .submit_job(
            &every_spec("spent", 60, now, CatchUp::RunOnce, Overlap::Skip, Some(1)),
            &now,
        )
        .await?;

    // Spend the single firing.
    fire_job(&store, job, &first, &first).await?;
    assert_eq!(runs_table(&store, job).await.len(), 1);

    // A stale arm for a later instant arrives anyway.
    let second = first.checked_add(SignedDuration::from_secs(60))?;
    let arms = fire_job(&store, job, &second, &second).await?;
    assert!(arms.is_empty(), "an exhausted job must not re-arm: {arms:?}");
    assert_eq!(
        runs_table(&store, job).await.len(),
        1,
        "a firing was recorded past the budget"
    );
    Ok(())
}

/// Truncation must not change a job that has no budget at all — the common
/// case, and the one where handing `due_instants` a limit could most easily
/// cut a range that should have been whole.
#[tokio::test]
async fn an_uncapped_schedule_still_catches_up_over_the_whole_range() -> Result<()> {
    let (_dir, store) = temp_store().await?;
    let now = Timestamp::now();
    let anchor = now.checked_sub(SignedDuration::from_secs(601))?;
    let (job, _, first) = store
        .submit_job(
            &every_spec("open", 60, anchor, CatchUp::RunOnce, Overlap::Skip, None),
            &anchor,
        )
        .await?;

    fire_job(&store, job, &first, &now).await?;

    let rows = runs_table(&store, job).await;
    assert_eq!(rows[0].2, Some(9), "all nine earlier instants compact: {rows:?}");
    let (_, next, _) = job_row(&store, job).await;
    assert!(next.is_some(), "an uncapped schedule keeps going");
    Ok(())
}

/// The other two §4.2 paths that end a run without a normal step close, and
/// the one that deliberately doesn't release the queue.
#[tokio::test]
async fn every_terminal_path_releases_the_queue_except_held() -> Result<()> {
    // --- missed_wait = Abandon: abandoned is still ended.
    let (_dir, store) = temp_store().await?;
    let now = Timestamp::now();
    let (job, _, first) = store
        .submit_job(
            &every_spec("abandon", 60, now, CatchUp::RunOnce, Overlap::Queue, None),
            &now,
        )
        .await?;
    fire_job(&store, job, &first, &first).await?;
    let second = first.checked_add(SignedDuration::from_secs(60))?;
    fire_job(&store, job, &second, &second).await?;
    assert!(job_row(&store, job).await.2.is_some(), "slot should be filled");

    // End run 1 the way `missed_wait = Abandon` does.
    // The release now rides the same commit that ends the run — there is no
    // second call to make, and no window where one landed and the other did
    // not (codex #2).
    let drained = store
        .mark_missed(job, cued::model::RunId(1), "run", &second, None)
        .await?;
    assert!(drained.is_some(), "an abandoned run must release the queue");
    assert!(job_row(&store, job).await.2.is_none());

    // --- Held: §4.2 has a held run block the queue indefinitely, because
    // the human decision it is waiting on presumably affects the next run.
    let (_dir2, store) = temp_store().await?;
    let (job, _, first) = store
        .submit_job(
            &every_spec("held", 60, now, CatchUp::RunOnce, Overlap::Queue, None),
            &now,
        )
        .await?;
    fire_job(&store, job, &first, &first).await?;
    let second = first.checked_add(SignedDuration::from_secs(60))?;
    fire_job(&store, job, &second, &second).await?;

    store.begin_step(job, cued::model::RunId(1), "run", &first).await?.expect("claim");
    store
        .hold_run(
            job,
            cued::model::RunId(1),
            "run",
            cued::model::HeldReason::Interrupted,
            &cued::model::NotifySpec { title: "t".into(), body: "b".into() },
            &second,
            None,
        )
        .await?;
    assert!(
        store.drain_queued(job, "run").await?.is_none(),
        "a held run must keep blocking the queue (§4.2)"
    );
    assert!(
        job_row(&store, job).await.2.is_some(),
        "the queued firing stays put until the human decides"
    );
    Ok(())
}

/// codex #4, the startup half: reconcile used to drain the queue *before*
/// resolving interrupted runs, so an `on_interrupt = Fail` that closed the
/// only live run terminally freed a firing nothing was left to notice.
#[tokio::test]
async fn reconcile_drains_after_it_has_finished_ending_runs() -> Result<()> {
    let (_dir, store) = temp_store().await?;
    let now = Timestamp::now();
    let mut spec = every_spec("failing", 60, now, CatchUp::RunOnce, Overlap::Queue, None);
    spec.policies.on_interrupt = cued::model::OnInterrupt::Fail;
    let (job, _, first) = store.submit_job(&spec, &now).await?;

    fire_job(&store, job, &first, &first).await?;
    let second = first.checked_add(SignedDuration::from_secs(60))?;
    fire_job(&store, job, &second, &second).await?;
    assert!(job_row(&store, job).await.2.is_some(), "slot filled");

    // Leave run 1 as a crash leaves it: cursor Running, no daemon.
    store.begin_step(job, cued::model::RunId(1), "run", &first).await?.expect("claim");

    let arms = cued::daemon::reconcile(&store, &second).await?;

    assert!(
        job_row(&store, job).await.2.is_none(),
        "reconcile left a firing queued behind a run it had just failed"
    );
    assert!(
        arms.iter().any(|arm| matches!(arm, Arm::Step(_))),
        "the drained firing should come back as an armed run: {arms:?}"
    );
    Ok(())
}

/// codex #7: `cued retry` could produce two live runs of one job, by a route
/// no policy governs.
///
/// On a recurring job the *newest* row is routinely a `Skipped` one — a
/// firing the overlap policy declined while an older run was still going.
/// "The latest run" (§3.4) pointed at it, `rewind_run` saw a terminal cursor
/// and made it runnable, and the job then had two live runs despite §4.2
/// allowing exactly one.
#[tokio::test]
async fn retry_neither_targets_a_skipped_row_nor_makes_a_second_live_run() -> Result<()> {
    let (_dir, store) = temp_store().await?;
    let now = Timestamp::now();
    let (job, _, first) = store
        .submit_job(
            &every_spec("busy", 60, now, CatchUp::RunOnce, Overlap::Skip, None),
            &now,
        )
        .await?;

    // Firing 1 becomes run 1; firing 2 arrives while it is live and is
    // recorded as a Skipped row — the newest row in the table.
    fire_job(&store, job, &first, &first).await?;
    let second = first.checked_add(SignedDuration::from_secs(60))?;
    fire_job(&store, job, &second, &second).await?;
    let rows = runs_table(&store, job).await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[1].1, "skipped", "the newest row is the skip: {rows:?}");

    // "The latest run" must mean run 1, not the skip record.
    let (target, _, _) = store.latest_run_cursor(job).await?;
    assert_eq!(target, cued::model::RunId(1), "retry aimed at the skipped firing");

    // Run 1 is still live, so retrying it must be refused rather than
    // silently producing a second live run.
    let error = store
        .rewind_run(job, cued::model::RunId(1), "run", &second)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("still live"), "{error}");

    // Even aimed explicitly at the skip row, a rewind can't smuggle a second
    // live run past the policy.
    let error = store
        .rewind_run(job, cued::model::RunId(2), "run", &second)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("still live"), "{error}");
    assert_eq!(
        runs_table(&store, job).await.iter().filter(|(_, s, _)| s == "pending").count(),
        1,
        "a job runs one at a time (§4.2)"
    );
    Ok(())
}

/// The guard must not block the case `retry` exists for: the run finished or
/// failed, nothing else is going, and the user wants it run again.
#[tokio::test]
async fn retry_still_works_once_nothing_is_live() -> Result<()> {
    let (_dir, store) = temp_store().await?;
    let now = Timestamp::now();
    let (job, _, first) = store
        .submit_job(
            &every_spec("done", 60, now, CatchUp::RunOnce, Overlap::Skip, None),
            &now,
        )
        .await?;
    fire_job(&store, job, &first, &first).await?;
    complete_run(&store, job, cued::model::RunId(1), &first).await?;

    // A skip row after it, so targeting has to step past that too.
    let second = first.checked_add(SignedDuration::from_secs(60))?;
    store.begin_step(job, cued::model::RunId(1), "run", &first).await.ok();
    let (target, kind, _) = store.latest_run_cursor(job).await?;
    assert_eq!(target, cued::model::RunId(1));
    assert_eq!(kind, "done");

    store.rewind_run(job, target, "run", &second).await?;
    let rows = runs_table(&store, job).await;
    assert_eq!(rows.len(), 1, "retry rewinds in place — same run, not a new one");
    assert_eq!(rows[0].1, "pending", "the rewound run is runnable again");
    Ok(())
}

/// codex #3, twice over. The §4.1 budget was first derived from *rows*, and
/// a queued firing is not a row; then from rows plus the slot as a boolean,
/// which still missed every firing that overwrote it.
///
/// Swept across caps deliberately. The first attempt at this test used
/// `count = 3` alone — and three is the one value that terminates anyway,
/// because the run and the slot happen to add up to it. A cap of four ran
/// for ever and the test said nothing.
#[tokio::test]
async fn a_queued_firing_spends_the_count_budget() -> Result<()> {
    for cap in [1u32, 2, 3, 4, 5, 9] {
        let (_dir, store) = temp_store().await?;
        let now = Timestamp::now();
        let (job, _, first) = store
            .submit_job(
                &every_spec("q", 60, now, CatchUp::RunOnce, Overlap::Queue, Some(cap)),
                &now,
            )
            .await?;

        // Never complete run 1, so firing 2 onward can only queue — and
        // each later one overwrites the same slot.
        let mut at = first;
        let mut firings = 0;
        for _ in 0..40 {
            let arms = fire_job(&store, job, &at, &at).await?;
            firings += 1;
            match arms.iter().find_map(|arm| match arm {
                Arm::Fire { at, .. } => Some(*at),
                _ => None,
            }) {
                Some(next) => at = next,
                None => break,
            }
        }
        assert_eq!(firings, cap, "cap {cap} spent {firings} firings");
        assert_eq!(
            store.fired_count(job).await?,
            cap,
            "the durable counter should read exactly the budget"
        );
        let (_, next, _) = job_row(&store, job).await;
        assert!(next.is_none(), "cap {cap} kept re-arming after the budget");
    }
    Ok(())
}

/// The counter has to survive §10.2 pruning its history, or a swept job gets
/// its budget back. This is the second way rows were the wrong shape for it.
#[tokio::test]
async fn the_firing_budget_survives_garbage_collection() -> Result<()> {
    let (_dir, store) = temp_store().await?;
    let now = Timestamp::now();
    let (job, _, first) = store
        .submit_job(
            &every_spec("swept", 60, now, CatchUp::RunOnce, Overlap::Skip, Some(2)),
            &now,
        )
        .await?;

    fire_job(&store, job, &first, &first).await?;
    complete_run(&store, job, cued::model::RunId(1), &first).await?;
    assert_eq!(store.fired_count(job).await?, 1);

    // Prune everything the count could have been derived from — through
    // §10.2's own sweep, not a hand-rolled delete, so this is the real path.
    let swept = store
        .gc(
            &cued::config::Retention { days: 3650, runs_per_job: 0 },
            &first.checked_add(SignedDuration::from_secs(60))?,
        )
        .await?;
    assert!(!swept.runs.is_empty(), "the sweep should have pruned the run");

    assert_eq!(
        store.fired_count(job).await?,
        1,
        "the budget was refunded when its history was pruned"
    );
    Ok(())
}

/// codex #6: pausing left the queue slot filled. When the live run ended, the
/// status-agnostic drain minted a run for that past instant — which could not
/// execute while paused, but `resume` re-arms every waiting run, so it ran
/// then. "Resume never back-fills" (§4.2) has to cover the queue too.
#[tokio::test]
async fn pausing_freezes_the_queue_and_resuming_drops_it() -> Result<()> {
    let (_dir, store) = temp_store().await?;
    let now = Timestamp::now();
    let (job, _, first) = store
        .submit_job(
            &every_spec("p", 60, now, CatchUp::RunOnce, Overlap::Queue, None),
            &now,
        )
        .await?;

    fire_job(&store, job, &first, &first).await?;
    let second = first.checked_add(SignedDuration::from_secs(60))?;
    fire_job(&store, job, &second, &second).await?;
    assert!(job_row(&store, job).await.2.is_some(), "slot filled");

    // Claim while active: pause lets an already-running step finish, but the
    // durable start gate correctly rejects a new claim after the pause.
    let run = cued::model::RunId(1);
    let attempt = store.begin_step(job, run, "run", &second).await?.expect("claim");
    store.set_paused(job).await?;

    // Ending the live run must NOT mint a run from the slot while paused.
    let drained = store.finish_step(StepClose {
        job, entry_step: "run", run, step: "run", attempt, ended_at: &second,
        exit_code: Some(0), timed_out: false, outcome_edge: None,
        next: NextCursor::Terminal { status: RunStatus::Done, fail_reason: None },
        notifications: Vec::new(),
    }).await?.drained;
    assert!(drained.is_none(), "a paused job drained its queue anyway");
    assert!(
        job_row(&store, job).await.2.is_some(),
        "the firing should stay in the slot, not become a run"
    );

    // And resuming discards it rather than running a past occurrence.
    let ahead = second.checked_add(SignedDuration::from_secs(600))?;
    store.set_resumed(job, Some(&ahead), &second).await?;
    assert!(
        job_row(&store, job).await.2.is_none(),
        "resume back-filled the queued firing"
    );
    assert!(
        runs_table(&store, job).await.iter().all(|(_, s, _)| s != "pending"),
        "no past occurrence should be waiting to run after a resume"
    );
    Ok(())
}
