//! End-to-end smoke (DESIGN.md §11): a real daemon on temp paths, a real
//! SQLite store, the real Unix socket and wire protocol, real processes —
//! only the directories are fake.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use jiff::{SignedDuration, Timestamp};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use cued::config::Config;
use cued::model::{CapturedEnv, Hooks, JobSpec, JobStatus, MissedWait, Policies, RunStatus, Schedule};
use cued::paths::Paths;
use cued::proto::{PROTO_VERSION, Request, RequestBody, Response};
use cued::store::Store;
use cued::{daemon, submit};

fn temp_paths(root: &Path) -> Result<Paths> {
    let paths = Paths {
        data_dir: root.join("data"),
        db_file: root.join("data/cued.db"),
        lock_file: root.join("data/cued.lock"),
        logs_dir: root.join("data/logs"),
        daemon_log: root.join("data/daemon.log"),
        socket_file: root.join("cued.sock"),
        config_file: root.join("config.toml"),
    };
    std::fs::create_dir_all(&paths.data_dir)?;
    std::fs::create_dir_all(&paths.logs_dir)?;
    Ok(paths)
}

async fn roundtrip(socket: &Path, body: RequestBody) -> Result<Response> {
    let mut stream = UnixStream::connect(socket).await?;
    let mut payload = serde_json::to_string(&Request { proto: PROTO_VERSION, body })?;
    payload.push('\n');
    stream.write_all(payload.as_bytes()).await?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).await?;
    Ok(serde_json::from_str(&line)?)
}

/// §3.5 transport stub: every test daemon sees "no bus", so `cargo test`
/// never pops real notifications on a developer's desktop.
struct NoBusNotifier;

impl cued::notify::Notifier for NoBusNotifier {
    async fn deliver(&self, _spec: &cued::model::NotifySpec) -> Result<bool> {
        Ok(false)
    }
}

