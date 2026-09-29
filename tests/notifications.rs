//! Delivery-half tests (DESIGN.md §3.5): the queue drains oldest-first,
//! "no bus" leaves everything for the next tick, and one poisoned row
//! doesn't dam the queue. Transport is scripted — no D-Bus in `cargo test`.

use std::sync::Mutex;

use anyhow::{Result, anyhow};
use jiff::Timestamp;

use cued::daemon::{DeliveryLedger, deliver_pending};
use cued::model::{CapturedEnv, Hooks, JobSpec, NotifySpec, Policies, Schedule};
use cued::notify::Notifier;
use cued::store::Store;
use cued::submit::single_notify_graph;

/// Answers each delivery from a script; records what it was asked to show.
struct Scripted {
    script: Mutex<Vec<Result<bool>>>,
    shown: Mutex<Vec<String>>,
}

impl Scripted {
    fn new(script: Vec<Result<bool>>) -> Self {
        Self {
            script: Mutex::new(script),
            shown: Mutex::new(Vec::new()),
        }
    }
}

impl Notifier for Scripted {
    fn deliver(&self, spec: &NotifySpec) -> impl std::future::Future<Output = Result<bool>> + Send {
        let outcome = {
            let mut script = self.script.lock().expect("script");
            if script.is_empty() {
                Ok(true)
            } else {
                script.remove(0)
            }
        };
        if !matches!(outcome, Ok(false)) {
            self.shown.lock().expect("shown").push(spec.title.clone());
        }
        async move { outcome }
    }
}

/// A store with one job and `titles.len()` queued notifications.
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

async fn undelivered_titles(store: &Store) -> Vec<String> {
    store
        .undelivered_notifications()
        .await
        .expect("queue")
        .into_iter()
        .map(|pending| pending.spec.title)
        .collect()
}

#[tokio::test]
async fn delivers_oldest_first_and_marks_rows() -> Result<()> {
    let (_dir, store) = store_with_queue(&["first", "second", "third"]).await?;
    let notifier = Scripted::new(vec![]); // everything succeeds

    let delivered = deliver_pending(
        &store,
        &notifier,
        &mut DeliveryLedger::default(),
        &Timestamp::now(),
    )
    .await?;

    assert_eq!(delivered, 3);
    assert_eq!(
        *notifier.shown.lock().unwrap(),
        vec!["first", "second", "third"],
        "oldest first"
    );
    assert!(undelivered_titles(&store).await.is_empty());
    Ok(())
}

#[tokio::test]
async fn no_bus_leaves_the_whole_queue() -> Result<()> {
    let (_dir, store) = store_with_queue(&["a", "b"]).await?;
    let notifier = Scripted::new(vec![Ok(false)]); // §3.5: no bus right now

    let delivered = deliver_pending(
        &store,
        &notifier,
        &mut DeliveryLedger::default(),
        &Timestamp::now(),
    )
    .await?;

    assert_eq!(delivered, 0);
    assert_eq!(
        undelivered_titles(&store).await,
        vec!["a", "b"],
        "late beats lost"
    );

    // The bus appears (next login) → the same rows land on the next pass.
    let retry = Scripted::new(vec![]);
    assert_eq!(
        deliver_pending(
            &store,
            &retry,
            &mut DeliveryLedger::default(),
            &Timestamp::now()
        )
        .await?,
        2
    );
    assert!(undelivered_titles(&store).await.is_empty());
    Ok(())
}

#[tokio::test]
async fn a_poisoned_row_does_not_dam_the_queue() -> Result<()> {
    let (_dir, store) = store_with_queue(&["bad", "good"]).await?;
    let notifier = Scripted::new(vec![Err(anyhow!("server rejected it")), Ok(true)]);

    let delivered = deliver_pending(
        &store,
        &notifier,
        &mut DeliveryLedger::default(),
        &Timestamp::now(),
    )
    .await?;

    assert_eq!(delivered, 1, "the failure is skipped, not fatal");
    assert_eq!(
        undelivered_titles(&store).await,
        vec!["bad"],
        "stays queued for retry"
    );
    Ok(())
}
