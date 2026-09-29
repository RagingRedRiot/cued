//! Crash-point tests (DESIGN.md §11): leave the store exactly as a killed
//! daemon would — intent committed, outcome not — then run reconciliation
//! against the same SQLite file and assert it lands in the safe state:
//! Held not lost, never a double-spawn, policies honored.
//!
//! `begin_step` followed by nothing IS the crash simulation: it commits
//! `cursor = Running` + the StepRun row, which is the §3.3 step-1 state a
//! real kill leaves behind.

use std::collections::BTreeMap;

use anyhow::Result;
use jiff::{SignedDuration, Timestamp};

use cued::daemon::{Arm, reconcile};
use cued::model::{
    Action, CapturedEnv, Condition, Effect, Graph, HeldReason, Hooks, JobId, JobSpec, NotifySpec,
    OnInterrupt, Policies, RunId, RunStatus, Schedule, Step, Transition,
};
use cued::store::{Claim, DueStep, NextCursor, StepClose, Store};

/// The Step half of an arm list (reconcile also returns recurring Fire arms).
fn step_arms(arms: &[Arm]) -> Vec<&DueStep> {
    arms.iter()
        .filter_map(|arm| match arm {
            Arm::Step(due) | Arm::RetryStep { due, .. } => Some(due),
            Arm::Fire { .. } | Arm::RetryFire { .. } => None,
        })
        .collect()
}

async fn submit_one(store: &Store, spec: &JobSpec, now: &Timestamp) -> Result<(JobId, RunId)> {
    let (job, run, _) = store.submit_job(spec, now).await?;
    Ok((job, run.expect("a one-off creates its run at submit")))
}

async fn claim(store: &Store, job: JobId, run: RunId, step: &str, now: &Timestamp) -> Result<u32> {
    Ok(store
        .begin_step(job, run, step, now)
        .await?
        .expect("cursor was waiting on this step"))
}

fn shell_step(transitions: Vec<Transition>, restart_safe: bool) -> Step {
    Step {
        action: Action::Shell {
            argv: vec!["/bin/true".into()],
        },
        cwd: None,
        env: None,
        timeout: None,
        kill_grace: None,
        transitions,
        max_visits: None,
        restart_safe,
        missed_wait: None,
    }
}

fn one_step_spec(
    name: &str,
    at: Timestamp,
    on_interrupt: OnInterrupt,
    restart_safe: bool,
) -> JobSpec {
    let graph = Graph {
        entry: "main".into(),
        steps: BTreeMap::from([("main".to_string(), shell_step(Vec::new(), restart_safe))]),
    };
    spec_with(name, at, graph, on_interrupt)
}