async fn await_socket(socket: &Path) -> Result<()> {
    for _ in 0..200 {
        if UnixStream::connect(socket).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    bail!("daemon socket never came up")
}

#[tokio::test]
async fn one_off_submits_fires_and_lists() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let paths = temp_paths(dir.path())?;
    let store = Store::open(&paths.db_file).await?;
    let daemon_task = tokio::spawn(daemon::serve_with(paths.clone(), Config::default(), store, NoBusNotifier));
    await_socket(&paths.socket_file).await?;

    // Ping: proto handshake works.
    match roundtrip(&paths.socket_file, RequestBody::Ping).await? {
        Response::Pong { proto } => assert_eq!(proto, PROTO_VERSION),
        other => bail!("expected Pong, got {other:?}"),
    }

    // Submit a one-off scheduled 1s in the past — overdue on arrival, so the
    // first tick fires it (§3.4 Case 1 RunAsap by way of the rebuilt heap).
    let out_file = dir.path().join("side-effect.txt");
    let script = format!("echo ran > {}", out_file.display());
    let spec = Box::new(JobSpec {
        name: Some("smoke".into()),
        schedule: Schedule::Once {
            at: Timestamp::now().checked_sub(SignedDuration::from_secs(1))?,
        },
        graph: submit::single_shell_graph(vec!["/bin/sh".into(), "-c".into(), script]),
        cwd: "/".into(),
        env: CapturedEnv::default(),
        policies: Policies::default(),
        hooks: Hooks::default(),
    });
    let job = match roundtrip(&paths.socket_file, RequestBody::Submit { spec }).await? {
        Response::Submitted { job, .. } => job,
        other => bail!("expected Submitted, got {other:?}"),
    };

    // The observable outcome: the command really ran.
    for _ in 0..250 {
        if out_file.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let side_effect = std::fs::read_to_string(&out_file).context("step never ran")?;
    assert_eq!(side_effect, "ran\n");

    // …and the merged per-attempt log captured the output path's stdout
    // (empty here, but the file must exist with the §2.1 layout).
    let log = paths.logs_dir.join(format!("{job}/r1/run.1.log"));
    for _ in 0..100 {
        if log.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(log.exists(), "per-attempt log missing at {}", log.display());

    // List: the job shows up Done with its run Done (poll — the §3.3 step-2
    // commit can land just after the side effect).
    let mut listed = None;
    for _ in 0..100 {
        if let Response::JobList { jobs } =
            roundtrip(&paths.socket_file, RequestBody::List { all: false }).await?
            && let Some(entry) = jobs.iter().find(|j| j.id == job)
            && entry.status == JobStatus::Done
        {
            listed = Some(serde_json::to_string(&jobs)?);
            let run = entry.last_run.as_ref().context("no last run")?;
            assert_eq!(run.status, RunStatus::Done);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(listed.is_some(), "job never reached Done in list");

    daemon_task.abort();
    Ok(())
}

/// The full §3.4 arc over the real socket: a daemon dies mid-step, the next
/// daemon parks the run Held with its notification, and `continue` — by
/// job *name* — re-runs the step to completion.
#[tokio::test]
async fn crash_held_then_continue_completes() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let paths = temp_paths(dir.path())?;
    let store = Store::open(&paths.db_file).await?;

    // Daemon #1 "crashes" mid-step: intent committed (§3.3 step 1), then
    // nothing — exactly what a kill leaves behind.
    let out_file = dir.path().join("resumed.txt");
    let script = format!("echo resumed >> {}", out_file.display());
    let spec = JobSpec {
        name: Some("comeback".into()),
        schedule: Schedule::Once {
            at: Timestamp::now().checked_sub(SignedDuration::from_secs(1))?,
        },
        graph: cued::submit::single_shell_graph(vec!["/bin/sh".into(), "-c".into(), script]),
        cwd: "/".into(),
        env: CapturedEnv::default(),
        policies: Policies::default(),
        hooks: Hooks::default(),
    };
    let now = Timestamp::now();
    let (job, run, _) = store.submit_job(&spec, &now).await?;
    let run = run.expect("one-off run");
    store
        .begin_step(job, run, "run", &now)
        .await?
        .expect("claimed the waiting cursor");

    // Daemon #2 starts: reconciliation must park the run, not re-run it.
    let daemon_task = tokio::spawn(daemon::serve_with(paths.clone(), Config::default(), store.clone(), NoBusNotifier));
    await_socket(&paths.socket_file).await?;

    let mut held = false;
    for _ in 0..100 {
        let status: String =
            sqlx::query_scalar("SELECT status FROM runs WHERE job_id = ? AND id = ?")
                .bind(job.0)
                .bind(run.0)
                .fetch_one(store.pool())
                .await?;
        if status == "held" {
            held = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(held, "run never parked in Held");
    assert!(!out_file.exists(), "held step must not have been re-run");
    let notifications: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM notifications WHERE job_id = ?")
            .bind(job.0)
            .fetch_one(store.pool())
            .await?;
    assert_eq!(notifications, 1, "on_hold notification enqueued");

    // The human decides: continue, addressed by name (§2).
    match roundtrip(&paths.socket_file, RequestBody::Continue { job: "comeback".into() }).await? {
        Response::Rearmed { step, .. } => assert_eq!(step, "run"),
        other => bail!("expected Rearmed, got {other:?}"),
    }

    for _ in 0..250 {
        if out_file.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        std::fs::read_to_string(&out_file).context("step never re-ran after continue")?,
        "resumed\n"
    );

    daemon_task.abort();
    Ok(())
}

/// §3.4 Case 1, Abandon: a wait that expired beyond the grace while nobody
/// was looking marks the run Missed instead of running late.
#[tokio::test]
async fn abandon_marks_overdue_run_missed() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let paths = temp_paths(dir.path())?;
    let store = Store::open(&paths.db_file).await?;
    let daemon_task = tokio::spawn(daemon::serve_with(paths.clone(), Config::default(), store.clone(), NoBusNotifier));
    await_socket(&paths.socket_file).await?;

    let spec = Box::new(JobSpec {
        name: Some("stale".into()),
        schedule: Schedule::Once {
            // Well past the 60s missed grace.
            at: Timestamp::now().checked_sub(SignedDuration::from_secs(300))?,
        },
        graph: cued::submit::single_shell_graph(vec!["/bin/false".into()]),
        cwd: "/".into(),
        env: CapturedEnv::default(),
        policies: Policies {
            missed_wait: MissedWait::Abandon,
            ..Policies::default()
        },
        hooks: Hooks::default(),
    });
    let job = match roundtrip(&paths.socket_file, RequestBody::Submit { spec }).await? {
        Response::Submitted { job, .. } => job,
        other => bail!("expected Submitted, got {other:?}"),
    };

    let mut status = String::new();
    for _ in 0..250 {
        status = sqlx::query_scalar("SELECT status FROM runs WHERE job_id = ? AND id = 1")
            .bind(job.0)
            .fetch_one(store.pool())
            .await?;
        if status != "pending" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(status, "missed", "Abandon must mark, not run");
    // The step never executed (no attempt row), and the one-shot is Done.
    let attempts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM step_runs WHERE job_id = ?")
        .bind(job.0)
        .fetch_one(store.pool())
        .await?;
    assert_eq!(attempts, 0);

    daemon_task.abort();
    Ok(())
}

/// Recurrence over the real socket and real clock: an every-second job
/// accrues distinct runs, pause stops it, resume re-arms it forward.
#[tokio::test]
async fn recurring_job_fires_repeatedly_and_pauses() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let paths = temp_paths(dir.path())?;
    let store = Store::open(&paths.db_file).await?;
    let daemon_task = tokio::spawn(daemon::serve_with(paths.clone(), Config::default(), store.clone(), NoBusNotifier));
    await_socket(&paths.socket_file).await?;

    let out_file = dir.path().join("ticks.txt");
    let script = format!("echo tick >> {}", out_file.display());
    let spec = Box::new(JobSpec {
        name: Some("ticker".into()),
        schedule: Schedule::Every {
            interval: SignedDuration::from_secs(1),
            anchor: Timestamp::now(),
            until: None,
            count: None,
        },
        graph: cued::submit::single_shell_graph(vec!["/bin/sh".into(), "-c".into(), script]),
        cwd: "/".into(),
        env: CapturedEnv::default(),
        policies: Policies::default(),
        hooks: Hooks::default(),
    });
    let job = match roundtrip(&paths.socket_file, RequestBody::Submit { spec }).await? {
        Response::Submitted { job, run, .. } => {
            assert!(run.is_none(), "recurring: no run at submit");
            job
        }
        other => bail!("expected Submitted, got {other:?}"),
    };

    // §4.2: each firing creates its own Run; wait for two to complete.
    let done_runs = |store: Store| async move {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM runs WHERE job_id = ? AND status = 'done'",
        )
        .bind(job.0)
        .fetch_one(store.pool())
        .await
    };
    let mut done = 0;
    for _ in 0..400 {
        done = done_runs(store.clone()).await?;
        if done >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(done >= 2, "wanted 2 completed firings, saw {done}");

    // Pause: firing stops even though targets keep coming due.
    match roundtrip(&paths.socket_file, RequestBody::Pause { job: "ticker".into() }).await? {
        Response::Paused { job: paused } => assert_eq!(paused, job),
        other => bail!("expected Paused, got {other:?}"),
    }
    tokio::time::sleep(Duration::from_millis(1300)).await;
    let frozen = done_runs(store.clone()).await?;
    tokio::time::sleep(Duration::from_millis(2200)).await;
    assert_eq!(
        done_runs(store.clone()).await?,
        frozen,
        "no new completed runs while paused"
    );

    // Resume: re-armed to a future instant, firing continues.
    match roundtrip(&paths.socket_file, RequestBody::Resume { job: "ticker".into() }).await? {
        Response::Resumed { next_at, .. } => assert!(next_at.is_some()),
        other => bail!("expected Resumed, got {other:?}"),
    }
    let mut resumed_done = frozen;
    for _ in 0..400 {
        resumed_done = done_runs(store.clone()).await?;
        if resumed_done > frozen {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(resumed_done > frozen, "no firing after resume");

    daemon_task.abort();
    Ok(())
}

/// The reminder arc (§3.5): a Notify step "succeeds" on durable enqueue
/// even with no bus; the row waits, then lands when a transport appears.
#[tokio::test]
async fn reminder_enqueues_without_a_bus_then_delivers() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let paths = temp_paths(dir.path())?;
    let store = Store::open(&paths.db_file).await?;
    let daemon_task = tokio::spawn(daemon::serve_with(
        paths.clone(),
        Config::default(),
        store.clone(),
        NoBusNotifier,
    ));
    await_socket(&paths.socket_file).await?;

    // What `cued remind "1s ago" "stretch"` would submit.
    let spec = Box::new(JobSpec {
        name: Some("nudge".into()),
        schedule: Schedule::Once {
            at: Timestamp::now().checked_sub(SignedDuration::from_secs(1))?,
        },
        graph: cued::submit::single_notify_graph("stretch".into(), String::new()),
        cwd: "/".into(),
        env: CapturedEnv::default(),
        policies: Policies::default(),
        hooks: Hooks::default(),
    });
    let job = match roundtrip(&paths.socket_file, RequestBody::Submit { spec }).await? {
        Response::Submitted { job, .. } => job,
        other => bail!("expected Submitted, got {other:?}"),
    };

    // The run completes (enqueue = success, §3.2) while the row stays queued.
    let mut status = String::new();
    for _ in 0..250 {
        status = sqlx::query_scalar("SELECT status FROM runs WHERE job_id = ? AND id = 1")
            .bind(job.0)
            .fetch_one(store.pool())
            .await?;
        if status == "done" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(status, "done", "notify step succeeds on enqueue, busless or not");
    let pending = store.undelivered_notifications().await?;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].spec.title, "stretch");

    // A transport appears (≈ next login): the queued row lands.
    struct AlwaysDelivers;
    impl cued::notify::Notifier for AlwaysDelivers {
        async fn deliver(&self, _spec: &cued::model::NotifySpec) -> Result<bool> {
            Ok(true)
        }
    }
    let mut ledger = daemon::DeliveryLedger::default();
    let delivered =
        daemon::deliver_pending(&store, &AlwaysDelivers, &mut ledger, &Timestamp::now()).await?;
    assert_eq!(delivered, 1);
    assert!(store.undelivered_notifications().await?.is_empty());

    daemon_task.abort();
    Ok(())
}

#[tokio::test]
async fn proto_mismatch_is_a_designed_reply() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let paths = temp_paths(dir.path())?;
    let store = Store::open(&paths.db_file).await?;
    let daemon_task = tokio::spawn(daemon::serve_with(paths.clone(), Config::default(), store, NoBusNotifier));
    await_socket(&paths.socket_file).await?;

    let mut stream = UnixStream::connect(&paths.socket_file).await?;
    stream
        .write_all(b"{\"proto\":999,\"body\":{\"cmd\":\"ping\"}}\n")
        .await?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).await?;
    match serde_json::from_str::<Response>(&line)? {
        Response::ProtoMismatch { daemon_proto } => assert_eq!(daemon_proto, PROTO_VERSION),
        other => bail!("expected ProtoMismatch, got {other:?}"),
    }

    daemon_task.abort();
    Ok(())
}

/// §3.3: an internal failure in the step path must not cost the run its
/// place on the heap. The entry is popped before the step runs, so a daemon
/// that just logged the error would leave the run on a `waiting` cursor
/// nothing will ever pop again — firing never, silently, until a restart.
///
/// Error injection without a new seam: point a waiting cursor at a step id
/// the graph doesn't contain. `try_run_step` fails looking it up, every
/// time, so this drives the whole budget and lands on the terminal
/// behaviour — the run parked in `Held` where `cued list` shows it, with the
/// §3.5 notification enqueued.
#[tokio::test]
async fn a_step_that_keeps_failing_internally_ends_up_held_not_lost() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let paths = temp_paths(dir.path())?;
    let store = Store::open(&paths.db_file).await?;

    // A one-off far enough out that nothing fires while we corrupt it.
    let spec = JobSpec {
        name: Some("ghost".into()),
        schedule: Schedule::Once {
            at: Timestamp::now().checked_add(SignedDuration::from_secs(3600))?,
        },
        graph: submit::single_shell_graph(vec!["/bin/true".into()]),
        cwd: "/".into(),
        env: CapturedEnv::default(),
        policies: Policies::default(),
        hooks: Hooks::default(),
    };
    let now = Timestamp::now();
    let (job, run, _) = store.submit_job(&spec, &now).await?;
    let run = run.expect("a one-off gets its run at submit");

    // Send the cursor somewhere the graph can't follow, due now.
    sqlx::query(
        "UPDATE runs SET cursor_step = 'no-such-step', cursor_at = ?
         WHERE job_id = ? AND id = ?",
    )
    .bind(now.to_string())
    .bind(job.0)
    .bind(run.0)
    .execute(store.pool())
    .await?;

    // Reconciliation rebuilds the heap from that cursor, so the daemon arms
    // it, fails, and retries on its own from here.
    let daemon_task = tokio::spawn(daemon::serve_with(
        paths.clone(),
        Config::default(),
        store.clone(),
        NoBusNotifier,
    ));
    await_socket(&paths.socket_file).await?;

    // The budget is five tries over ~7.75s of backoff; allow slack.
    let mut held = false;
    for _ in 0..120 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let status: String =
            sqlx::query_scalar("SELECT cursor_kind FROM runs WHERE job_id = ? AND id = ?")
                .bind(job.0)
                .bind(run.0)
                .fetch_one(store.pool())
                .await?;
        if status == "held" {
            held = true;
            break;
        }
    }
    assert!(held, "the run should be parked in Held, not silently stranded");

    let (run_status, reason): (String, Option<String>) =
        sqlx::query_as("SELECT status, held_reason FROM runs WHERE job_id = ? AND id = ?")
            .bind(job.0)
            .bind(run.0)
            .fetch_one(store.pool())
            .await?;
    assert_eq!(run_status, "held");
    // §3.3: a distinct park reason — this wasn't an interrupt.
    assert_eq!(reason.as_deref(), Some("errored"));

    // §3.5: parked and silent must be impossible.
    let queued: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM notifications WHERE job_id = ? AND delivered_at IS NULL",
    )
    .bind(job.0)
    .fetch_one(store.pool())
    .await?;
    assert!(queued >= 1, "the on_hold notification should be enqueued");

    daemon_task.abort();
    Ok(())
}

