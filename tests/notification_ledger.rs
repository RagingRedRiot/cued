//! The §3.5 delivery pass's duplicate-avoidance rules, with a scripted
//! transport (no D-Bus): a row acknowledged once is never re-sent in the same
//! daemon lifetime even while recording it fails; an overdue call holds its
//! row; distinct rows stay distinct; and a new daemon (fresh ledger) re-sends
//! whatever was never recorded — at-least-once, never zero.

use std::sync::Mutex;

use anyhow::{Result, anyhow};
use jiff::Timestamp;

use cued::daemon::{DeliveryLedger, deliver_pending};
use cued::model::{CapturedEnv, DeliveryReceipt, Hooks, JobSpec, NotifySpec, Policies, Schedule};
use cued::notify::{Delivery, Notifier};
use cued::store::Store;
use cued::submit::single_notify_graph;

/// Answers each attempt from a script (default: shown); records every call
/// that actually sent something to the "server".
struct Scripted {
    script: Mutex<Vec<Result<Delivery>>>,
    sent: Mutex<Vec<(i64, String)>>,
    next_id: Mutex<u32>,
}

impl Scripted {
    fn new(script: Vec<Result<Delivery>>) -> Self {
        Self {
            script: Mutex::new(script),
            sent: Mutex::new(Vec::new()),
            next_id: Mutex::new(1),
        }
    }
    fn sent(&self) -> Vec<(i64, String)> {
        self.sent.lock().unwrap().clone()
    }
}

impl Notifier for Scripted {
    async fn deliver(&self, _spec: &NotifySpec) -> Result<bool> {
        unreachable!("the daemon attempts by key")
    }

    async fn attempt(&self, key: i64, spec: &NotifySpec) -> Result<Delivery> {
        let outcome = {
            let mut script = self.script.lock().unwrap();
            if script.is_empty() {
                Ok(Delivery::Shown(None))
            } else {
                script.remove(0)
            }
        };
        let outcome = match outcome {
            Ok(Delivery::Shown(None)) => {
                let mut next = self.next_id.lock().unwrap();
                *next += 1;
                Ok(Delivery::Shown(Some(DeliveryReceipt {
                    server: ":1.7".into(),
                    id: *next - 1,
                })))
            }
            other => other,
        };
        if !matches!(outcome, Ok(Delivery::Awaiting | Delivery::Unavailable)) {
            self.sent.lock().unwrap().push((key, spec.title.clone()));
        }
        outcome
    }
}

async fn store_with_queue(titles: &[&str]) -> Result<(tempfile::TempDir, Store)> {
    let dir = tempfile::tempdir()?;
    let store = Store::open(&dir.path().join("cued.db")).await?;
    let now = Timestamp::now();
    let (job, _, _) = store
        .submit_job(
            &JobSpec {
                name: None,
                schedule: Schedule::Once { at: now },
                graph: single_notify_graph("x".into(), String::new()),
                cwd: "/".into(),
                env: CapturedEnv::default(),
                policies: Policies::default(),
                hooks: Hooks::default(),
            },
            &now,
        )
        .await?;
    for title in titles {
        sqlx::query(
            "INSERT INTO notifications (job_id, title, body, created_at) VALUES (?, ?, '', ?)",
        )
        .bind(job.0)
        .bind(title)
        .bind(now.to_string())
        .execute(store.pool())
        .await?;
    }
    Ok((dir, store))
}

async fn rows(store: &Store) -> Result<Vec<(i64, bool, Option<String>, Option<i64>)>> {
    Ok(
        sqlx::query_as::<_, (i64, Option<String>, Option<String>, Option<i64>)>(
            "SELECT id, delivered_at, delivery_server, delivery_id FROM notifications ORDER BY id",
        )
        .fetch_all(store.pool())
        .await?
        .into_iter()
        .map(|(id, at, server, nid)| (id, at.is_some(), server, nid))
        .collect(),
    )
}

async fn break_recording(store: &Store) -> Result<()> {
    sqlx::query(
        "CREATE TRIGGER record_gate BEFORE UPDATE OF delivered_at ON notifications
         BEGIN SELECT RAISE(FAIL, 'record_gate'); END",
    )
    .execute(store.pool())
    .await?;
    Ok(())
}

