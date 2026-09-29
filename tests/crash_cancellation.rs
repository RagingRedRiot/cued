//! Claim-boundary and deadline ownership contracts (DESIGN.md §3.3, §4.2).
//! The real-process versions of these, with deterministic fault points, live
//! in testing/followup/crash-cancellation.

use std::collections::BTreeMap;

use anyhow::Result;
use cued::model::*;
use cued::store::{Claim, Store};
use jiff::Timestamp;

async fn fixture() -> Result<(tempfile::TempDir, Store, JobId, RunId, Timestamp)> {
    let dir = tempfile::tempdir()?;
    let store = Store::open(&dir.path().join("db")).await?;
    let now = Timestamp::now();
    let step = Step {
        action: Action::Shell {
            argv: vec!["/bin/true".into()],
        },
        cwd: None,
        env: None,
        timeout: None,
        kill_grace: None,
        transitions: vec![],
        max_visits: None,
        restart_safe: false,
        missed_wait: None,
    };
    let spec = JobSpec {
        name: Some("boundary".into()),
        schedule: Schedule::Once { at: now },
        graph: Graph {
            entry: "main".into(),
            steps: BTreeMap::from([("main".into(), step)]),
        },
        cwd: "/".into(),
        env: CapturedEnv::default(),
        policies: Policies::default(),
        hooks: Hooks::default(),
    };
    let (job, run, _) = store.submit_job(&spec, &now).await?;
    Ok((dir, store, job, run.expect("one-off has a run"), now))
}

async fn cursor(store: &Store, job: JobId, run: RunId) -> Result<(String, String)> {
    Ok(
        sqlx::query_as("SELECT status, cursor_kind FROM runs WHERE job_id = ? AND id = ?")
            .bind(job.0)
            .bind(run.0)
            .fetch_one(store.pool())
            .await?,
    )
}

async fn ended(store: &Store, job: JobId, run: RunId, attempt: u32) -> Result<Option<String>> {
    Ok(sqlx::query_scalar(
        "SELECT ended_at FROM step_runs WHERE job_id = ? AND run_id = ? AND attempt = ?",
    )
    .bind(job.0)
    .bind(run.0)
    .bind(i64::from(attempt))
    .fetch_one(store.pool())
    .await?)
}

/// §4.2: a step already running when you pause keeps running. The claim is
/// what makes it running — the pre-spawn re-check must not turn a pause that
/// landed after it into "don't spawn", because nothing would then close the
/// `running` cursor it leaves behind: the run strands with no process until
/// a daemon restart parks it as interrupted.
#[tokio::test]
async fn a_pause_after_the_claim_does_not_revoke_it() -> Result<()> {
    let (_dir, store, job, run, now) = fixture().await?;
    let attempt = store
        .begin_step(job, run, "main", &now)
        .await?
        .expect("claimed");
    store.set_paused(job).await?;

    assert!(
        store
            .claim_is_current(
                job,
                run,
                Claim {
                    step: "main",
                    attempt
                }
            )
            .await?,
        "a pause must not revoke a claim it can't un-make"
    );
    assert_eq!(cursor(&store, job, run).await?.1, "running");
    Ok(())
}

/// Nothing new starts while paused: a claim attempted *after* the pause is
/// still refused, leaving the run waiting for `resume`.
#[tokio::test]
async fn a_pause_before_the_claim_still_prevents_it() -> Result<()> {
    let (_dir, store, job, run, now) = fixture().await?;
    store.set_paused(job).await?;
    assert!(store.begin_step(job, run, "main", &now).await?.is_none());
    assert_eq!(cursor(&store, job, run).await?.1, "waiting");
    Ok(())
}

/// The re-check still exists for `cued cancel`, which moves the cursor.
#[tokio::test]
async fn a_cancel_after_the_claim_still_revokes_it() -> Result<()> {
    let (_dir, store, job, run, now) = fixture().await?;
    let attempt = store
        .begin_step(job, run, "main", &now)
        .await?
        .expect("claimed");
    store.cancel_job(job, &now).await?;
    assert!(
        !store
            .claim_is_current(
                job,
                run,
                Claim {
                    step: "main",
                    attempt
                }
            )
            .await?
    );
    Ok(())
}

/// §3.2: the deadline ends the run and closes the attempt it killed in one
/// commit — never a moment (or a crash) where the run reads Failed while the
/// attempt still looks unfinished.
#[tokio::test]
async fn the_deadline_closes_its_attempt_in_the_same_commit() -> Result<()> {
    let (_dir, store, job, run, now) = fixture().await?;
    let attempt = store
        .begin_step(job, run, "main", &now)
        .await?
        .expect("claimed");
    let claim = Claim {
        step: "main",
        attempt,
    };

    assert!(
        store
            .fail_deadline(job, run, "main", &now, None, Some(claim))
            .await?
            .is_some()
    );
    assert_eq!(
        cursor(&store, job, run).await?,
        ("failed".into(), "done".into())
    );
    assert!(ended(&store, job, run, attempt).await?.is_some());
    Ok(())
}

/// A deadline held up for a replaced attempt closes only that attempt's own
/// row; the newer attempt's run and row are untouched.
#[tokio::test]
async fn a_stale_deadline_closes_only_its_own_attempt() -> Result<()> {
    let (_dir, store, job, run, now) = fixture().await?;
    let old = store
        .begin_step(job, run, "main", &now)
        .await?
        .expect("claimed");
    store.cancel_job(job, &now).await?;
    store.rewind_run(job, run, "main", &now).await?;
    let new = store
        .begin_step(job, run, "main", &now)
        .await?
        .expect("reclaimed");

    let stale = Claim {
        step: "main",
        attempt: old,
    };
    assert!(
        store
            .fail_deadline(job, run, "main", &now, None, Some(stale))
            .await?
            .is_none()
    );
    assert_eq!(
        cursor(&store, job, run).await?,
        ("running".into(), "running".into())
    );
    assert!(
        ended(&store, job, run, old).await?.is_some(),
        "the old attempt did stop"
    );
    assert!(
        ended(&store, job, run, new).await?.is_none(),
        "the live attempt is not ended"
    );
    Ok(())
}