/// §4.2 + §2.2: cancel stops the job *and* terminates what's running — and
/// "terminates" means the process group, not just the direct child. A
/// `cued cancel` that left the step's children alive would be the §2.2 leak
/// with a friendlier name.
#[tokio::test]
async fn cancel_kills_the_running_process_group_and_ends_the_job() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let paths = temp_paths(dir.path())?;
    let store = Store::open(&paths.db_file).await?;
    let daemon_task = tokio::spawn(daemon::serve_with(
        paths.clone(),
        Config::default(),
        store.clone(),
        NoBusNotifier,
    ));
    await_socket(&paths.socket_file).await?;

    // A long step that also backgrounds a grandchild: if only the direct
    // child were signalled, the grandchild would outlive the cancel and
    // write the marker.
    let marker = dir.path().join("grandchild-ran");
    let script = format!("(sleep 3; touch {}) & sleep 30", marker.display());
    let spec = Box::new(JobSpec {
        name: Some("longrunner".into()),
        schedule: Schedule::Once {
            at: Timestamp::now().checked_sub(SignedDuration::from_secs(1))?,
        },
        graph: submit::single_shell_graph(vec!["/bin/sh".into(), "-c".into(), script]),
        cwd: "/".into(),
        env: CapturedEnv::default(),
        policies: Policies::default(),
        hooks: Hooks::default(),
    });
    let job = match roundtrip(&paths.socket_file, RequestBody::Submit { spec }).await? {
        Response::Submitted { job, .. } => job,
        other => bail!("expected Submitted, got {other:?}"),
    };

    // Wait until the step is genuinely executing — cancelling a run that
    // hasn't spawned yet would prove nothing about the kill path.
    let mut running = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let kind: String =
            sqlx::query_scalar("SELECT cursor_kind FROM runs WHERE job_id = ?")
                .bind(job.0)
                .fetch_one(store.pool())
                .await?;
        if kind == "running" {
            running = true;
            break;
        }
    }
    assert!(running, "the step never started; nothing to cancel");

    let runs = match roundtrip(
        &paths.socket_file,
        RequestBody::Cancel { job: job.to_string() },
    )
    .await?
    {
        Response::JobCancelled { job: cancelled, runs } => {
            assert_eq!(cancelled, job);
            runs
        }
        other => bail!("expected JobCancelled, got {other:?}"),
    };
    assert_eq!(runs.len(), 1, "the live run should be named: {runs:?}");

    // §4.2: the run is Cancelled and the job stops for good.
    let (run_status, job_status, next): (String, String, Option<String>) = sqlx::query_as(
        "SELECT r.status, j.status, j.next_fire_at FROM runs r
         JOIN jobs j ON j.id = r.job_id WHERE r.job_id = ?",
    )
    .bind(job.0)
    .fetch_one(store.pool())
    .await?;
    assert_eq!(run_status, "cancelled");
    assert_eq!(job_status, "cancelled");
    assert!(next.is_none(), "a cancelled job must not stay armed");

    // §2.2: the whole group is gone, grandchild included.
    tokio::time::sleep(Duration::from_millis(3500)).await;
    assert!(!marker.exists(), "a grandchild outlived the cancel");

    // The step it was cancelled during must not have walked the run forward
    // or re-opened it — cancel is terminal.
    let after: String = sqlx::query_scalar("SELECT cursor_kind FROM runs WHERE job_id = ?")
        .bind(job.0)
        .fetch_one(store.pool())
        .await?;
    assert_eq!(after, "done", "the cancelled run was resurrected");

    daemon_task.abort();
    Ok(())
}