async fn fix_recording(store: &Store) -> Result<()> {
    sqlx::query("DROP TRIGGER record_gate")
        .execute(store.pool())
        .await?;
    Ok(())
}

#[tokio::test]
async fn a_failed_record_is_retried_without_reshowing() -> Result<()> {
    let (_dir, store) = store_with_queue(&["a", "b"]).await?;
    let notifier = Scripted::new(vec![]);
    let mut ledger = DeliveryLedger::default();
    break_recording(&store).await?;

    // Shown, not recorded: remembered, and nothing more goes on screen.
    assert_eq!(
        deliver_pending(&store, &notifier, &mut ledger, &Timestamp::now()).await?,
        0
    );
    assert_eq!(notifier.sent(), vec![(1, "a".into())]);
    assert_eq!(ledger.unrecorded(), 1);
    assert!(
        rows(&store)
            .await?
            .iter()
            .all(|(_, delivered, _, _)| !delivered),
        "never marked early"
    );

    // Still failing: no redisplay, no new display.
    for _ in 0..3 {
        deliver_pending(&store, &notifier, &mut ledger, &Timestamp::now()).await?;
    }
    assert_eq!(notifier.sent().len(), 1);

    // Recovered: "a" is recorded with its original receipt, then "b" shown.
    fix_recording(&store).await?;
    assert_eq!(
        deliver_pending(&store, &notifier, &mut ledger, &Timestamp::now()).await?,
        2
    );
    assert_eq!(notifier.sent(), vec![(1, "a".into()), (2, "b".into())]);
    assert_eq!(ledger.unrecorded(), 0);
    assert_eq!(
        rows(&store).await?,
        vec![
            (1, true, Some(":1.7".into()), Some(1)),
            (2, true, Some(":1.7".into()), Some(2))
        ]
    );
    Ok(())
}

#[tokio::test]
async fn a_new_daemon_reshows_what_was_never_recorded() -> Result<()> {
    let (_dir, store) = store_with_queue(&["a"]).await?;
    let notifier = Scripted::new(vec![]);
    break_recording(&store).await?;
    deliver_pending(
        &store,
        &notifier,
        &mut DeliveryLedger::default(),
        &Timestamp::now(),
    )
    .await?;
    fix_recording(&store).await?;

    // A restart loses the in-memory ledger: at-least-once, not zero.
    let mut restarted = DeliveryLedger::default();
    assert_eq!(
        deliver_pending(&store, &notifier, &mut restarted, &Timestamp::now()).await?,
        1
    );
    assert_eq!(notifier.sent(), vec![(1, "a".into()), (1, "a".into())]);
    Ok(())
}

#[tokio::test]
async fn the_ledger_forgets_rows_deleted_meanwhile() -> Result<()> {
    let (_dir, store) = store_with_queue(&["a"]).await?;
    let notifier = Scripted::new(vec![]);
    let mut ledger = DeliveryLedger::default();
    break_recording(&store).await?;
    deliver_pending(&store, &notifier, &mut ledger, &Timestamp::now()).await?;
    assert_eq!(ledger.unrecorded(), 1);
    sqlx::query("DELETE FROM notifications")
        .execute(store.pool())
        .await?;
    deliver_pending(&store, &notifier, &mut ledger, &Timestamp::now()).await?;
    assert_eq!(ledger.unrecorded(), 0);
    Ok(())
}

#[tokio::test]
async fn an_overdue_call_holds_its_row_but_not_the_queue() -> Result<()> {
    let (_dir, store) = store_with_queue(&["slow", "next"]).await?;
    let notifier = Scripted::new(vec![
        Ok(Delivery::TimedOut),    // pass 1: "slow" sent, no answer yet → pass ends
        Ok(Delivery::Awaiting),    // pass 2: "slow" still overdue → skipped, not re-sent
        Ok(Delivery::Shown(None)), //        "next" shown
        Ok(Delivery::Shown(None)), // pass 3: "slow"'s late acknowledgement
    ]);
    let mut ledger = DeliveryLedger::default();

    assert_eq!(
        deliver_pending(&store, &notifier, &mut ledger, &Timestamp::now()).await?,
        0
    );
    assert_eq!(
        notifier.sent(),
        vec![(1, "slow".into())],
        "a wedged server gets no more calls this pass"
    );
    assert!(
        rows(&store)
            .await?
            .iter()
            .all(|(_, delivered, _, _)| !delivered),
        "not marked before acknowledged"
    );

    assert_eq!(
        deliver_pending(&store, &notifier, &mut ledger, &Timestamp::now()).await?,
        1
    );
    assert_eq!(
        deliver_pending(&store, &notifier, &mut ledger, &Timestamp::now()).await?,
        1
    );
    let delivered: Vec<bool> = rows(&store).await?.iter().map(|row| row.1).collect();
    assert_eq!(delivered, vec![true, true]);
    Ok(())
}

