//! Follow-up approval/expiry boundary tests (testing/followup/approval-expiry).
//! Every instant is injected; nothing here reads the wall clock.
use anyhow::Result;
use cued::daemon::{Arm, fire_job, reconcile};
use cued::model::*;
use cued::store::Store;
use cued::submit::single_shell_graph;
use jiff::{SignedDuration, Timestamp};

fn time() -> Timestamp {
    "2026-09-24T12:00:00Z".parse().unwrap()
}
fn later(now: Timestamp, secs: i64) -> Timestamp {
    now.checked_add(SignedDuration::from_secs(secs)).unwrap()
}
fn nanos(at: Timestamp, offset: i64) -> Timestamp {
    at.checked_add(SignedDuration::from_nanos(offset)).unwrap()
}
fn spec(name: &str, schedule: Schedule) -> JobSpec {
    JobSpec {
        name: Some(name.into()),
        schedule,
        graph: single_shell_graph(vec!["/bin/true".into()]),
        cwd: "/tmp".into(),
        env: CapturedEnv::default(),
        policies: Policies::default(),
        hooks: Hooks::default(),
    }
}
fn once(name: &str, at: Timestamp) -> JobSpec {
    spec(name, Schedule::Once { at })
}
fn recurring(name: &str, now: Timestamp) -> JobSpec {
    spec(
        name,
        Schedule::Every {
            interval: SignedDuration::from_secs(3600),
            anchor: now,
            until: None,
            count: Some(3),
        },
    )
}
async fn temp_store() -> Result<(tempfile::TempDir, Store)> {
    let dir = tempfile::tempdir()?;
    let store = Store::open(&dir.path().join("db")).await?;
    Ok((dir, store))
}
async fn pending(store: &Store, spec: &JobSpec, now: Timestamp) -> Result<JobId> {
    Ok(store
        .submit_definition(spec, &now, JobSource::Mcp, true)
        .await?
        .0)
}
async fn run_count(store: &Store, id: JobId) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT COUNT(*) FROM runs WHERE job_id = ?")
            .bind(id.0)
            .fetch_one(store.pool())
            .await?,
    )
}
/// (definition, deadline, reason) for each shape of approval deadline.
fn shapes(now: Timestamp) -> Vec<(JobSpec, Timestamp, ExpiryReason)> {
    vec![
        (
            once("shape", later(now, 60)),
            later(now, 60),
            ExpiryReason::ScheduledAtPassed,
        ),
        (
            recurring("shape", now),
            now.checked_add(PENDING_APPROVAL_TTL).unwrap(),
            ExpiryReason::ApprovalTtlElapsed,
        ),
    ]
}

/// §7.6: "Expired jobs release their live-name reservation immediately."
/// Expiry happens at the deadline, not when some sweep next notices it, so a
/// submission at `now >= deadline` must not be refused by a pending job whose
/// authorization window already closed. One nanosecond earlier, the name is
/// still held.
#[tokio::test]
async fn name_is_released_at_the_deadline_without_a_prior_sweep() -> Result<()> {
    let now = time();
    for (definition, deadline, reason) in shapes(now) {
        let (_dir, store) = temp_store().await?;
        let id = pending(&store, &definition, now).await?;
        let before = nanos(deadline, -1);
        let replacement = once("shape", later(deadline, 3600));
        let refused = store
            .submit_definition(&replacement, &before, JobSource::Mcp, true)
            .await;
        assert!(
            refused.is_err(),
            "name still reserved 1ns before the deadline"
        );
        assert!(store.load_job(id).await?.status.is_live());

        // No expire_pending / list / show between the deadline and this submit.
        let (reused, _, _) = store
            .submit_definition(&replacement, &deadline, JobSource::Mcp, true)
            .await?;
        assert_ne!(reused, id);
        let old = store.load_job(id).await?;
        assert_eq!(old.status, JobStatus::Expired);
        assert_eq!(old.expired_at, Some(deadline));
        assert_eq!(old.expiry_reason, Some(reason));
        assert_eq!(run_count(&store, id).await?, 0);
        assert_eq!(store.fired_count(id).await?, 0);
    }
    Ok(())
}