/// §2.1 + §5.1: the daemon hands back a manifest and the client reads the
/// per-attempt files itself — no log bytes ever cross the socket. This
/// covers the whole round trip: a run that produced output, the attempt
/// metadata `cued logs` renders its headers from, and the file landing
/// exactly where `Paths::step_log` says it will.
#[tokio::test]
async fn logs_manifest_points_at_files_the_client_can_read() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let paths = temp_paths(dir.path())?;
    let store = Store::open(&paths.db_file).await?;
    let daemon_task = tokio::spawn(daemon::serve_with(
        paths.clone(),
        Config::default(),
        store.clone(),
        NoBusNotifier,
    ));
    await_socket(&paths.socket_file).await?;

    let spec = Box::new(JobSpec {
        name: Some("noisy".into()),
        schedule: Schedule::Once {
            at: Timestamp::now().checked_sub(SignedDuration::from_secs(1))?,
        },
        graph: submit::single_shell_graph(vec![
            "/bin/sh".into(),
            "-c".into(),
            "echo to-stdout; echo to-stderr >&2; exit 7".into(),
        ]),
        cwd: "/".into(),
        env: CapturedEnv::default(),
        policies: Policies::default(),
        hooks: Hooks::default(),
    });
    let job = match roundtrip(&paths.socket_file, RequestBody::Submit { spec }).await? {
        Response::Submitted { job, .. } => job,
        other => bail!("expected Submitted, got {other:?}"),
    };

    // Wait for the run to finish so the attempt has an outcome.
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let kind: String = sqlx::query_scalar("SELECT cursor_kind FROM runs WHERE job_id = ?")
            .bind(job.0)
            .fetch_one(store.pool())
            .await?;
        if kind == "done" {
            break;
        }
    }

    let (run, attempts) = match roundtrip(
        &paths.socket_file,
        RequestBody::Logs { job: job.to_string(), run: None, step: None, attempt: None },
    )
    .await?
    {
        Response::LogManifest { run, attempts, .. } => (run, attempts),
        other => bail!("expected LogManifest, got {other:?}"),
    };

    assert_eq!(attempts.len(), 1, "one step, one attempt: {attempts:?}");
    let entry = &attempts[0];
    assert_eq!(entry.step, "run");
    assert_eq!(entry.attempt, 1);
    assert_eq!(entry.exit_code, Some(7), "the header's outcome comes from here");
    assert!(!entry.timed_out);
    assert!(entry.ended_at.is_some(), "a finished attempt is not followable");

    // The client derives the path from the manifest; it must be the one the
    // daemon actually wrote, and §2.1's merged view holds both streams.
    let path = paths.step_log(job, run, &entry.step, entry.attempt);
    let captured = std::fs::read_to_string(&path)
        .with_context(|| format!("the manifest pointed at {}", path.display()))?;
    assert!(captured.contains("to-stdout"), "{captured:?}");
    assert!(captured.contains("to-stderr"), "merged view is missing stderr: {captured:?}");

    // §7.5: captured output is as private as the store.
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);

    // §10.3: a run that doesn't exist is an error, not an empty answer.
    match roundtrip(
        &paths.socket_file,
        RequestBody::Logs { job: job.to_string(), run: Some(99), step: None, attempt: None },
    )
    .await?
    {
        Response::Error { message } => assert!(message.contains("no such run"), "{message}"),
        other => bail!("expected an error for a missing run, got {other:?}"),
    }

    daemon_task.abort();
    Ok(())
}

