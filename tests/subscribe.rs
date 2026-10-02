//! `Subscribe` (DESIGN.md §5.1) over the real socket: a viewer that only
//! refetches when told sees a job through to the end, and is told nothing
//! while nothing changes — including by its own refetches.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use jiff::{SignedDuration, Timestamp};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

use cued::config::Config;
use cued::model::{CapturedEnv, Hooks, JobId, JobSpec, JobStatus, Policies, RunStatus, Schedule};
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

fn request_line(proto: u32, body: RequestBody) -> Result<String> {
    let mut line = serde_json::to_string(&Request { proto, body })?;
    line.push('\n');
    Ok(line)
}

async fn roundtrip(socket: &Path, body: RequestBody) -> Result<Response> {
    let mut stream = UnixStream::connect(socket).await?;
    stream
        .write_all(request_line(PROTO_VERSION, body)?.as_bytes())
        .await?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).await?;
    Ok(serde_json::from_str(&line)?)
}

/// One subscribed connection.
struct Subscription {
    lines: Lines<BufReader<OwnedReadHalf>>,
    write: OwnedWriteHalf,
}

impl Subscription {
    async fn open(socket: &Path, proto: u32) -> Result<(Self, Response)> {
        let (read, mut write) = UnixStream::connect(socket).await?.into_split();
        write
            .write_all(request_line(proto, RequestBody::Subscribe)?.as_bytes())
            .await?;
        let mut subscription = Self {
            lines: BufReader::new(read).lines(),
            write,
        };
        let first = subscription
            .next(Duration::from_secs(5))
            .await?
            .context("no reply to Subscribe")?;
        Ok((subscription, first))
    }

    /// The next notice, `None` at end of stream; an error if none comes
    /// within `within`.
    async fn next(&mut self, within: Duration) -> Result<Option<Response>> {
        let line = tokio::time::timeout(within, self.lines.next_line())
            .await
            .context("no notice in time")??;
        line.map(|line| Ok(serde_json::from_str(&line)?))
            .transpose()
    }
}

fn spec(at: Timestamp, script: &str) -> Box<JobSpec> {
    Box::new(JobSpec {
        name: None,
        schedule: Schedule::Once { at },
        graph: submit::single_shell_graph(vec!["/bin/sh".into(), "-c".into(), script.into()]),
        cwd: "/".into(),
        env: CapturedEnv::default(),
        policies: Policies::default(),
        hooks: Hooks::default(),
    })
}

/// The job and its latest run's status, as a viewer's list shows them.
async fn listed(socket: &Path, job: JobId) -> Result<(JobStatus, Option<RunStatus>)> {
    match roundtrip(socket, RequestBody::List { all: true }).await? {
        Response::JobList { jobs } => {
            let entry = jobs
                .iter()
                .find(|entry| entry.id == job)
                .context("unlisted")?;
            Ok((entry.status, entry.last_run.as_ref().map(|run| run.status)))
        }
        other => bail!("expected JobList, got {other:?}"),
    }
}

#[tokio::test]
async fn a_viewer_follows_a_run_on_notices_alone() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let paths = temp_paths(dir.path())?;
    let store = Store::open(&paths.db_file).await?;
    let daemon_task = tokio::spawn(daemon::serve_with(
        paths.clone(),
        Config::default(),
        store,
        NoBusNotifier,
    ));
    await_socket(&paths.socket_file).await?;
    let socket = &paths.socket_file;

    let (mut subscription, first) = Subscription::open(socket, PROTO_VERSION).await?;
    assert!(matches!(first, Response::Subscribed), "{first:?}");

    // Due in a moment, and slow enough that running is its own state.
    let at = Timestamp::now().checked_add(SignedDuration::from_millis(500))?;
    let job = match roundtrip(
        socket,
        RequestBody::Submit {
            spec: spec(at, "sleep 1"),
        },
    )
    .await?
    {
        Response::Submitted { job, .. } => job,
        other => bail!("expected Submitted, got {other:?}"),
    };

    // A viewer: refetch on each notice, never on a timer.
    let mut seen = Vec::new();
    loop {
        match subscription.next(Duration::from_secs(10)).await? {
            Some(Response::Changed) => {}
            other => bail!("expected Changed, got {other:?}; saw {seen:?}"),
        }
        let state = listed(socket, job).await?;
        if seen.last() != Some(&state) {
            seen.push(state);
        }
        if state.0 == JobStatus::Done {
            break;
        }
    }
    assert!(
        seen.contains(&(JobStatus::Active, Some(RunStatus::Running))),
        "the running step was never shown: {seen:?}"
    );
    assert_eq!(seen.last(), Some(&(JobStatus::Done, Some(RunStatus::Done))));

    // Settled: the viewer's own refetch must not set off another notice.
    listed(socket, job).await?;
    match subscription.next(Duration::from_millis(1500)).await {
        Err(_) => {}
        Ok(notice) => bail!("told of a change while nothing changed: {notice:?}"),
    }

    // The stream takes no requests: another line ends it.
    subscription.write.write_all(b"{}\n").await?;
    assert!(
        subscription.next(Duration::from_secs(5)).await?.is_none(),
        "the stream outlived a second request"
    );

    daemon_task.abort();
    Ok(())
}

#[tokio::test]
async fn a_stale_client_is_refused_before_subscribing() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let paths = temp_paths(dir.path())?;
    let store = Store::open(&paths.db_file).await?;
    let daemon_task = tokio::spawn(daemon::serve_with(
        paths.clone(),
        Config::default(),
        store,
        NoBusNotifier,
    ));
    await_socket(&paths.socket_file).await?;

    let (_, first) = Subscription::open(&paths.socket_file, PROTO_VERSION + 1).await?;
    assert!(
        matches!(first, Response::ProtoMismatch { daemon_proto } if daemon_proto == PROTO_VERSION),
        "{first:?}"
    );

    daemon_task.abort();
    Ok(())
}
