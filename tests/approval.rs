//! Approval acceptance tests use controlled instants and the real durable store.
use anyhow::Result;
use cued::config::Retention;
use cued::daemon::{Arm, fire_job, reconcile};
use cued::model::*;
use cued::store::{Claim, Fired, Firing, Store};
use cued::submit::single_shell_graph;
use jiff::{SignedDuration, Timestamp};

fn time() -> Timestamp {
    "2026-09-24T12:00:00Z".parse().unwrap()
}
fn later(now: Timestamp, secs: i64) -> Timestamp {
    now.checked_add(SignedDuration::from_secs(secs)).unwrap()
}
fn spec(schedule: Schedule) -> JobSpec {
    JobSpec {
        name: Some("review-me".into()),
        schedule,
        graph: single_shell_graph(vec!["/bin/true".into()]),
        cwd: "/tmp".into(),
        env: CapturedEnv::default(),
        policies: Policies::default(),
        hooks: Hooks::default(),
    }
}
fn recurring(now: Timestamp) -> JobSpec {
    spec(Schedule::Every {
        interval: SignedDuration::from_secs(60),
        anchor: now,
        until: None,
        count: Some(3),
    })
}
async fn temp_store() -> Result<(tempfile::TempDir, Store)> {
    let dir = tempfile::tempdir()?;
    let store = Store::open(&dir.path().join("db")).await?;
    Ok((dir, store))
}
async fn run_count(store: &Store, id: JobId) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT COUNT(*) FROM runs WHERE job_id = ?")
            .bind(id.0)
            .fetch_one(store.pool())
            .await?,
    )
}
async fn submit(store: &Store, spec: &JobSpec, now: Timestamp) -> Result<JobId> {
    let (id, run, _) = store
        .submit_definition(spec, &now, JobSource::Mcp, true)
        .await?;
    assert!(run.is_none());
    Ok(id)
}

#[tokio::test]
async fn pending_never_claims_or_spends_even_after_restart_and_full_queue() -> Result<()> {
    for overlap in [Overlap::Skip, Overlap::Queue] {
        let (dir, store) = temp_store().await?;
        let now = time();
        let mut definition = recurring(now);
        definition.policies.overlap = overlap;
        let id = submit(&store, &definition, now).await?;
        // Seed a full queue to probe the durable drain, not only scheduler prechecks.
        sqlx::query("UPDATE jobs SET queued_at = ? WHERE id = ?")
            .bind(later(now, 60).to_string())
            .bind(id.0)
            .execute(store.pool())
            .await?;
        drop(store);
        let store = Store::open(&dir.path().join("db")).await?;
        assert!(reconcile(&store, &later(now, 600)).await?.is_empty());
        assert!(
            fire_job(&store, id, &later(now, 60), &later(now, 600))
                .await?
                .is_empty()
        );
        assert!(store.drain_queued(id, "run").await?.is_none());
        assert_eq!(
            store
                .record_firing(Firing {
                    job: id,
                    entry_step: "run",
                    claiming: &later(now, 60),
                    consumed: 100,
                    run_at: Some(&later(now, 60)),
                    skipped: None,
                    queue_at: None,
                    next_fire_at: None,
                    now: &later(now, 600)
                })
                .await?,
            Fired::Superseded
        );
        assert_eq!(
            store
                .begin_step(id, RunId(1), "run", &later(now, 600))
                .await?,
            None
        );
        assert_eq!(run_count(&store, id).await?, 0);
        assert_eq!(store.fired_count(id).await?, 0);
    }
    Ok(())
}