/// §3.2: "`deadline` trips mid-step: the running step is killed via the §2.2
/// TERM→grace→KILL sequence, the run ends `Failed` with the reason recorded
/// as `deadline`, and `on_failure` (if set) fires."
#[tokio::test]
async fn a_deadline_kills_the_running_step_and_fails_the_run() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let paths = temp_paths(dir.path())?;
    let store = Store::open(&paths.db_file).await?;
    let daemon_task = tokio::spawn(daemon::serve_with(
        paths.clone(),
        Config::default(),
        store.clone(),
        NoBusNotifier,
    ));
    await_socket(&paths.socket_file).await?;

    // A step far longer than the budget, which also backgrounds a
    // grandchild: the deadline must reach the whole process group, not just
    // the direct child (§2.2).
    let marker = dir.path().join("grandchild-ran");
    let script = format!("echo started; (sleep 3; touch {}) & sleep 30", marker.display());
    let spec = Box::new(JobSpec {
        name: Some("budget".into()),
        schedule: Schedule::Once {
            at: Timestamp::now().checked_sub(SignedDuration::from_secs(1))?,
        },
        graph: submit::single_shell_graph(vec!["/bin/sh".into(), "-c".into(), script]),
        cwd: "/".into(),
        env: CapturedEnv::default(),
        policies: Policies {
            deadline: Some(SignedDuration::from_secs(1)),
            ..Policies::default()
        },
        hooks: Hooks {
            // §3.2: on_failure fires when the deadline ends the run.
            on_failure: Some(cued::model::NotifySpec {
                title: "over budget".into(),
                body: "the run hit its deadline".into(),
            }),
            ..Hooks::default()
        },
    });
    let job = match roundtrip(&paths.socket_file, RequestBody::Submit { spec }).await? {
        Response::Submitted { job, .. } => job,
        other => bail!("expected Submitted, got {other:?}"),
    };

    let mut ended = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let kind: String = sqlx::query_scalar("SELECT cursor_kind FROM runs WHERE job_id = ?")
            .bind(job.0)
            .fetch_one(store.pool())
            .await?;
        if kind == "done" {
            ended = true;
            break;
        }
    }
    assert!(ended, "the deadline never ended the run");

    let (status, reason): (String, Option<String>) =
        sqlx::query_as("SELECT status, fail_reason FROM runs WHERE job_id = ?")
            .bind(job.0)
            .fetch_one(store.pool())
            .await?;
    assert_eq!(status, "failed", "a blown deadline is a failure, not a cancel");
    assert_eq!(reason.as_deref(), Some("deadline"), "the reason must be on record");

    // §2.2: the whole group went, grandchild included.
    tokio::time::sleep(Duration::from_millis(3500)).await;
    assert!(!marker.exists(), "a grandchild outlived the deadline");

    // The killed attempt keeps what it managed to emit — `cued logs` should
    // still be able to show why the step was taking so long.
    let attempt_closed: Option<String> =
        sqlx::query_scalar("SELECT ended_at FROM step_runs WHERE job_id = ?")
            .bind(job.0)
            .fetch_one(store.pool())
            .await?;
    assert!(attempt_closed.is_some(), "the attempt row should be closed");

    let hooked: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM notifications WHERE job_id = ? AND title = 'over budget'",
    )
    .bind(job.0)
    .fetch_one(store.pool())
    .await?;
    assert_eq!(hooked, 1, "§3.2: on_failure fires on a deadline");

    daemon_task.abort();
    Ok(())
}

/// §3.2: "the deadline clock starts at `started_at` of the run" — so it can
/// also run out *between* steps, while the run sits on a sleep-edge. A step
/// that will never run must leave no attempt behind claiming it did.
#[tokio::test]
async fn a_deadline_that_expires_during_a_wait_stops_the_next_step() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let paths = temp_paths(dir.path())?;
    let store = Store::open(&paths.db_file).await?;
    let daemon_task = tokio::spawn(daemon::serve_with(
        paths.clone(),
        Config::default(),
        store.clone(),
        NoBusNotifier,
    ));
    await_socket(&paths.socket_file).await?;

    // step one, then a 2s sleep-edge — but the budget is only 1s.
    let ran = dir.path().join("second-step-ran");
    let graph = submit::chain_graph(
        "true",
        &[submit::Link::ThenAfter(
            "2s".into(),
            format!("touch {}", ran.display()),
        )],
        submit::ChainFailure::Stop,
    )?;
    let spec = Box::new(JobSpec {
        name: Some("wait".into()),
        schedule: Schedule::Once {
            at: Timestamp::now().checked_sub(SignedDuration::from_secs(1))?,
        },
        graph,
        cwd: "/".into(),
        env: CapturedEnv::default(),
        policies: Policies {
            deadline: Some(SignedDuration::from_secs(1)),
            ..Policies::default()
        },
        hooks: Hooks::default(),
    });
    let job = match roundtrip(&paths.socket_file, RequestBody::Submit { spec }).await? {
        Response::Submitted { job, .. } => job,
        other => bail!("expected Submitted, got {other:?}"),
    };

    let mut ended = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let kind: String = sqlx::query_scalar("SELECT cursor_kind FROM runs WHERE job_id = ?")
            .bind(job.0)
            .fetch_one(store.pool())
            .await?;
        if kind == "done" {
            ended = true;
            break;
        }
    }
    assert!(ended, "the run should have ended when its budget ran out");

    let reason: Option<String> =
        sqlx::query_scalar("SELECT fail_reason FROM runs WHERE job_id = ?")
            .bind(job.0)
            .fetch_one(store.pool())
            .await?;
    assert_eq!(reason.as_deref(), Some("deadline"));
    assert!(!ran.exists(), "the second step ran despite the deadline");

    // Only step1 ever started: the check happens before the claim, so the
    // step that never ran has no attempt row inventing one.
    let attempts: Vec<String> =
        sqlx::query_scalar("SELECT step_id FROM step_runs WHERE job_id = ? ORDER BY step_id")
            .bind(job.0)
            .fetch_all(store.pool())
            .await?;
    assert_eq!(attempts, ["step1"], "an attempt was recorded for a step that never ran");

    daemon_task.abort();
    Ok(())
}

/// §6.3 holds against the *wire*, not just the CLI. `timeparse` constrains
/// what a user can type and what a §6.2 file can say; a client speaking the
/// §5.1 protocol directly is under no such limit, and the daemon is the
/// trust boundary (§7.3 authenticates *who*, not *what*). These schedules
/// are unreachable through any front-end and must still be refused.
#[tokio::test]
async fn the_daemon_refuses_a_malformed_schedule_from_the_wire() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let paths = temp_paths(dir.path())?;
    let store = Store::open(&paths.db_file).await?;
    let daemon_task = tokio::spawn(daemon::serve_with(
        paths.clone(),
        Config::default(),
        store.clone(),
        NoBusNotifier,
    ));
    await_socket(&paths.socket_file).await?;

    let anchor = Timestamp::now();
    let submit_with = |schedule: Schedule| {
        let paths = paths.clone();
        async move {
            let spec = Box::new(JobSpec {
                name: None,
                schedule,
                graph: submit::single_shell_graph(vec!["/bin/true".into()]),
                cwd: "/".into(),
                env: CapturedEnv::default(),
                policies: Policies::default(),
                hooks: Hooks::default(),
            });
            roundtrip(&paths.socket_file, RequestBody::Submit { spec }).await
        }
    };

    // A zero interval divides by zero in §4.2's catch-up arithmetic.
    let zero = Schedule::Every {
        interval: SignedDuration::from_secs(0),
        anchor,
        until: None,
        count: None,
    };
    match submit_with(zero).await? {
        Response::Error { message } => assert!(message.contains("positive"), "{message}"),
        other => bail!("a zero interval was accepted: {other:?}"),
    }

    // A rule that can match no date at all.
    let unmatchable = Schedule::Calendar {
        spec: cued::model::CalendarSpec::Weekly {
            days: Vec::new(),
            at: jiff::civil::time(9, 0, 0, 0),
        },
        zone: "UTC".into(),
        until: None,
        count: None,
    };
    match submit_with(unmatchable).await? {
        Response::Error { message } => assert!(message.contains("weekday"), "{message}"),
        other => bail!("an empty weekly rule was accepted: {other:?}"),
    }

    // Nothing of the sort reached the store.
    let jobs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs")
        .fetch_one(store.pool())
        .await?;
    assert_eq!(jobs, 0, "a refused submission must leave no job behind");

    // And the daemon is still serving — a rejection is an answer, not a fall.
    let good = Schedule::Once {
        at: anchor.checked_add(SignedDuration::from_hours(1))?,
    };
    assert!(matches!(
        submit_with(good).await?,
        Response::Submitted { .. }
    ));

    daemon_task.abort();
    Ok(())
}