/// Cancel and expiry share the writer; at `now >= deadline` the job already
/// expired, and that durable fact (time and cause) must not be overwritten by
/// a later Cancelled. Before the deadline cancel denies the pending job.
#[tokio::test]
async fn cancel_at_the_deadline_cannot_erase_the_durable_expiry() -> Result<()> {
    let now = time();
    for (definition, deadline, reason) in shapes(now) {
        for offset in [-1i64, 0, 1] {
            let (_dir, store) = temp_store().await?;
            let id = pending(&store, &definition, now).await?;
            let instant = nanos(deadline, offset);
            let cancelled = store.cancel_job(id, &instant).await;
            let job = store.load_job(id).await?;
            if offset < 0 {
                assert!(cancelled.is_ok());
                assert_eq!(job.status, JobStatus::Cancelled);
                assert_eq!(job.expired_at, None);
            } else {
                let error = format!("{:#}", cancelled.expect_err("expired first"));
                assert!(error.contains("Expired"), "{error}");
                assert_eq!(job.status, JobStatus::Expired);
                assert_eq!(job.expired_at, Some(deadline));
                assert_eq!(job.expiry_reason, Some(reason));
            }
            assert_eq!(
                job.approval.as_ref().map(|a| a.state),
                Some(ApprovalState::Pending)
            );
            assert_eq!(run_count(&store, id).await?, 0);
        }
    }
    Ok(())
}

/// Concurrent approve / cancel / sweep one nanosecond before and exactly at the
/// deadline: whatever interleaving the writer picks, no run can start unless
/// approval won, and at the deadline only expiry can win.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn approve_cancel_and_sweep_race_has_one_consistent_outcome() -> Result<()> {
    let now = time();
    for (definition, deadline, reason) in shapes(now) {
        for offset in [-1i64, 0] {
            // Every start order of the three writers, several times each.
            for round in 0..18 {
                let (_dir, store) = temp_store().await?;
                let id = pending(&store, &definition, now).await?;
                let instant = nanos(deadline, offset);
                let hash = definition.definition_hash()?;
                let approve = {
                    let store = store.clone();
                    async move { store.approve(id, hash, &instant).await }
                };
                let cancel = {
                    let store = store.clone();
                    async move { store.cancel_job(id, &instant).await }
                };
                let sweep = {
                    let store = store.clone();
                    async move { store.expire_pending(&instant).await }
                };
                let (approve, cancel, sweep) = match round % 6 {
                    0 => {
                        let a = tokio::spawn(approve);
                        let c = tokio::spawn(cancel);
                        let s = tokio::spawn(sweep);
                        (a, c, s)
                    }
                    1 => {
                        let a = tokio::spawn(approve);
                        let s = tokio::spawn(sweep);
                        let c = tokio::spawn(cancel);
                        (a, c, s)
                    }
                    2 => {
                        let c = tokio::spawn(cancel);
                        let a = tokio::spawn(approve);
                        let s = tokio::spawn(sweep);
                        (a, c, s)
                    }
                    3 => {
                        let c = tokio::spawn(cancel);
                        let s = tokio::spawn(sweep);
                        let a = tokio::spawn(approve);
                        (a, c, s)
                    }
                    4 => {
                        let s = tokio::spawn(sweep);
                        let a = tokio::spawn(approve);
                        let c = tokio::spawn(cancel);
                        (a, c, s)
                    }
                    _ => {
                        let s = tokio::spawn(sweep);
                        let c = tokio::spawn(cancel);
                        let a = tokio::spawn(approve);
                        (a, c, s)
                    }
                };
                let (approved, cancelled) = (approve.await?, cancel.await?);
                sweep.await??;
                let job = store.load_job(id).await?;
                if offset == 0 {
                    assert!(approved.is_err() && cancelled.is_err());
                    assert_eq!(job.status, JobStatus::Expired);
                    assert_eq!(job.expired_at, Some(deadline));
                    assert_eq!(job.expiry_reason, Some(reason));
                } else {
                    assert!(cancelled.is_ok(), "cancel is always possible before expiry");
                    assert_eq!(job.status, JobStatus::Cancelled);
                    assert_eq!(job.expired_at, None);
                    // Approval either lost (still pending) or won before cancel.
                    let state = job.approval.as_ref().unwrap().state;
                    assert_eq!(approved.is_ok(), state == ApprovalState::Approved);
                }
                assert!(!job.can_start());
                assert!(reconcile(&store, &instant).await?.is_empty());
                assert!(
                    store
                        .begin_step(id, RunId(1), "run", &instant)
                        .await?
                        .is_none()
                );
                assert_eq!(store.fired_count(id).await?, 0);
            }
        }
    }
    Ok(())
}