#[tokio::test]
async fn one_shot_deadline_is_exclusive_and_has_one_transactional_winner() -> Result<()> {
    for offset in [-1, 0, 1] {
        let (_dir, store) = temp_store().await?;
        let now = time();
        let deadline = later(now, 60);
        let definition = spec(Schedule::Once { at: deadline });
        let id = submit(&store, &definition, now).await?;
        let instant = deadline.checked_add(SignedDuration::from_nanos(offset))?;
        let hash = definition.definition_hash()?;
        let (approval, sweep) = tokio::join!(
            store.approve(id, hash, &instant),
            store.expire_pending(&instant)
        );
        sweep?;
        let job = store.load_job(id).await?;
        if offset < 0 {
            assert!(approval.is_ok());
            assert_eq!(job.status, JobStatus::Active);
            assert_eq!(job.approval.unwrap().state, ApprovalState::Approved);
            assert_eq!(run_count(&store, id).await?, 1);
        } else {
            assert!(approval.is_err());
            assert_eq!(job.status, JobStatus::Expired);
            assert_eq!(job.expired_at, Some(deadline));
            assert_eq!(job.expiry_reason, Some(ExpiryReason::ScheduledAtPassed));
            assert!(store.approve(id, hash, &instant).await.is_err());
            assert!(reconcile(&store, &instant).await?.is_empty());
            assert_eq!(run_count(&store, id).await?, 0);
            assert_eq!(store.fired_count(id).await?, 0);
        }
    }
    Ok(())
}

#[tokio::test]
async fn expired_orphan_is_visible_retained_collected_and_releases_name() -> Result<()> {
    let (_dir, store) = temp_store().await?;
    let now = time();
    let definition = spec(Schedule::Once { at: later(now, 60) });
    let id = submit(&store, &definition, now).await?;
    assert!(
        store
            .submit_definition(&definition, &now, JobSource::Mcp, true)
            .await
            .is_err()
    );
    store.expire_pending(&later(now, 60)).await?;
    let overview = store.list_overview(false, &later(now, 60)).await?;
    assert_eq!(overview.len(), 1);
    assert_eq!(overview[0].status, JobStatus::Expired);
    let replacement = spec(Schedule::Once {
        at: later(now, 120),
    });
    assert_ne!(submit(&store, &replacement, later(now, 60)).await?, id);
    let retention = Retention {
        days: 1,
        runs_per_job: 20,
    };
    assert!(
        !store
            .gc(&retention, &later(now, 86459))
            .await?
            .jobs
            .contains(&id)
    );
    assert!(store.load_job(id).await.is_ok());
    assert!(
        store
            .gc(&retention, &later(now, 86460))
            .await?
            .jobs
            .contains(&id)
    );
    assert!(store.load_job(id).await.is_err());
    Ok(())
}

#[tokio::test]
async fn ttl_is_from_creation_rejected_without_sweep_and_only_expires_pending() -> Result<()> {
    let (_dir, store) = temp_store().await?;
    let now = time();
    let mut definition = recurring(now);
    let id = submit(&store, &definition, now).await?;
    let deadline = now.checked_add(PENDING_APPROVAL_TTL)?;
    // No sweep preceded this approval attempt.
    assert!(
        store
            .approve(id, definition.definition_hash()?, &deadline)
            .await
            .is_err()
    );
    let expired = store.load_job(id).await?;
    assert_eq!(expired.status, JobStatus::Expired);
    assert_eq!(
        expired.expiry_reason,
        Some(ExpiryReason::ApprovalTtlElapsed)
    );
    assert_eq!(expired.expired_at, Some(deadline));
    assert_eq!(run_count(&store, id).await?, 0);
    definition.name = Some("approved".into());
    let approved = submit(&store, &definition, now).await?;
    store
        .approve(approved, definition.definition_hash()?, &later(now, 1))
        .await?;
    store.expire_pending(&later(deadline, 1)).await?;
    assert_eq!(
        store.load_job(approved).await?.approval.unwrap().state,
        ApprovalState::Approved
    );
    assert_eq!(store.load_job(approved).await?.status, JobStatus::Active);
    assert!(
        store
            .gc(
                &Retention {
                    days: 1,
                    runs_per_job: 20
                },
                &later(deadline, 86400)
            )
            .await?
            .jobs
            .contains(&id)
    );
    Ok(())
}