/// codex #6: `fire_job_task` had no retry budget, so a transient internal
/// error stranded the whole *schedule* — the arm was already popped, and
/// nothing re-armed it until a daemon restart. This is the same hazard §3.3
/// describes for a due step, and a recurring job makes it the worse of the
/// two: a stranded step loses one run, a stranded firing loses every run
/// from then on.
///
/// Error injection with no new seam: corrupt the job's stored `schedule`
/// JSON. `fire_job` loads the job on its very first await, so the failure is
/// deterministic and repeatable — which drives the whole budget and lands on
/// the terminal behaviour, a notification the user can actually see.
/// (Corrupting the *graph* is not enough: a graph naming a missing step
/// still deserializes, so the firing succeeds and the failure lands in the
/// step path instead, which has had its own retry budget since §3.3.)
#[tokio::test]
async fn a_firing_that_keeps_failing_warns_instead_of_going_quiet() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let paths = temp_paths(dir.path())?;
    let store = Store::open(&paths.db_file).await?;

    let now = Timestamp::now();
    let spec = JobSpec {
        name: Some("doomed".into()),
        // A one-second cadence so the first arm is due almost at once:
        // `next_fire` is always strictly in the future at submit, so a long
        // interval would spend the whole test just waiting to start.
        schedule: Schedule::Every {
            interval: SignedDuration::from_secs(1),
            anchor: now,
            until: None,
            count: None,
        },
        graph: submit::single_shell_graph(vec!["/bin/true".into()]),
        cwd: "/".into(),
        env: CapturedEnv::default(),
        policies: Policies::default(),
        hooks: Hooks::default(),
    };
    let (job, _, _) = store.submit_job(&spec, &now).await?;

    // Corrupt the stored schedule so every load-and-fire pass fails alike.
    sqlx::query("UPDATE jobs SET schedule = ? WHERE id = ?")
        .bind("{not valid json")
        .bind(job.0)
        .execute(store.pool())
        .await?;

    let daemon_task = tokio::spawn(daemon::serve_with(
        paths.clone(),
        Config::default(),
        store.clone(),
        NoBusNotifier,
    ));
    await_socket(&paths.socket_file).await?;

    // Five tries over ~7.75s of backoff, then the warning; allow slack.
    let mut warned = false;
    for _ in 0..120 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM notifications WHERE job_id = ? AND run_id IS NULL",
        )
        .bind(job.0)
        .fetch_one(store.pool())
        .await?;
        if n > 0 {
            warned = true;
            break;
        }
    }
    assert!(warned, "the schedule went quiet with nothing to show for it");

    let (title, body): (String, String) = sqlx::query_as(
        "SELECT title, body FROM notifications WHERE job_id = ? AND run_id IS NULL",
    )
    .bind(job.0)
    .fetch_one(store.pool())
    .await?;
    assert!(title.contains("stopped firing"), "{title}");
    assert!(body.contains("restart"), "the warning should say how to recover: {body}");

    // §5.3: the schedule is still on record, so a restart re-arms it — the
    // store is the truth and the heap is derived.
    let next: Option<String> = sqlx::query_scalar("SELECT next_fire_at FROM jobs WHERE id = ?")
        .bind(job.0)
        .fetch_one(store.pool())
        .await?;
    assert!(next.is_some(), "the job must keep its schedule to be recoverable");

    daemon_task.abort();
    Ok(())
}

/// codex #4: a firing parked in the §4.2 queue slot starts when the live run
/// ends — but only a *normal* step close used to drain it. A run ended by a
/// deadline left the slot filled, and since `finish_job_if_exhausted`
/// refuses to finish a job with something queued, the job sat `Active`
/// forever with nothing on the heap to move it.
///
/// The schedule below is capped at two firings on purpose. While a recurring
/// job keeps firing, the *next* firing drains the slot and the stranding
/// heals itself — so a test with an open-ended cadence passes either way and
/// proves nothing. It is when no further firing is coming that the slot has
/// nobody left to release it.
#[tokio::test]
async fn a_deadline_ending_a_run_still_releases_the_queued_firing() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let paths = temp_paths(dir.path())?;
    let store = Store::open(&paths.db_file).await?;
    let daemon_task = tokio::spawn(daemon::serve_with(
        paths.clone(),
        Config::default(),
        store.clone(),
        NoBusNotifier,
    ));
    await_socket(&paths.socket_file).await?;

    // Two firings only, overlap=Queue, 1s budget: firing 2 queues behind
    // run 1, run 1 then blows its deadline, and nothing further is coming.
    let script = "sleep 30".to_string();
    let spec = Box::new(JobSpec {
        name: Some("queued".into()),
        schedule: Schedule::Every {
            interval: SignedDuration::from_secs(1),
            anchor: Timestamp::now(),
            until: None,
            count: Some(2),
        },
        graph: submit::single_shell_graph(vec!["/bin/sh".into(), "-c".into(), script]),
        cwd: "/".into(),
        env: CapturedEnv::default(),
        policies: Policies {
            overlap: cued::model::Overlap::Queue,
            deadline: Some(SignedDuration::from_secs(1)),
            ..Policies::default()
        },
        hooks: Hooks::default(),
    });
    let job = match roundtrip(&paths.socket_file, RequestBody::Submit { spec }).await? {
        Response::Submitted { job, .. } => job,
        other => bail!("expected Submitted, got {other:?}"),
    };

    // Wait for a second run to exist — that is the queue having drained.
    let mut drained = false;
    for _ in 0..120 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let runs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM runs WHERE job_id = ?")
            .bind(job.0)
            .fetch_one(store.pool())
            .await?;
        if runs >= 2 {
            drained = true;
            break;
        }
    }
    assert!(drained, "the queued firing was stranded by the deadline");

    let (queued, next): (Option<String>, Option<String>) =
        sqlx::query_as("SELECT queued_at, next_fire_at FROM jobs WHERE id = ?")
            .bind(job.0)
            .fetch_one(store.pool())
            .await?;
    assert!(queued.is_none(), "the slot should be empty once drained");
    assert!(next.is_none(), "the budget was two firings — nothing more is coming");

    roundtrip(&paths.socket_file, RequestBody::Cancel { job: job.to_string() }).await?;
    daemon_task.abort();
    Ok(())
}