fn spec_with(name: &str, at: Timestamp, graph: Graph, on_interrupt: OnInterrupt) -> JobSpec {
    JobSpec {
        name: Some(name.into()),
        schedule: Schedule::Once { at },
        graph,
        cwd: "/".into(),
        env: CapturedEnv::default(),
        policies: Policies {
            on_interrupt,
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

async fn run_status(store: &Store, job: JobId) -> String {
    sqlx::query_scalar("SELECT status FROM runs WHERE job_id = ? AND id = 1")
        .bind(job.0)
        .fetch_one(store.pool())
        .await
        .expect("run row")
}

async fn notification_titles(store: &Store, job: JobId) -> Vec<String> {
    sqlx::query_scalar("SELECT title FROM notifications WHERE job_id = ? ORDER BY id")
        .bind(job.0)
        .fetch_all(store.pool())
        .await
        .expect("notifications")
}

/// Case 2 default: killed mid-step → Held + on_hold notification, and
/// nothing re-armed (never a blind re-run of unknown side effects).
#[tokio::test]
async fn interrupted_running_holds_by_default() -> Result<()> {
    let (_dir, store) = temp_store().await?;
    let now = Timestamp::now();
    let (job, run) = submit_one(
        &store,
        &one_step_spec("held", now, OnInterrupt::Hold, false),
        &now,
    )
    .await?;
    claim(&store, job, run, "main", &now).await?; // crash here

    let due = reconcile(&store, &now).await?;

    assert!(due.is_empty(), "a held run must not be re-armed: {due:?}");
    assert_eq!(run_status(&store, job).await, "held");
    let titles = notification_titles(&store, job).await;
    assert_eq!(titles.len(), 1, "exactly one on_hold notification");
    assert!(titles[0].contains("paused"), "{titles:?}");

    // Reconciling again is idempotent: the cursor is Held now, not Running.
    let again = reconcile(&store, &now).await?;
    assert!(again.is_empty());
    assert_eq!(notification_titles(&store, job).await.len(), 1);
    Ok(())
}

/// Case 2, on_interrupt = Fail, no recovery edge: fail-fast — the run ends
/// Failed and the one-shot's job is Done.
#[tokio::test]
async fn interrupted_with_fail_policy_fails_fast() -> Result<()> {
    let (_dir, store) = temp_store().await?;
    let now = Timestamp::now();
    let (job, run) = submit_one(
        &store,
        &one_step_spec("ff", now, OnInterrupt::Fail, false),
        &now,
    )
    .await?;
    claim(&store, job, run, "main", &now).await?;

    let due = reconcile(&store, &now).await?;

    assert!(due.is_empty());
    assert_eq!(run_status(&store, job).await, "failed");
    let job_status: String = sqlx::query_scalar("SELECT status FROM jobs WHERE id = ?")
        .bind(job.0)
        .fetch_one(store.pool())
        .await?;
    assert_eq!(job_status, "done");
    // The killed attempt was closed: signal-style, no exit code.
    let (ended, exit): (Option<String>, Option<i32>) = sqlx::query_as(
        "SELECT ended_at, exit_code FROM step_runs WHERE job_id = ? AND attempt = 1",
    )
    .bind(job.0)
    .fetch_one(store.pool())
    .await?;
    assert!(ended.is_some());
    assert_eq!(exit, None);
    Ok(())
}

/// Case 2, on_interrupt = Fail with a `when Failed → Goto recovery` edge:
/// the kill is routed through the step's own failure handling.
#[tokio::test]
async fn interrupted_with_fail_policy_routes_recovery_edge() -> Result<()> {
    let (_dir, store) = temp_store().await?;
    let now = Timestamp::now();
    let graph = Graph {
        entry: "main".into(),
        steps: BTreeMap::from([
            (
                "main".to_string(),
                shell_step(
                    vec![Transition {
                        when: Condition::Failed,
                        then: Effect::Goto {
                            step: "recovery".into(),
                            after: None,
                        },
                    }],
                    false,
                ),
            ),
            ("recovery".to_string(), shell_step(Vec::new(), false)),
        ]),
    };
    let (job, run) = submit_one(
        &store,
        &spec_with("routed", now, graph, OnInterrupt::Fail),
        &now,
    )
    .await?;
    claim(&store, job, run, "main", &now).await?;

    let due = reconcile(&store, &now).await?;

    let steps = step_arms(&due);
    assert_eq!(steps.len(), 1, "recovery step re-armed: {due:?}");
    assert_eq!(steps[0].step, "recovery");
    assert_eq!(run_status(&store, job).await, "waiting");
    Ok(())
}

/// Case 2, on_interrupt = Retry: gated per step by restart_safe — opted-in
/// steps re-arm, everything else degrades to Hold.
#[tokio::test]
async fn interrupted_retry_is_gated_by_restart_safe() -> Result<()> {
    let (_dir, store) = temp_store().await?;
    let now = Timestamp::now();

    let (unsafe_job, unsafe_run) = submit_one(
        &store,
        &one_step_spec("no-optin", now, OnInterrupt::Retry, false),
        &now,
    )
    .await?;
    claim(&store, unsafe_job, unsafe_run, "main", &now).await?;

    let (safe_job, safe_run) = submit_one(
        &store,
        &one_step_spec("optin", now, OnInterrupt::Retry, true),
        &now,
    )
    .await?;
    claim(&store, safe_job, safe_run, "main", &now).await?;

    let due = reconcile(&store, &now).await?;

    // Not restart_safe → Hold, the §3.4 gate.
    assert_eq!(run_status(&store, unsafe_job).await, "held");
    assert_eq!(notification_titles(&store, unsafe_job).await.len(), 1);
    // restart_safe → re-armed to run now; attempts will append.
    assert_eq!(run_status(&store, safe_job).await, "waiting");
    let rearmed: Vec<_> = step_arms(&due)
        .into_iter()
        .filter(|d| d.job == safe_job)
        .collect();
    assert_eq!(rearmed.len(), 1);
    assert_eq!(rearmed[0].step, "main");
    let next_attempt = claim(&store, safe_job, safe_run, "main", &now).await?;
    assert_eq!(next_attempt, 2, "attempt numbers keep appending");
    Ok(())
}

/// Case 1: waiting cursors — future or overdue — are always re-armed; the
/// missed_wait policy applies at fire time, not here.
#[tokio::test]
async fn waiting_cursors_rearm_at_their_frozen_targets() -> Result<()> {
    let (_dir, store) = temp_store().await?;
    let now = Timestamp::now();
    let future = now.checked_add(SignedDuration::from_secs(3600))?;
    let (job, run) = submit_one(
        &store,
        &one_step_spec("later", future, OnInterrupt::Hold, false),
        &now,
    )
    .await?;

    let due = reconcile(&store, &now).await?;

    let steps = step_arms(&due);
    assert_eq!(steps.len(), 1);
    assert_eq!((steps[0].job, steps[0].run), (job, run));
    assert_eq!(steps[0].at, future, "frozen target, not now");
    Ok(())
}

/// Manual control: `continue` resumes a Held run from its parked step —
/// and only a Held one.
#[tokio::test]
async fn continue_resumes_held_runs_only() -> Result<()> {
    let (_dir, store) = temp_store().await?;
    let now = Timestamp::now();
    let (job, run) = submit_one(
        &store,
        &one_step_spec("stuck", now, OnInterrupt::Hold, false),
        &now,
    )
    .await?;

    // Not held yet → refused.
    assert!(store.resume_held(job, run, &now).await.is_err());

    claim(&store, job, run, "main", &now).await?;
    reconcile(&store, &now).await?;
    assert_eq!(run_status(&store, job).await, "held");

    let step = store.resume_held(job, run, &now).await?;
    assert_eq!(step, "main");
    assert_eq!(run_status(&store, job).await, "waiting");
    Ok(())
}

/// Manual control: `retry` rewinds in place — same run id, attempts keep
/// appending, visit counters reset (the epoch), a Done one-shot's job
/// comes back to life.
#[tokio::test]
async fn retry_rewinds_in_place_with_visit_reset() -> Result<()> {
    let (_dir, store) = temp_store().await?;
    let now = Timestamp::now();
    let (job, run) = submit_one(
        &store,
        &one_step_spec("redo", now, OnInterrupt::Hold, false),
        &now,
    )
    .await?;

    // A live run can't be retried.
    assert!(store.rewind_run(job, run, "main", &now).await.is_err());

    // Run it to a terminal state.
    let attempt = claim(&store, job, run, "main", &now).await?;
    store
        .finish_step(cued::store::StepClose {
            job,
            entry_step: "run",
            run,
            step: "main",
            attempt,
            ended_at: &now,
            exit_code: Some(1),
            timed_out: false,
            outcome_edge: None,
            next: cued::store::NextCursor::Terminal {
                status: cued::model::RunStatus::Failed,
                fail_reason: None,
            },
            notifications: Vec::new(),
        })
        .await?;
    assert_eq!(store.visit_count(job, run, "main").await?, 1);

    store.rewind_run(job, run, "main", &now).await?;

    assert_eq!(run_status(&store, job).await, "pending");
    let job_status: String = sqlx::query_scalar("SELECT status FROM jobs WHERE id = ?")
        .bind(job.0)
        .fetch_one(store.pool())
        .await?;
    assert_eq!(job_status, "active", "retried one-shot's job is live again");
    // Epoch bumped: the visit counter is back to zero…
    assert_eq!(store.visit_count(job, run, "main").await?, 0);
    // …but attempts keep appending across the rewind.
    let next_attempt = claim(&store, job, run, "main", &now).await?;
    assert_eq!(next_attempt, 2);
    assert_eq!(store.visit_count(job, run, "main").await?, 1);
    Ok(())
}

/// §2 addressing: id ("j1" / "1") or live name; terminal jobs by id only.
#[tokio::test]
async fn resolve_job_accepts_id_or_live_name() -> Result<()> {
    let (_dir, store) = temp_store().await?;
    let now = Timestamp::now();
    let (job, _) = submit_one(
        &store,
        &one_step_spec("backup", now, OnInterrupt::Hold, false),
        &now,
    )
    .await?;

    assert_eq!(store.resolve_job(&format!("j{}", job.0)).await?, job);
    assert_eq!(store.resolve_job(&job.0.to_string()).await?, job);
    assert_eq!(store.resolve_job("backup").await?, job);
    assert!(store.resolve_job("j999").await.is_err());
    assert!(store.resolve_job("nonesuch").await.is_err());
    Ok(())
}

/// codex #1b: `begin_step` commits `Running` to the store, and only then is
/// the attempt registered in §2.3's live-execution registry. Between those
/// two acts a `cued cancel` finds no handle to signal, returns having told
/// the user the run was terminated — and the process starts anyway.
///
/// The window is microseconds wide and cannot be produced through the public
/// path, so this drives the guard that closes it. `claim_is_current` is asked
/// once more *after* registering: a cancel that landed in the gap has already
/// moved the cursor, and one landing later has a handle to reach.
///
/// Note what this does and doesn't cover. It pins the guard's *semantics* —
/// which is the subtle half. It cannot pin the fact that `try_run_step`
/// still calls it: forcing the window open needs a seam the daemon doesn't
/// have, and a test that raced for it would be flaky, which is worse than
/// none. Deleting the call site would leave this suite green.
#[tokio::test]
async fn a_cancel_during_the_claim_window_stops_the_spawn() -> Result<()> {
    let (_dir, store) = temp_store().await?;
    let now = Timestamp::now();
    let spec = one_step_spec("raced", now, OnInterrupt::Hold, false);
    let (job, run) = submit_one(&store, &spec, &now).await?;

    // The claim commits — this is the daemon about to spawn.
    let attempt = store
        .begin_step(job, run, "main", &now)
        .await?
        .expect("claim");
    assert_eq!(attempt, 1);
    assert!(
        store
            .claim_is_current(
                job,
                run,
                Claim {
                    step: "main",
                    attempt: 1
                }
            )
            .await?,
        "the step it just claimed must be current"
    );

    // The cancel lands in the window, before the handle is registered.
    store.cancel_job(job, &now).await?;

    assert!(
        !store
            .claim_is_current(
                job,
                run,
                Claim {
                    step: "main",
                    attempt: 1
                }
            )
            .await?,
        "a cancelled run must not go on to spawn a process"
    );
    Ok(())
}

/// The same guard must not fire spuriously — it sits on the path every Shell
/// step takes, so a false negative would mean steps silently not running.
#[tokio::test]
async fn the_spawn_guard_passes_an_ordinary_claim_and_rejects_a_stale_one() -> Result<()> {
    let (_dir, store) = temp_store().await?;
    let now = Timestamp::now();
    let spec = one_step_spec("plain", now, OnInterrupt::Hold, false);
    let (job, run) = submit_one(&store, &spec, &now).await?;

    // Not yet claimed: the cursor is Waiting, not Running.
    assert!(
        !store
            .claim_is_current(
                job,
                run,
                Claim {
                    step: "main",
                    attempt: 1
                }
            )
            .await?
    );

    store
        .begin_step(job, run, "main", &now)
        .await?
        .expect("claim");
    assert!(
        store
            .claim_is_current(
                job,
                run,
                Claim {
                    step: "main",
                    attempt: 1
                }
            )
            .await?
    );

    // A different step of the same run is not this attempt's business.
    assert!(
        !store
            .claim_is_current(
                job,
                run,
                Claim {
                    step: "other",
                    attempt: 1
                }
            )
            .await?
    );

    // And once the step closes, the cursor has moved on.
    store
        .finish_step(StepClose {
            job,
            entry_step: "run",
            run,
            step: "main",
            attempt: 1,
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
    assert!(
        !store
            .claim_is_current(
                job,
                run,
                Claim {
                    step: "main",
                    attempt: 1
                }
            )
            .await?
    );
    Ok(())
}

/// codex #5: `cued logs -f` hung forever on a parked job.
///
/// Two meanings had collided on one NULL. §3.4 leaves a killed attempt's row
/// open on purpose — its fate is unknown and the record should say so — while
/// the manifest read an absent `ended_at` as "still running". `-f` latched
/// onto the interrupted attempt and waited for an end time that would never
/// be written.
///
/// The cursor is what separates them, because it is what the daemon acts on.
#[tokio::test]
async fn a_held_attempt_is_interrupted_not_running() -> Result<()> {
    let (_dir, store) = temp_store().await?;
    let now = Timestamp::now();
    let spec = one_step_spec("parked", now, OnInterrupt::Hold, false);
    let (job, run) = submit_one(&store, &spec, &now).await?;

    // Executing: the cursor is on this step, the row is open.
    store
        .begin_step(job, run, "main", &now)
        .await?
        .expect("claim");
    let (_, attempts) = store.log_manifest(job, None, None, None).await?;
    assert!(
        attempts[0].running,
        "an executing attempt must read as running"
    );
    assert!(attempts[0].ended_at.is_none());

    // Interrupted: the row is *still* open — that is deliberate — but the
    // cursor has moved to Held, so nothing is executing.
    store
        .hold_run(
            job,
            run,
            "main",
            HeldReason::Interrupted,
            &NotifySpec {
                title: "t".into(),
                body: "b".into(),
            },
            &now,
            None,
        )
        .await?;
    let (_, attempts) = store.log_manifest(job, None, None, None).await?;
    assert!(
        attempts[0].ended_at.is_none(),
        "§3.4 keeps the killed attempt's row open — that is the record of \
         unknown fate and must not be papered over"
    );
    assert!(
        !attempts[0].running,
        "a parked attempt is not running; `-f` would wait on it forever"
    );

    // Resuming puts the cursor back on a sleep-edge, so still nothing runs
    // until the step is claimed again.
    store.resume_held(job, run, &now).await?;
    let (_, attempts) = store.log_manifest(job, None, None, None).await?;
    assert!(
        !attempts[0].running,
        "a resumed-but-unclaimed run executes nothing"
    );

    // And a finished attempt is neither.
    store
        .begin_step(job, run, "main", &now)
        .await?
        .expect("re-claim");
    store
        .finish_step(StepClose {
            job,
            entry_step: "run",
            run,
            step: "main",
            attempt: 2,
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
    let (_, attempts) = store.log_manifest(job, None, None, None).await?;
    assert!(
        attempts.iter().all(|a| !a.running),
        "nothing runs in a finished run"
    );
    assert!(
        attempts[1].ended_at.is_some(),
        "a completed attempt has an end time"
    );
    Ok(())
}

/// Set up the sequence Astra's campaign reproduced: attempt 1 is claimed and
/// its post-claim write is still pending (its store was failing), the run is
/// cancelled and manually retried, and attempt 2 is now live on the same run
/// and step. Returns both attempt numbers.
async fn stale_and_live_attempt(
    store: &Store,
    job: JobId,
    run: RunId,
    now: &Timestamp,
) -> Result<(u32, u32)> {
    let stale = claim(store, job, run, "main", now).await?;
    // `cued cancel`, then `cued retry`: same run id, same step, new epoch.
    store.cancel_job(job, now).await?;
    store.rewind_run(job, run, "main", now).await?;
    let live = claim(store, job, run, "main", now).await?;
    assert!(live > stale, "a manual retry must mint a newer attempt");
    Ok((stale, live))
}

async fn run_state(store: &Store, job: JobId, run: RunId) -> (String, String) {
    sqlx::query_as("SELECT cursor_kind, status FROM runs WHERE job_id = ? AND id = ?")
        .bind(job.0)
        .bind(run.0)
        .fetch_one(store.pool())
        .await
        .expect("run row")
}

/// §3.3/§3.4: a completion write that was held up behind a failing store must
/// not end a *newer* attempt's run. `cued retry` reuses the run id and the
/// step name and only bumps the epoch and the attempt, so "the cursor is
/// running this step" is not proof of ownership — the attempt number is.
#[tokio::test]
async fn a_stale_close_cannot_end_a_newer_attempt() -> Result<()> {
    let (_dir, store) = temp_store().await?;
    let now = Timestamp::now();
    let spec = one_step_spec("owned", now, OnInterrupt::Hold, false);
    let (job, run) = submit_one(&store, &spec, &now).await?;
    let (stale, live) = stale_and_live_attempt(&store, job, run, &now).await?;

    let close = |attempt: u32| StepClose {
        job,
        entry_step: "main",
        run,
        step: "main",
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
    };

    let closed = store.finish_step(close(stale)).await?;
    assert!(
        !closed.advanced,
        "the stale close must not move the newer attempt's run"
    );
    assert_eq!(
        run_state(&store, job, run).await,
        ("running".to_string(), "running".to_string()),
        "the live attempt's run must still be running"
    );
    // The stale attempt's own record is still written: it really did run and
    // really did end, whoever owns the cursor now.
    let ended: Option<String> = sqlx::query_scalar(
        "SELECT ended_at FROM step_runs WHERE job_id = ? AND run_id = ? AND attempt = ?",
    )
    .bind(job.0)
    .bind(run.0)
    .bind(stale as i64)
    .fetch_one(store.pool())
    .await?;
    assert!(
        ended.is_some(),
        "the stale attempt's audit row must still be closed"
    );

    // And the live attempt still closes normally.
    assert!(store.finish_step(close(live)).await?.advanced);
    assert_eq!(run_state(&store, job, run).await.0, "done");
    Ok(())
}

/// The same ownership rule for the park: an attempt whose retries ran out
/// must not put a newer, *executing* attempt into Held.
#[tokio::test]
async fn a_stale_park_cannot_hold_a_newer_attempt() -> Result<()> {
    let (_dir, store) = temp_store().await?;
    let now = Timestamp::now();
    let spec = one_step_spec("owned", now, OnInterrupt::Hold, false);
    let (job, run) = submit_one(&store, &spec, &now).await?;
    let (stale, live) = stale_and_live_attempt(&store, job, run, &now).await?;
    let notify = NotifySpec {
        title: "parked".into(),
        body: "b".into(),
    };

    store
        .hold_run(
            job,
            run,
            "main",
            HeldReason::Errored,
            &notify,
            &now,
            Some(Claim {
                step: "main",
                attempt: stale,
            }),
        )
        .await?;
    assert_eq!(
        run_state(&store, job, run).await,
        ("running".to_string(), "running".to_string()),
        "a live attempt must not be parked by an older one's exhausted retries"
    );
    let queued: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM notifications WHERE job_id = ?")
        .bind(job.0)
        .fetch_one(store.pool())
        .await?;
    assert_eq!(
        queued, 0,
        "no on_hold notification for a park that didn't happen"
    );

    // The live attempt can still park itself.
    store
        .hold_run(
            job,
            run,
            "main",
            HeldReason::Errored,
            &notify,
            &now,
            Some(Claim {
                step: "main",
                attempt: live,
            }),
        )
        .await?;
    assert_eq!(run_state(&store, job, run).await.0, "held");
    Ok(())
}

/// …and the pre-spawn check: if it had to be retried, the cursor it finds may
/// belong to a newer attempt. Answering "yes" there would put two processes
/// on one step.
#[tokio::test]
async fn a_stale_claim_is_not_current_so_it_never_spawns() -> Result<()> {
    let (_dir, store) = temp_store().await?;
    let now = Timestamp::now();
    let spec = one_step_spec("owned", now, OnInterrupt::Hold, false);
    let (job, run) = submit_one(&store, &spec, &now).await?;
    let (stale, live) = stale_and_live_attempt(&store, job, run, &now).await?;

    assert!(
        !store
            .claim_is_current(
                job,
                run,
                Claim {
                    step: "main",
                    attempt: stale
                }
            )
            .await?,
        "the stale attempt must not spawn against a newer attempt's cursor"
    );
    assert!(
        store
            .claim_is_current(
                job,
                run,
                Claim {
                    step: "main",
                    attempt: live
                }
            )
            .await?,
        "the live attempt's own claim must still read as current"
    );
    Ok(())
}

/// The deadline path takes the same fence: a held-up deadline from an older
/// attempt must not fail a newer attempt's run.
#[tokio::test]
async fn a_stale_deadline_cannot_fail_a_newer_attempt() -> Result<()> {
    let (_dir, store) = temp_store().await?;
    let now = Timestamp::now();
    let spec = one_step_spec("owned", now, OnInterrupt::Hold, false);
    let (job, run) = submit_one(&store, &spec, &now).await?;
    let (stale, _live) = stale_and_live_attempt(&store, job, run, &now).await?;

    let outcome = store
        .fail_deadline(
            job,
            run,
            "main",
            &now,
            None,
            Some(Claim {
                step: "main",
                attempt: stale,
            }),
        )
        .await?;
    assert!(outcome.is_none(), "a stale deadline must decide nothing");
    assert_eq!(run_state(&store, job, run).await.0, "running");
    Ok(())
}