#[tokio::test]
async fn approval_after_missed_recurrence_catches_up_once_without_spending_pending_cap()
-> Result<()> {
    for catch_up in [CatchUp::RunOnce, CatchUp::Skip] {
        let (_dir, store) = temp_store().await?;
        let now = time();
        let mut definition = recurring(now);
        definition.policies.catch_up = catch_up;
        let id = submit(&store, &definition, now).await?;
        let approved_at = later(now, 601);
        let (_, next) = store
            .approve(id, definition.definition_hash()?, &approved_at)
            .await?;
        assert_eq!(store.fired_count(id).await?, 0);
        let next = next.unwrap();
        if catch_up == CatchUp::Skip {
            assert_eq!(next, later(now, 660));
        } else {
            assert_eq!(next, later(now, 600));
        }
        let arms = fire_job(&store, id, &next, &next.max(approved_at)).await?;
        assert_eq!(arms.iter().filter(|a| matches!(a, Arm::Step(_))).count(), 1);
        assert_eq!(run_count(&store, id).await?, 1);
        assert_eq!(store.fired_count(id).await?, 1);
        assert_eq!(
            store.load_job(id).await?.approval.unwrap().approved_at,
            Some(approved_at)
        );
    }
    Ok(())
}

#[tokio::test]
async fn pause_approval_preserves_axis_resume_and_cancel_cannot_bypass() -> Result<()> {
    let (_dir, store) = temp_store().await?;
    let now = time();
    let definition = recurring(now);
    let id = submit(&store, &definition, now).await?;
    store.set_paused(id).await?;
    assert!(
        store
            .set_resumed(id, Some(&later(now, 60)), &now)
            .await
            .is_err()
    );
    store
        .approve(id, definition.definition_hash()?, &later(now, 1))
        .await?;
    assert_eq!(store.load_job(id).await?.status, JobStatus::Paused);
    assert!(reconcile(&store, &now).await?.is_empty());
    store.set_resumed(id, Some(&later(now, 60)), &now).await?;
    assert!(store.load_job(id).await?.can_start());
    store.cancel_job(id, &now).await?;
    assert!(
        store
            .approve(id, definition.definition_hash()?, &now)
            .await
            .is_err()
    );
    assert!(!store.load_job(id).await?.can_start());
    let id = submit(&store, &definition, now).await?;
    store.cancel_job(id, &now).await?;
    assert!(
        store
            .approve(id, definition.definition_hash()?, &now)
            .await
            .is_err()
    );
    assert_eq!(run_count(&store, id).await?, 0);
    Ok(())
}

#[tokio::test]
async fn canonical_definition_change_repends_atomically_and_blocks_all_recovery() -> Result<()> {
    let (_dir, store) = temp_store().await?;
    let now = time();
    let definition = spec(Schedule::Once {
        at: later(now, 3600),
    });
    // Gating follows the record, including a CLI-labelled job.
    let (id, _, _) = store
        .submit_definition(&definition, &now, JobSource::Cli, true)
        .await?;
    store
        .approve(id, definition.definition_hash()?, &now)
        .await?;
    let attempt = store.begin_step(id, RunId(1), "run", &now).await?.unwrap();
    let mut changed_env = CapturedEnv::default();
    changed_env
        .vars
        .insert("DEPLOY_SECRET".into(), "actual-secret".into());
    sqlx::query("UPDATE jobs SET env = ? WHERE id = ?")
        .bind(serde_json::to_string(&changed_env)?)
        .bind(id.0)
        .execute(store.pool())
        .await?;
    let changed = store.load_job(id).await?;
    assert_eq!(
        changed.approval.as_ref().unwrap().state,
        ApprovalState::Pending
    );
    assert!(!changed.approval_valid());
    assert!(
        store
            .approve(id, definition.definition_hash()?, &now)
            .await
            .is_err()
    );
    assert!(
        !store
            .claim_is_current(
                id,
                RunId(1),
                Claim {
                    step: "run",
                    attempt
                }
            )
            .await?
    );
    assert!(
        store
            .rearm_interrupted(id, RunId(1), "run", &now)
            .await
            .is_err()
    );
    assert!(reconcile(&store, &now).await?.is_empty());
    sqlx::query("UPDATE runs SET cursor_kind = 'held', status = 'held' WHERE job_id = ?")
        .bind(id.0)
        .execute(store.pool())
        .await?;
    assert!(store.resume_held(id, RunId(1), &now).await.is_err());
    assert!(store.rewind_run(id, RunId(1), "run", &now).await.is_err());
    sqlx::query("UPDATE runs SET cursor_kind = 'waiting', status = 'pending' WHERE job_id = ?")
        .bind(id.0)
        .execute(store.pool())
        .await?;
    assert!(store.begin_step(id, RunId(1), "run", &now).await?.is_none());
    assert!(reconcile(&store, &now).await?.is_empty());
    store
        .approve(id, changed.definition().definition_hash()?, &now)
        .await?;
    assert_eq!(
        store.load_job(id).await?.approval.unwrap().state,
        ApprovalState::Approved
    );
    Ok(())
}