/// §5.3 write path under a burst: many one-offs due at the same instant all
/// claim, run and close. With several write connections, the claims' deferred
/// read-then-write transactions failed each other with `SQLITE_BUSY` instead
/// of waiting, the jitter-free retries collided again, and all but about one
/// per round ended up Held. The single writer connection queues them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_burst_of_simultaneous_one_offs_all_run() -> Result<()> {
    const BURST: i64 = 100;
    let dir = tempfile::tempdir()?;
    let paths = temp_paths(dir.path())?;
    let store = Store::open(&paths.db_file).await?;
    let daemon_task = tokio::spawn(daemon::serve_with(
        paths.clone(),
        Config::default(),
        store.clone(),
        NoBusNotifier,
    ));
    await_socket(&paths.socket_file).await?;

    // Far enough out that every submit lands before the instant arrives, so
    // they all come due together rather than trickling in.
    let at = Timestamp::now().checked_add(SignedDuration::from_secs(3))?;
    for n in 0..BURST {
        let spec = Box::new(JobSpec {
            name: Some(format!("burst-{n}")),
            schedule: Schedule::Once { at },
            graph: submit::single_shell_graph(vec!["/bin/true".into()]),
            cwd: "/".into(),
            env: CapturedEnv::default(),
            policies: Policies::default(),
            hooks: Hooks::default(),
        });
        match roundtrip(&paths.socket_file, RequestBody::Submit { spec }).await? {
            Response::Submitted { .. } => {}
            other => bail!("expected Submitted, got {other:?}"),
        }
    }
    assert!(Timestamp::now() < at, "submitting took too long to make a burst");

    let mut done = 0;
    for _ in 0..120 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        done = sqlx::query_scalar("SELECT COUNT(*) FROM runs WHERE cursor_kind = 'done'")
            .fetch_one(store.pool())
            .await?;
        if done == BURST {
            break;
        }
    }
    let held: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM runs WHERE status = 'held'")
        .fetch_one(store.pool())
        .await?;
    assert_eq!(held, 0, "a burst should queue for the writer, not end up Held");
    assert_eq!(done, BURST, "every job in the burst should have run to completion");

    daemon_task.abort();
    Ok(())
}

/// Fault injection for the §3.3 step-2 close: every attempt to record a
/// step's end fails until the trigger is dropped.
async fn break_step_close(store: &Store) -> Result<()> {
    sqlx::query(
        "CREATE TRIGGER injected_close_failure BEFORE UPDATE OF ended_at ON step_runs
         BEGIN SELECT RAISE(FAIL, 'injected close failure'); END",
    )
    .execute(store.pool())
    .await?;
    Ok(())
}

/// Submit a one-off that is due now and runs `sleep 1`, and wait until the
/// daemon has claimed it — so the close is still ahead of it.
async fn claimed_sleeper(
    paths: &Paths,
    store: &Store,
) -> Result<(cued::model::JobId, cued::model::RunId)> {
    let spec = Box::new(JobSpec {
        name: Some("sleeper".into()),
        schedule: Schedule::Once { at: Timestamp::now() },
        graph: submit::single_shell_graph(vec!["/bin/sleep".into(), "1".into()]),
        cwd: "/".into(),
        env: CapturedEnv::default(),
        policies: Policies::default(),
        hooks: Hooks::default(),
    });
    let job = match roundtrip(&paths.socket_file, RequestBody::Submit { spec }).await? {
        Response::Submitted { job, .. } => job,
        other => bail!("expected Submitted, got {other:?}"),
    };
    for _ in 0..200 {
        let claimed: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM runs WHERE job_id = ? AND cursor_kind = 'running'",
        )
        .bind(job.0)
        .fetch_optional(store.pool())
        .await?;
        if let Some(run) = claimed {
            return Ok((job, cued::model::RunId(run)));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    bail!("the step was never claimed")
}

async fn poll_cursor(
    store: &Store,
    job: cued::model::JobId,
    run: cued::model::RunId,
    want: &str,
) -> Result<(String, String)> {
    let mut last = (String::new(), String::new());
    for _ in 0..80 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        last = sqlx::query_as("SELECT cursor_kind, status FROM runs WHERE job_id = ? AND id = ?")
            .bind(job.0)
            .bind(run.0)
            .fetch_one(store.pool())
            .await?;
        if last.0 == want {
            break;
        }
    }
    Ok(last)
}

/// §3.3 step 2 under a transient store failure: the process has ended, the
/// close fails, and then the store recovers. The close must be retried with
/// the outcome it already has — re-arming the whole step instead found the
/// cursor already `running`, claimed nothing, and left the run stuck there.
#[tokio::test]
async fn a_close_that_fails_briefly_still_lands() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let paths = temp_paths(dir.path())?;
    let store = Store::open(&paths.db_file).await?;
    let daemon_task = tokio::spawn(daemon::serve_with(
        paths.clone(),
        Config::default(),
        store.clone(),
        NoBusNotifier,
    ));
    await_socket(&paths.socket_file).await?;

    let (job, run) = claimed_sleeper(&paths, &store).await?;
    break_step_close(&store).await?;
    // The step ends ~1s in; the close then fails for about a second —
    // two or three tries — before the store recovers.
    tokio::time::sleep(Duration::from_millis(2000)).await;
    sqlx::query("DROP TRIGGER injected_close_failure")
        .execute(store.pool())
        .await?;

    let (cursor, status) = poll_cursor(&store, job, run, "done").await?;
    assert_eq!(cursor, "done", "the close never landed; the run is stranded");
    assert_eq!(status, "done", "the step succeeded and the run should say so");

    daemon_task.abort();
    Ok(())
}

/// …and when the store never recovers, the run is parked in Held with its
/// notification — not left `running` with nothing behind it.
#[tokio::test]
async fn a_close_that_keeps_failing_parks_the_run() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let paths = temp_paths(dir.path())?;
    let store = Store::open(&paths.db_file).await?;
    let daemon_task = tokio::spawn(daemon::serve_with(
        paths.clone(),
        Config::default(),
        store.clone(),
        NoBusNotifier,
    ));
    await_socket(&paths.socket_file).await?;

    let (job, run) = claimed_sleeper(&paths, &store).await?;
    break_step_close(&store).await?;

    let (cursor, status) = poll_cursor(&store, job, run, "held").await?;
    assert_eq!(cursor, "held", "a close that never lands should park the run");
    assert_eq!(status, "held");
    let reason: Option<String> =
        sqlx::query_scalar("SELECT held_reason FROM runs WHERE job_id = ? AND id = ?")
            .bind(job.0)
            .bind(run.0)
            .fetch_one(store.pool())
            .await?;
    assert_eq!(reason.as_deref(), Some("errored"));

    daemon_task.abort();
    Ok(())
}

