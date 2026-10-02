//! The backend against a real daemon on temp paths (DESIGN.md §5.1): it
//! follows a run to the end on notices alone, goes quiet when nothing
//! changes, and picks up a daemon that starts after the window did.
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::{Duration, Instant};

use cued::config::Config;
use cued::model::{CapturedEnv, Hooks, JobSpec, JobStatus, Policies, Schedule};
use cued::paths::Paths;
use cued::proto::{RequestBody, Response};
use cued::store::Store;
use cued_gui::backend::{Backend, Command, Detail, Link, Update};
use jiff::{SignedDuration, Timestamp};

struct NoBusNotifier;

impl cued::notify::Notifier for NoBusNotifier {
    async fn deliver(&self, _spec: &cued::model::NotifySpec) -> anyhow::Result<bool> {
        Ok(false)
    }
}

fn temp_paths(root: &Path) -> Paths {
    let paths = Paths {
        data_dir: root.join("data"),
        db_file: root.join("data/cued.db"),
        lock_file: root.join("data/cued.lock"),
        logs_dir: root.join("data/logs"),
        daemon_log: root.join("data/daemon.log"),
        socket_file: root.join("cued.sock"),
        config_file: root.join("config.toml"),
    };
    std::fs::create_dir_all(&paths.logs_dir).unwrap();
    paths
}

/// An in-process daemon on its own runtime, so the backend's blocking
/// threads talk to it exactly as to a real one.
fn start_daemon(runtime: &tokio::runtime::Runtime, paths: &Paths) {
    let store = runtime.block_on(Store::open(&paths.db_file)).unwrap();
    runtime.spawn(cued::daemon::serve_with(
        paths.clone(),
        Config::default(),
        store,
        NoBusNotifier,
    ));
}

/// Updates until `done` says stop, failing after `within`.
fn until(backend: &Backend, within: Duration, mut done: impl FnMut(&Update) -> bool) {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        match backend.try_recv() {
            Some(update) if done(&update) => return,
            Some(_) => {}
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    panic!("timed out");
}

fn submit(paths: &Paths, script: &str) -> cued::model::JobId {
    let spec = JobSpec {
        name: Some("pipeline".into()),
        schedule: Schedule::Once {
            at: Timestamp::now()
                .checked_sub(SignedDuration::from_secs(1))
                .unwrap(),
        },
        graph: cued::submit::single_shell_graph(vec!["/bin/sh".into(), "-c".into(), script.into()]),
        cwd: "/".into(),
        env: CapturedEnv::default(),
        policies: Policies::default(),
        hooks: Hooks::default(),
    };
    match cued::client::call(
        paths,
        RequestBody::Submit {
            spec: Box::new(spec),
        },
    )
    .unwrap()
    {
        Response::Submitted { job, .. } => job,
        other => panic!("expected Submitted, got {other:?}"),
    }
}

#[test]
fn follows_a_run_to_the_end_then_goes_quiet() {
    let dir = tempfile::tempdir().unwrap();
    let paths = temp_paths(dir.path());
    let runtime = tokio::runtime::Runtime::new().unwrap();
    start_daemon(&runtime, &paths);
    while cued::client::call(&paths, RequestBody::Ping).is_err() {
        std::thread::sleep(Duration::from_millis(20));
    }

    let repaints = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&repaints);
    let backend = Backend::start(paths.clone(), None, false, move || {
        counted.fetch_add(1, Relaxed);
    });
    until(
        &backend,
        Duration::from_secs(10),
        |update| matches!(update, Update::Jobs(Ok(jobs)) if jobs.is_empty()),
    );

    let job = submit(&paths, "echo started; sleep 1; echo finished");
    backend.send(Command::Select(Some(job)));
    let (mut saw_running, mut saw_done) = (false, false);
    until(&backend, Duration::from_secs(20), |update| {
        match update {
            Update::Jobs(Ok(jobs)) => {
                saw_done |= jobs
                    .iter()
                    .any(|entry| entry.id == job && entry.status == JobStatus::Done);
            }
            Update::Detail(Some(Ok(Detail { attempts, log, .. }))) => {
                saw_running |= attempts.iter().any(|attempt| attempt.running);
                let finished = attempts
                    .iter()
                    .any(|attempt| !attempt.running && attempt.exit_code == Some(0));
                let output = log
                    .as_ref()
                    .is_some_and(|log| log.text.contains("finished"));
                return saw_done && finished && output;
            }
            _ => {}
        }
        false
    });
    assert!(saw_running, "the running step was never shown");

    // Settled: no notices, no fetches, no repaints.
    std::thread::sleep(Duration::from_millis(500));
    while backend.try_recv().is_some() {}
    let before = repaints.load(Relaxed);
    std::thread::sleep(Duration::from_secs(2));
    assert!(
        backend.try_recv().is_none(),
        "an update while nothing changed"
    );
    assert_eq!(
        repaints.load(Relaxed),
        before,
        "a repaint while nothing changed"
    );
}

#[test]
fn a_daemon_started_after_the_window_is_picked_up() {
    let dir = tempfile::tempdir().unwrap();
    let paths = temp_paths(dir.path());
    let backend = Backend::start(paths.clone(), None, false, || {});
    until(&backend, Duration::from_secs(10), |update| {
        matches!(update, Update::Link(Link::NoDaemon))
    });

    let runtime = tokio::runtime::Runtime::new().unwrap();
    start_daemon(&runtime, &paths);
    until(&backend, Duration::from_secs(15), |update| {
        matches!(update, Update::Link(Link::Live))
    });
    until(&backend, Duration::from_secs(10), |update| {
        matches!(update, Update::Jobs(Ok(_)))
    });
}