#[test]
fn canonical_hash_covers_every_definition_field_and_is_deterministic() -> Result<()> {
    let definition = recurring(time());
    let original = definition.definition_hash()?;
    let json = serde_json::to_string(&definition)?;
    assert_eq!(
        original,
        serde_json::from_str::<JobSpec>(&json)?.definition_hash()?
    );
    let mut variants = Vec::new();
    let mut changed = definition.clone();
    changed.cwd = "/".into();
    variants.push(changed);
    let mut changed = definition.clone();
    changed.name = None;
    variants.push(changed);
    let mut changed = definition.clone();
    changed.env.vars.insert("SECRET".into(), "value".into());
    variants.push(changed);
    let mut changed = definition.clone();
    changed.policies.overlap = Overlap::Queue;
    variants.push(changed);
    let mut changed = definition.clone();
    changed.hooks.on_hold = Some(NotifySpec {
        title: "review".into(),
        body: "body".into(),
    });
    variants.push(changed);
    let mut changed = definition.clone();
    changed.schedule = Schedule::Once { at: time() };
    variants.push(changed);
    let mut changed = definition.clone();
    changed.graph.steps.get_mut("run").unwrap().cwd = Some("/".into());
    variants.push(changed);
    for changed in variants {
        assert_ne!(original, changed.definition_hash()?);
    }
    Ok(())
}

#[tokio::test]
async fn open_jobs_and_cli_jobs_keep_existing_semantics_and_approval_marker_lasts() -> Result<()> {
    for source in [JobSource::Cli, JobSource::Mcp] {
        let (_dir, store) = temp_store().await?;
        let now = time();
        let definition = spec(Schedule::Once {
            at: later(now, -60),
        });
        let (id, run, _) = store
            .submit_definition(&definition, &now, source, false)
            .await?;
        assert_eq!(run, Some(RunId(1)));
        assert!(store.load_job(id).await?.approval.is_none());
        assert!(
            store
                .begin_step(id, run.unwrap(), "run", &now)
                .await?
                .is_some()
        );
    }
    Ok(())
}

#[tokio::test]
async fn pending_notifications_are_durable_and_do_not_expose_captured_secrets() -> Result<()> {
    let (dir, store) = temp_store().await?;
    let now = time();
    let mut definition = recurring(now);
    definition
        .env
        .vars
        .insert("CUSTOM_VALUE".into(), "sensitive-secret".into());
    definition.graph = single_shell_graph(vec!["echo".into(), "sensitive-secret".into()]);
    let id = submit(&store, &definition, now).await?;
    let job = store.load_job(id).await?;
    let preview = serde_json::to_string(&cued::mcp::redacted_job(&job)?)?;
    assert!(!preview.contains("sensitive-secret"));
    assert!(preview.contains("CUSTOM_VALUE"));
    assert!(job.display_status().contains("Pending approval"));
    drop(store);
    let store = Store::open(&dir.path().join("db")).await?;
    let notifications = store.undelivered_notifications().await?;
    assert_eq!(notifications.len(), 1);
    assert!(
        notifications[0]
            .spec
            .body
            .contains(&format!("cued approve {id}"))
    );
    assert!(!notifications[0].spec.body.contains("sensitive-secret"));
    Ok(())
}