/// §4.2 pause, mid-workflow: a step can't be safely frozen, so the one
/// already running finishes and its outcome is recorded like any other —
/// pause only stops the *next* step from starting. Resume picks the run up
/// from that recorded cursor.
#[tokio::test]
async fn pause_lets_the_running_step_finish_and_holds_the_next() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let paths = temp_paths(dir.path())?;
    let store = Store::open(&paths.db_file).await?;
    let daemon_task = tokio::spawn(daemon::serve_with(
        paths.clone(),
        Config::default(),
        store.clone(),
        NoBusNotifier,
    ));
    await_socket(&paths.socket_file).await?;

    let ran = dir.path().join("second-step-ran");
    let graph = submit::chain_graph(
        "sleep 1",
        &[submit::Link::ThenAfter("1s".into(), format!("touch {}", ran.display()))],
        submit::ChainFailure::Stop,
    )?;
    let spec = Box::new(JobSpec {
        name: Some("paused-workflow".into()),
        schedule: Schedule::Once { at: Timestamp::now() },
        graph,
        cwd: "/".into(),
        env: CapturedEnv::default(),
        policies: Policies::default(),
        hooks: Hooks::default(),
    });
    let job = match roundtrip(&paths.socket_file, RequestBody::Submit { spec }).await? {
        Response::Submitted { job, .. } => job,
        other => bail!("expected Submitted, got {other:?}"),
    };
    let cursor = || async {
        let row: (String, String) =
            sqlx::query_as("SELECT cursor_kind, status FROM runs WHERE job_id = ?")
                .bind(job.0)
                .fetch_one(store.pool())
                .await?;
        anyhow::Ok(row)
    };

    // Pause while the first step is running.
    for _ in 0..120 {
        if cursor().await?.0 == "running" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(cursor().await?.0, "running", "the first step never started");
    match roundtrip(&paths.socket_file, RequestBody::Pause { job: job.to_string() }).await? {
        Response::Paused { .. } => {}
        other => bail!("expected Paused, got {other:?}"),
    }

    // Well past the first step's end and the 1s wait after it.
    tokio::time::sleep(Duration::from_millis(3000)).await;
    let closed: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM step_runs WHERE job_id = ? AND ended_at IS NOT NULL AND exit_code = 0",
    )
    .bind(job.0)
    .fetch_one(store.pool())
    .await?;
    assert_eq!(closed, 1, "the step running at pause must finish and be recorded");
    assert_eq!(cursor().await?.0, "waiting", "the run should sit on its next step");
    assert!(!ran.exists(), "the next step started while the job was paused");

    match roundtrip(&paths.socket_file, RequestBody::Resume { job: job.to_string() }).await? {
        Response::Resumed { .. } => {}
        other => bail!("expected Resumed, got {other:?}"),
    }
    for _ in 0..100 {
        if cursor().await?.0 == "done" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(cursor().await?, ("done".into(), "done".into()));
    assert!(ran.exists(), "resume should run the held-back step");

    daemon_task.abort();
    Ok(())
}

/// §4.2 + §3.4 Case 1: time spent paused is the user's "not yet", not a
/// missed moment. A `missed_wait = abandon` run whose next step came due
/// long ago *during a pause* must run on resume, not be marked Missed.
#[tokio::test]
async fn resume_does_not_count_paused_time_as_missed() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let paths = temp_paths(dir.path())?;
    let store = Store::open(&paths.db_file).await?;
    let daemon_task = tokio::spawn(daemon::serve_with(
        paths.clone(),
        Config::default(),
        store.clone(),
        NoBusNotifier,
    ));
    await_socket(&paths.socket_file).await?;

    let ran = dir.path().join("second-step-ran");
    let graph = submit::chain_graph(
        "true",
        &[submit::Link::ThenAfter("1h".into(), format!("touch {}", ran.display()))],
        submit::ChainFailure::Stop,
    )?;
    let spec = Box::new(JobSpec {
        name: Some("long-pause".into()),
        schedule: Schedule::Once { at: Timestamp::now() },
        graph,
        cwd: "/".into(),
        env: CapturedEnv::default(),
        policies: Policies { missed_wait: MissedWait::Abandon, ..Policies::default() },
        hooks: Hooks::default(),
    });
    let job = match roundtrip(&paths.socket_file, RequestBody::Submit { spec }).await? {
        Response::Submitted { job, .. } => job,
        other => bail!("expected Submitted, got {other:?}"),
    };
    let cursor = || async {
        let row: (String, String) =
            sqlx::query_as("SELECT cursor_kind, status FROM runs WHERE job_id = ?")
                .bind(job.0)
                .fetch_one(store.pool())
                .await?;
        anyhow::Ok(row)
    };
    // Wait for the *second* waiting cursor: a one-off is born waiting, so
    // the wait that matters is the one after the first step has closed.
    let first_closed = || async {
        let closed: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM step_runs WHERE job_id = ? AND ended_at IS NOT NULL",
        )
        .bind(job.0)
        .fetch_one(store.pool())
        .await?;
        anyhow::Ok(closed)
    };
    for _ in 0..120 {
        if first_closed().await? == 1 && cursor().await?.0 == "waiting" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(first_closed().await?, 1, "the first step never closed");
    assert_eq!(cursor().await?.0, "waiting", "the run never reached its 1h wait");

    roundtrip(&paths.socket_file, RequestBody::Pause { job: job.to_string() }).await?;
    // What a long pause leaves behind: the wait came due five minutes ago —
    // well past the missed grace — while the job sat paused.
    let due = Timestamp::now().checked_sub(SignedDuration::from_mins(5))?;
    sqlx::query("UPDATE runs SET cursor_at = ? WHERE job_id = ?")
        .bind(due.to_string())
        .bind(job.0)
        .execute(store.pool())
        .await?;

    roundtrip(&paths.socket_file, RequestBody::Resume { job: job.to_string() }).await?;
    for _ in 0..100 {
        if cursor().await?.0 == "done" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        cursor().await?,
        ("done".into(), "done".into()),
        "resume should run the step, not abandon it as missed"
    );
    assert!(ran.exists(), "the held-back step never ran");

    daemon_task.abort();
    Ok(())
}