#[tokio::test]
async fn transport_errors_and_no_bus_keep_the_row() -> Result<()> {
    let (_dir, store) = store_with_queue(&["a", "b"]).await?;
    let notifier = Scripted::new(vec![Err(anyhow!("rejected")), Ok(Delivery::Unavailable)]);
    let mut ledger = DeliveryLedger::default();
    assert_eq!(
        deliver_pending(&store, &notifier, &mut ledger, &Timestamp::now()).await?,
        0
    );
    assert_eq!(ledger.unrecorded(), 0, "an error is not an acknowledgement");
    assert_eq!(
        deliver_pending(&store, &notifier, &mut ledger, &Timestamp::now()).await?,
        2
    );
    Ok(())
}

#[tokio::test]
async fn identical_rows_are_each_shown() -> Result<()> {
    let (_dir, store) = store_with_queue(&["Stretch", "Stretch", "Stretch"]).await?;
    let notifier = Scripted::new(vec![]);
    assert_eq!(
        deliver_pending(
            &store,
            &notifier,
            &mut DeliveryLedger::default(),
            &Timestamp::now()
        )
        .await?,
        3
    );
    let keys: Vec<i64> = notifier.sent().iter().map(|(key, _)| *key).collect();
    assert_eq!(keys, vec![1, 2, 3]);
    Ok(())
}

#[tokio::test]
async fn real_gc_forgets_an_unrecorded_receipt_without_blocking_a_new_row() -> Result<()> {
    use cued::config::Retention;
    let (_dir, store) = store_with_queue(&["old"]).await?;
    let notifier = Scripted::new(vec![]);
    let mut ledger = DeliveryLedger::default();
    break_recording(&store).await?;
    deliver_pending(&store, &notifier, &mut ledger, &Timestamp::now()).await?;
    assert_eq!(ledger.unrecorded(), 1);
    sqlx::query("UPDATE runs SET status='done', cursor_kind='done', ended_at=?")
        .bind(Timestamp::now().to_string())
        .execute(store.pool())
        .await?;
    sqlx::query("UPDATE jobs SET status='done'")
        .execute(store.pool())
        .await?;
    let pruned = store
        .gc(
            &Retention {
                days: 30,
                runs_per_job: 0,
            },
            &Timestamp::now(),
        )
        .await?;
    assert_eq!(pruned.runs.len(), 1);
    assert_eq!(pruned.jobs.len(), 1);
    deliver_pending(&store, &notifier, &mut ledger, &Timestamp::now()).await?;
    assert_eq!(ledger.unrecorded(), 0);
    fix_recording(&store).await?;
    let now = Timestamp::now();
    let (job, _, _) = store
        .submit_job(
            &JobSpec {
                name: None,
                schedule: Schedule::Once { at: now },
                graph: single_notify_graph("new".into(), String::new()),
                cwd: "/".into(),
                env: CapturedEnv::default(),
                policies: Policies::default(),
                hooks: Hooks::default(),
            },
            &now,
        )
        .await?;
    sqlx::query(
        "INSERT INTO notifications (job_id, title, body, created_at) VALUES (?, 'new', '', ?)",
    )
    .bind(job.0)
    .bind(now.to_string())
    .execute(store.pool())
    .await?;
    assert_eq!(
        deliver_pending(&store, &notifier, &mut ledger, &now).await?,
        1
    );
    assert_eq!(notifier.sent(), vec![(1, "old".into()), (2, "new".into())]);
    assert_eq!(
        rows(&store).await?,
        vec![(2, true, Some(":1.7".into()), Some(2))]
    );
    Ok(())
}