/// Startup reconciliation at exactly the deadline expires; one nanosecond
/// earlier it leaves the definition pending and arms nothing for it.
#[tokio::test]
async fn restart_reconcile_expires_exactly_at_the_deadline_and_arms_nothing() -> Result<()> {
    let now = time();
    for (definition, deadline, reason) in shapes(now) {
        let (dir, store) = temp_store().await?;
        let id = pending(&store, &definition, now).await?;
        drop(store);
        let store = Store::open(&dir.path().join("db")).await?;
        let arms = reconcile(&store, &nanos(deadline, -1)).await?;
        assert!(arms.iter().all(|arm| !matches!(
            arm,
            Arm::Fire { job, .. } if *job == id
        )));
        assert!(arms.is_empty());
        assert!(store.load_job(id).await?.status.is_live());
        drop(store);
        let store = Store::open(&dir.path().join("db")).await?;
        assert!(reconcile(&store, &deadline).await?.is_empty());
        let job = store.load_job(id).await?;
        assert_eq!(job.status, JobStatus::Expired);
        assert_eq!(job.expired_at, Some(deadline));
        assert_eq!(job.expiry_reason, Some(reason));
        // The expired job is not live, so name-uniqueness no longer covers it,
        // and nothing re-arms it later either.
        assert!(
            fire_job(&store, id, &deadline, &later(deadline, 7200))
                .await?
                .is_empty()
        );
        assert_eq!(run_count(&store, id).await?, 0);
        assert_eq!(store.fired_count(id).await?, 0);
    }
    Ok(())
}

/// A paused pending recurring job still expires at created_at + 7 days, and
/// a pending recurrence many periods overdue has spent none of its cap.
#[tokio::test]
async fn paused_pending_recurrence_expires_at_ttl_without_spending_its_cap() -> Result<()> {
    let (_dir, store) = temp_store().await?;
    let now = time();
    let definition = recurring("paused", now);
    let id = pending(&store, &definition, now).await?;
    store.set_paused(id).await?;
    let deadline = now.checked_add(PENDING_APPROVAL_TTL)?;
    // Every hourly instant over the pending week is due; none may be spent.
    assert!(
        fire_job(&store, id, &later(now, 3600), &nanos(deadline, -1))
            .await?
            .is_empty()
    );
    store.expire_pending(&nanos(deadline, -1)).await?;
    assert_eq!(store.load_job(id).await?.status, JobStatus::Paused);
    store.expire_pending(&deadline).await?;
    let job = store.load_job(id).await?;
    assert_eq!(job.status, JobStatus::Expired);
    assert_eq!(job.expired_at, Some(deadline));
    assert_eq!(job.expiry_reason, Some(ExpiryReason::ApprovalTtlElapsed));
    assert_eq!(store.fired_count(id).await?, 0);
    assert_eq!(run_count(&store, id).await?, 0);
    Ok(())
}

/// Expiry is recorded at the deadline, not at whichever later sweep noticed
/// it, and a second sweep does not move it.
#[tokio::test]
async fn late_sweep_records_the_deadline_not_the_sweep_time() -> Result<()> {
    let now = time();
    for (definition, deadline, reason) in shapes(now) {
        let (_dir, store) = temp_store().await?;
        let id = pending(&store, &definition, now).await?;
        store.expire_pending(&later(deadline, 86_400)).await?;
        store.expire_pending(&later(deadline, 2 * 86_400)).await?;
        let job = store.load_job(id).await?;
        assert_eq!(job.expired_at, Some(deadline));
        assert_eq!(job.expiry_reason, Some(reason));
    }
    Ok(())
}

#[tokio::test]
async fn cancel_waiting_for_the_writer_preserves_elapsed_expiry() -> Result<()> {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    let (_dir, store) = temp_store().await?;
    let created = time();
    let deadline = later(created, 60);
    let job = pending(&store, &once("writer-wait", deadline), created).await?;
    let holding_writer = store.pool().begin().await?;
    let elapsed = AtomicBool::new(false);
    let clock_read = AtomicBool::new(false);
    let cancel = store.cancel_job_with_clock(job, || {
        clock_read.store(true, Ordering::SeqCst);
        if elapsed.load(Ordering::SeqCst) {
            deadline
        } else {
            nanos(deadline, -1)
        }
    });
    tokio::pin!(cancel);
    tokio::select! {
        result = &mut cancel => panic!("cancel acquired an occupied writer: {result:?}"),
        _ = tokio::time::sleep(Duration::from_millis(50)) => {}
    }
    assert!(!clock_read.load(Ordering::SeqCst));
    elapsed.store(true, Ordering::SeqCst);
    holding_writer.rollback().await?;
    assert!(cancel.await.unwrap_err().to_string().contains("Expired"));
    let expired = store.load_job(job).await?;
    assert_eq!(expired.status, JobStatus::Expired);
    assert_eq!(expired.expired_at, Some(deadline));
    assert_eq!(expired.expiry_reason, Some(ExpiryReason::ScheduledAtPassed));
    Ok(())
}