#[tokio::test]
async fn approval_clock_is_read_after_waiting_for_the_writer() -> Result<()> {
    use std::sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    };
    let (_dir, store) = temp_store().await?;
    let now = time();
    let definition = spec(Schedule::Once { at: later(now, 60) });
    let id = submit(&store, &definition, now).await?;
    let clock = Arc::new(AtomicI64::new(0));
    let writer = store.pool().acquire().await?;
    let (sent, received) = tokio::sync::oneshot::channel();
    let task = {
        let store = store.clone();
        let clock = clock.clone();
        tokio::spawn(async move {
            sent.send(()).unwrap();
            store
                .approve_with_clock(id, definition.definition_hash().unwrap(), || {
                    later(now, clock.load(Ordering::SeqCst))
                })
                .await
        })
    };
    received.await?;
    clock.store(60, Ordering::SeqCst);
    drop(writer);
    assert!(task.await?.is_err());
    assert_eq!(store.load_job(id).await?.status, JobStatus::Expired);
    assert_eq!(run_count(&store, id).await?, 0);
    Ok(())
}

#[tokio::test]
async fn skipped_exhausted_schedule_finishes_without_spending_pending_budget() -> Result<()> {
    let (_dir, store) = temp_store().await?;
    let now = time();
    let mut definition = recurring(now);
    definition.schedule = Schedule::Every {
        interval: SignedDuration::from_secs(60),
        anchor: now,
        until: Some(later(now, 120)),
        count: Some(3),
    };
    definition.policies.catch_up = CatchUp::Skip;
    let id = submit(&store, &definition, now).await?;
    let (_, next) = store
        .approve(id, definition.definition_hash()?, &later(now, 600))
        .await?;
    assert!(next.is_none());
    assert_eq!(
        store.load_job(id).await?.status,
        JobStatus::Active,
        "approval preserves lifecycle"
    );
    assert!(reconcile(&store, &later(now, 600)).await?.is_empty());
    assert_eq!(store.load_job(id).await?.status, JobStatus::Done);
    assert_eq!(store.fired_count(id).await?, 0);
    assert_eq!(run_count(&store, id).await?, 0);
    Ok(())
}

#[tokio::test]
async fn new_approval_cannot_authorize_a_stale_definition_snapshot() -> Result<()> {
    let (_dir, store) = temp_store().await?;
    let now = time();
    let definition = recurring(now);
    let old_hash = definition.definition_hash()?;
    let id = submit(&store, &definition, now).await?;
    store.approve(id, old_hash, &now).await?;
    // An old firing task already loaded definition; simulate an atomic edit
    // and a fresh human review before that task reaches the durable claim.
    sqlx::query("UPDATE jobs SET cwd = '/' WHERE id = ?")
        .bind(id.0)
        .execute(store.pool())
        .await?;
    let new_hash = store.load_job(id).await?.definition().definition_hash()?;
    store.approve(id, new_hash, &now).await?;
    let at = later(now, 60);
    assert_eq!(
        store
            .record_firing_checked(
                Firing {
                    job: id,
                    entry_step: "run",
                    claiming: &at,
                    consumed: 1,
                    run_at: Some(&at),
                    skipped: None,
                    queue_at: None,
                    next_fire_at: Some(&later(now, 120)),
                    now: &at,
                },
                Some(old_hash)
            )
            .await?,
        Fired::Superseded
    );
    assert_eq!(store.fired_count(id).await?, 0);
    fire_job(&store, id, &at, &at).await?;
    assert!(
        store
            .begin_step_checked(id, RunId(1), "run", &at, Some(old_hash))
            .await?
            .is_none()
    );
    let attempt = store
        .begin_step_checked(id, RunId(1), "run", &at, Some(new_hash))
        .await?
        .unwrap();
    assert!(
        !store
            .claim_is_current_checked(
                id,
                RunId(1),
                Claim {
                    step: "run",
                    attempt
                },
                Some(old_hash)
            )
            .await?
    );
    let close = store
        .finish_step_checked(
            cued::store::StepClose {
                job: id,
                entry_step: "run",
                run: RunId(1),
                step: "run",
                attempt,
                ended_at: &at,
                exit_code: Some(0),
                timed_out: false,
                outcome_edge: None,
                next: cued::store::NextCursor::Terminal {
                    status: RunStatus::Done,
                    fail_reason: None,
                },
                notifications: vec![NotifySpec {
                    title: "old definition".into(),
                    body: "must not enqueue".into(),
                }],
            },
            Some(old_hash),
        )
        .await?;
    assert!(!close.advanced);
    assert_eq!(
        store.undelivered_notifications().await?.len(),
        1,
        "only submission notice"
    );
    Ok(())
}
