//! Notification delivery against a REAL daemon binary and a PRIVATE,
//! controllable org.freedesktop.Notifications server (DESIGN.md §3.5).
//!
//! Nothing here can reach the host desktop: every test starts its own
//! `dbus-daemon` from a config with no service directories (so nothing can be
//! auto-activated onto it), the daemon runs with a cleared environment
//! (private HOME/XDG dirs, socket, database, and DBUS_SESSION_BUS_ADDRESS
//! pointing only at the private bus), and the notification "server" is this
//! test binary re-executed as a helper process, so it can be SIGKILLed and
//! restarted like a real server crash (new unique bus name, new ID space).
//!
//! Displays are counted independently of the cued database: the helper
//! appends one `displayed` event per Notify call it shows, with the ID it
//! returned, to its own JSONL file. Durable rows are read from SQLite.
//!
//! The daemon's slow tick is 30 s and its per-call acknowledgement wait is
//! 10 s; the stalls below (12 s, 60 s) are chosen against those numbers.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use jiff::{SignedDuration, Timestamp};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use cued::model::{CapturedEnv, Hooks, JobSpec, Policies, Schedule};
use cued::proto::{PROTO_VERSION, Request, RequestBody, Response};
use cued::submit::single_notify_graph;

const DBUS_DAEMON: &str = "/usr/bin/dbus-daemon";
const HELPER_ENV: &str = "CUED_TEST_FAKE_NOTIFYD_ROOT";

// ---------------------------------------------------------------------------
// The private notification server (helper process)
// ---------------------------------------------------------------------------

/// Not a test: the body of the helper process. Ignored so a normal run never
/// executes it; without the env var it is a no-op even under `--ignored`.
#[test]
#[ignore = "helper process for the private notification server"]
fn fake_notification_server_process() {
    let Some(root) = std::env::var_os(HELPER_ENV) else {
        return;
    };
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    runtime
        .block_on(serve_fake(PathBuf::from(root)))
        .expect("fake notification server");
}

struct Fake {
    root: PathBuf,
    unique: String,
    next_id: AtomicU32,
}

impl Fake {
    fn event(&self, value: Value) {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join("events.jsonl"))
            .expect("events file");
        writeln!(file, "{value}").expect("event");
    }

    /// One-shot control files: consumed by the first call that sees them, so
    /// a retry is not stalled again.
    fn take(&self, name: &str) -> Option<u64> {
        let path = self.root.join(name);
        let text = std::fs::read_to_string(&path).ok()?;
        std::fs::remove_file(&path).ok()?;
        Some(text.trim().parse().unwrap_or(0))
    }
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl Fake {
    #[allow(clippy::too_many_arguments)]
    async fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        _app_icon: &str,
        summary: &str,
        body: &str,
        _actions: Vec<String>,
        _hints: HashMap<String, zbus::zvariant::OwnedValue>,
        _expire_timeout: i32,
    ) -> zbus::fdo::Result<u32> {
        self.event(json!({"event": "received", "server": self.unique, "summary": summary}));
        if let Some(secs) = self.take("stall-before") {
            tokio::time::sleep(Duration::from_secs(secs)).await;
        }
        if self.take("fail").is_some() {
            self.event(json!({"event": "rejected", "server": self.unique, "summary": summary}));
            return Err(zbus::fdo::Error::Failed("scripted rejection".into()));
        }
        // Spec: a replaces_id this server issued is replaced in place;
        // anything else gets a fresh ID.
        let id = if replaces_id != 0 && replaces_id < self.next_id.load(Ordering::SeqCst) {
            replaces_id
        } else {
            self.next_id.fetch_add(1, Ordering::SeqCst)
        };
        self.event(json!({
            "event": "displayed", "server": self.unique, "pid": std::process::id(),
            "id": id, "replaces_id": replaces_id, "app": app_name,
            "summary": summary, "body": body, "at": Timestamp::now().to_string(),
        }));
        if let Some(secs) = self.take("stall-after") {
            tokio::time::sleep(Duration::from_secs(secs)).await;
        }
        self.event(json!({"event": "acked", "server": self.unique, "id": id}));
        Ok(id)
    }

    async fn close_notification(&self, id: u32) {
        self.event(json!({"event": "closed", "server": self.unique, "id": id}));
    }

    async fn get_capabilities(&self) -> Vec<String> {
        vec!["body".into()]
    }

    async fn get_server_information(&self) -> (String, String, String, String) {
        ("cued-test".into(), "cued".into(), "0".into(), "1.2".into())
    }
}

async fn serve_fake(root: PathBuf) -> Result<()> {
    let address = std::env::var("DBUS_SESSION_BUS_ADDRESS")?;
    anyhow::ensure!(
        address.contains(&*root.parent().context("root parent")?.to_string_lossy()),
        "refusing to serve on a bus outside the test directory: {address}"
    );
    let connection = zbus::connection::Builder::address(address.as_str())?
        .build()
        .await?;
    let unique = connection.unique_name().context("unique name")?.to_string();
    let fake = Fake {
        root: root.clone(),
        unique: unique.clone(),
        next_id: AtomicU32::new(1),
    };
    connection
        .object_server()
        .at("/org/freedesktop/Notifications", fake)
        .await?;
    connection
        .request_name("org.freedesktop.Notifications")
        .await?;
    std::fs::write(root.join("ready"), &unique)?;
    // Exit with the world: a bus-activated instance is not the test's child.
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let alive = connection
            .call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "GetId",
                &(),
            )
            .await
            .is_ok();
        if !alive || !root.exists() {
            std::process::exit(0);
        }
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn available() -> bool {
    if Path::new(DBUS_DAEMON).exists() {
        return true;
    }
    eprintln!("SKIP: {DBUS_DAEMON} not installed; private-bus coverage not run");
    false
}

async fn wait_for(what: &str, within: Duration, mut check: impl FnMut() -> bool) -> Result<()> {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if check() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!("timed out after {within:?} waiting for {what}")
}

/// One isolated world: private bus, private daemon paths, a helper server.
struct World {
    dir: tempfile::TempDir,
    bus: Option<Child>,
    server: Option<Child>,
    daemon: Option<Child>,
    daemon_starts: u32,
}

impl World {
    fn new() -> Result<Self> {
        let dir = tempfile::Builder::new()
            .prefix("cued-notifbus-")
            .tempdir()?;
        for sub in ["home", "data", "config", "run", "server", "sock"] {
            std::fs::create_dir_all(dir.path().join(sub))?;
        }
        use std::os::unix::fs::PermissionsExt;
        for private in ["run", "sock"] {
            std::fs::set_permissions(
                dir.path().join(private),
                std::fs::Permissions::from_mode(0o700),
            )?;
        }
        Ok(Self {
            dir,
            bus: None,
            server: None,
            daemon: None,
            daemon_starts: 0,
        })
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }
    fn bus_socket(&self) -> PathBuf {
        self.root().join("bus")
    }
    fn bus_address(&self) -> String {
        format!("unix:path={}", self.bus_socket().display())
    }
    fn db(&self) -> PathBuf {
        self.root().join("data/cued/cued.db")
    }
    fn socket(&self) -> PathBuf {
        self.root().join("sock/cued.sock")
    }
    fn server_root(&self) -> PathBuf {
        self.root().join("server")
    }

    fn start_bus(&mut self) -> Result<()> {
        self.start_bus_with("")
    }

    /// A bus that can activate the fake server on demand, like a desktop
    /// whose notifier is D-Bus-activated. Only this world's service dir.
    fn start_bus_with_activation(&mut self) -> Result<()> {
        let services = self.root().join("services");
        std::fs::create_dir_all(&services)?;
        let script = self.root().join("activate-server.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nexec env -i PATH=/usr/bin:/bin {HELPER_ENV}='{}' DBUS_SESSION_BUS_ADDRESS='{}' \
                 '{}' fake_notification_server_process --exact --ignored --nocapture --test-threads 1 \
                 >>'{}' 2>&1\n",
                self.server_root().display(),
                self.bus_address(),
                std::env::current_exe()?.display(),
                self.root().join("server.log").display(),
            ),
        )?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))?;
        std::fs::write(
            services.join("org.freedesktop.Notifications.service"),
            format!(
                "[D-BUS Service]\nName=org.freedesktop.Notifications\nExec={}\n",
                script.display()
            ),
        )?;
        self.start_bus_with(&format!(
            "  <servicedir>{}</servicedir>\n",
            services.display()
        ))
    }

    fn start_bus_with(&mut self, extra: &str) -> Result<()> {
        let config = self.root().join("bus.conf");
        std::fs::write(
            &config,
            format!(
                r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path={}</listen>
{}  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#,
                self.bus_socket().display(),
                extra
            ),
        )?;
        let log = std::fs::File::create(self.root().join("bus.log"))?;
        let child = Command::new(DBUS_DAEMON)
            .arg("--nofork")
            .arg(format!("--config-file={}", config.display()))
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()
            .context("spawning private dbus-daemon")?;
        self.bus = Some(child);
        let socket = self.bus_socket();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !socket.exists() {
            anyhow::ensure!(Instant::now() < deadline, "private bus never came up");
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    }

    /// Start (or restart) the private server: a fresh process, so a fresh
    /// unique name and an ID space starting at 1.
    async fn start_server(&mut self) -> Result<String> {
        let ready = self.server_root().join("ready");
        let _ = std::fs::remove_file(&ready);
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root().join("server.log"))?;
        let child = Command::new(std::env::current_exe()?)
            .args([
                "fake_notification_server_process",
                "--exact",
                "--ignored",
                "--nocapture",
            ])
            .args(["--test-threads", "1"])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env(HELPER_ENV, self.server_root())
            .env("DBUS_SESSION_BUS_ADDRESS", self.bus_address())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()?;
        self.server = Some(child);
        wait_for(
            "private notification server",
            Duration::from_secs(10),
            || ready.exists(),
        )
        .await?;
        Ok(std::fs::read_to_string(&ready)?)
    }

    /// SIGKILL: a server crash, not a clean shutdown.
    fn kill_server(&mut self) -> Result<()> {
        if let Some(mut child) = self.server.take() {
            child.kill()?;
            child.wait()?;
        }
        Ok(())
    }

    fn control(&self, name: &str, secs: u64) -> Result<()> {
        Ok(std::fs::write(
            self.server_root().join(name),
            secs.to_string(),
        )?)
    }

    fn start_daemon(&mut self) -> Result<()> {
        self.daemon_starts += 1;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root().join("daemon.stderr"))?;
        let child = Command::new(env!("CARGO_BIN_EXE_cued"))
            .args(["daemon", "--foreground"])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.root().join("home"))
            .env("XDG_DATA_HOME", self.root().join("data"))
            .env("XDG_CONFIG_HOME", self.root().join("config"))
            .env("XDG_RUNTIME_DIR", self.root().join("run"))
            .env("CUED_SOCKET_DIR", self.root().join("sock"))
            .env("DBUS_SESSION_BUS_ADDRESS", self.bus_address())
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()?;
        self.daemon = Some(child);
        Ok(())
    }

    async fn await_daemon(&self) -> Result<()> {
        let socket = self.socket();
        for _ in 0..300 {
            if UnixStream::connect(&socket).await.is_ok() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        bail!(
            "daemon socket never came up; stderr:\n{}",
            self.daemon_log()
        )
    }

    async fn stop_daemon(&mut self) -> Result<()> {
        if let Some(mut child) = self.daemon.take() {
            // SAFETY: signalling a child this test spawned.
            unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
            let deadline = Instant::now() + Duration::from_secs(20);
            while child.try_wait()?.is_none() {
                if Instant::now() > deadline {
                    child.kill()?;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        Ok(())
    }

    /// Retries against an unavailable service are paced by nudges and the
    /// slow tick, never a tight loop.
    async fn assert_paced_retries(&self, over: Duration) -> Result<()> {
        let failures = || self.daemon_log().matches("failed to deliver").count();
        let before = failures();
        tokio::time::sleep(over).await;
        let during = failures() - before;
        anyhow::ensure!(
            during <= 2,
            "{during} delivery attempts in {over:?}: the retry loop is spinning"
        );
        Ok(())
    }

    fn daemon_log(&self) -> String {
        std::fs::read_to_string(self.root().join("daemon.stderr")).unwrap_or_default()
    }

    async fn submit(&self, title: &str, body: &str, env: CapturedEnv) -> Result<()> {
        let spec = Box::new(JobSpec {
            name: None,
            schedule: Schedule::Once {
                at: Timestamp::now().checked_sub(SignedDuration::from_secs(1))?,
            },
            graph: single_notify_graph(title.into(), body.into()),
            cwd: "/".into(),
            env,
            policies: Policies::default(),
            hooks: Hooks::default(),
        });
        let mut stream = UnixStream::connect(self.socket()).await?;
        let mut payload = serde_json::to_string(&Request {
            proto: PROTO_VERSION,
            body: RequestBody::Submit { spec },
        })?;
        payload.push('\n');
        stream.write_all(payload.as_bytes()).await?;
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).await?;
        match serde_json::from_str(&line)? {
            Response::Submitted { .. } => Ok(()),
            other => bail!("expected Submitted, got {other:?}"),
        }
    }

    fn events(&self) -> Vec<Value> {
        std::fs::read_to_string(self.server_root().join("events.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    fn count(&self, event: &str) -> usize {
        self.events().iter().filter(|e| e["event"] == event).count()
    }

    fn displays(&self) -> Vec<Value> {
        self.events()
            .into_iter()
            .filter(|e| e["event"] == "displayed")
            .collect()
    }

    async fn sql(&self, statement: &str) -> Result<()> {
        let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}", self.db().display())).await?;
        sqlx::query(statement).execute(&pool).await?;
        pool.close().await;
        Ok(())
    }

    /// (id, title, delivered?) for every durable queue row.
    async fn rows(&self) -> Result<Vec<(i64, String, bool)>> {
        let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}", self.db().display())).await?;
        let rows: Vec<(i64, String, Option<String>)> =
            sqlx::query_as("SELECT id, title, delivered_at FROM notifications ORDER BY id")
                .fetch_all(&pool)
                .await?;
        pool.close().await;
        Ok(rows
            .into_iter()
            .map(|(id, title, at)| (id, title, at.is_some()))
            .collect())
    }

    async fn await_all_recorded(&self, expected_rows: usize, within: Duration) -> Result<()> {
        let deadline = Instant::now() + within;
        loop {
            let rows = self.rows().await.unwrap_or_default();
            if rows.len() == expected_rows && rows.iter().all(|(_, _, delivered)| *delivered) {
                return Ok(());
            }
            if Instant::now() > deadline {
                bail!(
                    "rows never all recorded: {rows:?}\nevents: {:#?}\ndaemon:\n{}",
                    self.events(),
                    self.daemon_log()
                );
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// Every recorded row carries the (server lifetime, ID) of exactly one
    /// display the server independently logged, for that row's own text.
    async fn assert_receipts_match(&self) -> Result<()> {
        let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}", self.db().display())).await?;
        let receipts: Vec<(String, Option<String>, Option<i64>)> = sqlx::query_as(
            "SELECT title, delivery_server, delivery_id FROM notifications WHERE delivered_at IS NOT NULL",
        )
        .fetch_all(&pool)
        .await?;
        pool.close().await;
        let displays = self.displays();
        let mut claimed = std::collections::HashSet::new();
        for (title, server, id) in receipts {
            let (server, id) = (server.context("receipt server")?, id.context("receipt id")?);
            let matching: Vec<_> = displays
                .iter()
                .filter(|d| d["server"] == server.as_str() && d["id"] == id)
                .collect();
            anyhow::ensure!(
                matching.len() == 1,
                "receipt ({server}, {id}) matches {matching:?}"
            );
            anyhow::ensure!(
                matching[0]["summary"] == title.as_str(),
                "receipt names another popup"
            );
            anyhow::ensure!(claimed.insert((server, id)), "two rows claim one display");
        }
        Ok(())
    }

    /// Evidence for the run log (the test output is kept by the campaign).
    fn report(&self, case: &str) {
        let rows = futures_rows(self);
        eprintln!(
            "CASE {case}: displays={} received={} daemon_starts={} rows={rows}\nEVENTS {}\nDAEMON\n{}",
            self.count("displayed"),
            self.count("received"),
            self.daemon_starts,
            serde_json::to_string(&self.events()).unwrap_or_default(),
            self.daemon_log()
        );
    }
}

fn futures_rows(world: &World) -> String {
    // Best effort; report() must never fail a test on its own.
    let db = world.db();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;
        runtime.block_on(async move {
            let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}", db.display()))
                .await
                .ok()?;
            let rows: Vec<(i64, String, Option<String>)> =
                sqlx::query_as("SELECT id, title, delivered_at FROM notifications ORDER BY id")
                    .fetch_all(&pool)
                    .await
                    .ok()?;
            Some(format!("{rows:?}"))
        })
    })
    .join()
    .ok()
    .flatten()
    .unwrap_or_else(|| "<unreadable>".into())
}

impl Drop for World {
    fn drop(&mut self) {
        for child in [self.daemon.take(), self.server.take(), self.bus.take()]
            .into_iter()
            .flatten()
        {
            let mut child = child;
            let _ = child.kill();
            let _ = child.wait();
        }
        if std::env::var_os("CUED_TEST_KEEP").is_some() {
            let kept = self.dir.path().to_path_buf();
            eprintln!("kept {}", kept.display());
            std::mem::forget(std::mem::replace(
                &mut self.dir,
                tempfile::tempdir().expect("tmp"),
            ));
        }
    }
}

async fn world_with_daemon(server: bool) -> Result<World> {
    let mut world = World::new()?;
    world.start_bus()?;
    if server {
        world.start_server().await?;
    }
    world.start_daemon()?;
    world.await_daemon().await?;
    Ok(world)
}

/// Late-arriving extras (a retry after the slow tick) must have had their
/// chance to show up before a count is trusted.
const SETTLE: Duration = Duration::from_secs(35);

// ---------------------------------------------------------------------------
// Cases
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn service_unavailable_then_recovery_displays_once() -> Result<()> {
    if !available() {
        return Ok(());
    }
    let mut world = world_with_daemon(false).await?;
    world
        .submit("unavailable", "b", CapturedEnv::default())
        .await?;
    wait_for("the undelivered row", Duration::from_secs(10), || {
        std::fs::read_to_string(world.root().join("daemon.stderr"))
            .is_ok_and(|log| log.contains("failed to deliver"))
    })
    .await?;
    assert_eq!(
        world.rows().await?,
        vec![(1, "unavailable".into(), false)],
        "late beats lost"
    );
    world.assert_paced_retries(Duration::from_secs(5)).await?;

    world.start_server().await?;
    world.await_all_recorded(1, Duration::from_secs(45)).await?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    world.report("service_unavailable");
    world.assert_receipts_match().await?;
    assert_eq!(world.count("displayed"), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn bus_absent_then_appearing_displays_once() -> Result<()> {
    if !available() {
        return Ok(());
    }
    // No bus socket at all at first: the address resolves, the dial fails.
    let mut world = World::new()?;
    world.start_daemon()?;
    world.await_daemon().await?;
    world.submit("no-bus", "b", CapturedEnv::default()).await?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(world.rows().await?, vec![(1, "no-bus".into(), false)]);
    world.assert_paced_retries(Duration::from_secs(5)).await?;

    world.start_bus()?;
    world.start_server().await?;
    world.await_all_recorded(1, Duration::from_secs(45)).await?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    world.report("bus_absent");
    world.assert_receipts_match().await?;
    assert_eq!(world.count("displayed"), 1);
    Ok(())
}

/// Server shows the popup only after the daemon's 10 s acknowledgement wait.
#[tokio::test(flavor = "multi_thread")]
async fn display_after_ack_timeout_is_not_redisplayed() -> Result<()> {
    if !available() {
        return Ok(());
    }
    let world = world_with_daemon(true).await?;
    world.control("stall-before", 12)?;
    world
        .submit("late-display", "b", CapturedEnv::default())
        .await?;
    world.await_all_recorded(1, Duration::from_secs(60)).await?;
    tokio::time::sleep(SETTLE).await;
    world.report("display_after_timeout");
    world.assert_receipts_match().await?;
    assert_eq!(
        world.count("displayed"),
        1,
        "one durable entry, one display"
    );
    Ok(())
}

/// Server shows the popup at once but acknowledges after 12 s.
#[tokio::test(flavor = "multi_thread")]
async fn delayed_ack_is_not_redisplayed() -> Result<()> {
    if !available() {
        return Ok(());
    }
    let world = world_with_daemon(true).await?;
    world.control("stall-after", 12)?;
    world
        .submit("late-ack", "b", CapturedEnv::default())
        .await?;
    world.await_all_recorded(1, Duration::from_secs(60)).await?;
    tokio::time::sleep(SETTLE).await;
    world.report("delayed_ack");
    world.assert_receipts_match().await?;
    assert_eq!(
        world.count("displayed"),
        1,
        "one durable entry, one display"
    );
    Ok(())
}

/// Displayed and acknowledged, but writing delivered_at fails.
#[tokio::test(flavor = "multi_thread")]
async fn record_failure_after_display_is_not_redisplayed() -> Result<()> {
    if !available() {
        return Ok(());
    }
    let world = world_with_daemon(true).await?;
    world
        .sql("CREATE TRIGGER delivery_record_gate BEFORE UPDATE OF delivered_at ON notifications BEGIN SELECT RAISE(FAIL, 'delivery_record_gate'); END")
        .await?;
    world
        .submit("record-fail", "b", CapturedEnv::default())
        .await?;
    wait_for("the failed record", Duration::from_secs(10), || {
        std::fs::read_to_string(world.root().join("daemon.stderr"))
            .is_ok_and(|log| log.contains("delivery_record_gate"))
    })
    .await?;
    assert_eq!(world.count("displayed"), 1);
    assert_eq!(
        world.rows().await?,
        vec![(1, "record-fail".into(), false)],
        "not marked before it is recorded"
    );
    // Let at least one more pass run against the still-broken store.
    tokio::time::sleep(SETTLE).await;
    assert_eq!(
        world.count("displayed"),
        1,
        "a failed write must not cause a redisplay"
    );

    world.sql("DROP TRIGGER delivery_record_gate").await?;
    world.await_all_recorded(1, Duration::from_secs(45)).await?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    world.report("record_failure");
    world.assert_receipts_match().await?;
    assert_eq!(
        world.count("displayed"),
        1,
        "one durable entry, one display"
    );
    Ok(())
}

/// The inherent residual: the daemon dies after the display but before the
/// row is recorded. Nothing durable says "shown", so the next daemon shows
/// it again. At-least-once, never zero.
#[tokio::test(flavor = "multi_thread")]
async fn restart_after_unrecorded_display_redelivers_at_least_once() -> Result<()> {
    if !available() {
        return Ok(());
    }
    let mut world = world_with_daemon(true).await?;
    world
        .sql("CREATE TRIGGER delivery_record_gate BEFORE UPDATE OF delivered_at ON notifications BEGIN SELECT RAISE(FAIL, 'delivery_record_gate'); END")
        .await?;
    world
        .submit("restart-unrecorded", "b", CapturedEnv::default())
        .await?;
    wait_for("the failed record", Duration::from_secs(10), || {
        std::fs::read_to_string(world.root().join("daemon.stderr"))
            .is_ok_and(|log| log.contains("delivery_record_gate"))
    })
    .await?;
    world.stop_daemon().await?;
    world.sql("DROP TRIGGER delivery_record_gate").await?;
    world.start_daemon()?;
    world.await_daemon().await?;
    world.await_all_recorded(1, Duration::from_secs(45)).await?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    world.report("restart_unrecorded");
    world.assert_receipts_match().await?;
    assert_eq!(
        world.count("displayed"),
        2,
        "documented at-least-once residual"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn daemon_restart_neither_loses_nor_repeats_recorded_state() -> Result<()> {
    if !available() {
        return Ok(());
    }
    let mut world = world_with_daemon(false).await?;
    // One delivered before the restart…
    world.start_server().await?;
    world
        .submit("before-restart", "b", CapturedEnv::default())
        .await?;
    world.await_all_recorded(1, Duration::from_secs(20)).await?;
    // …one still queued (no server) across the restart.
    world.kill_server()?;
    world
        .submit("across-restart", "b", CapturedEnv::default())
        .await?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    world.assert_paced_retries(Duration::from_secs(5)).await?;
    world.stop_daemon().await?;
    assert_eq!(world.rows().await?.iter().filter(|(_, _, d)| !d).count(), 1);

    world.start_server().await?;
    world.start_daemon()?;
    world.await_daemon().await?;
    world.await_all_recorded(2, Duration::from_secs(45)).await?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    world.report("daemon_restart");
    world.assert_receipts_match().await?;
    let titles: Vec<String> = world
        .displays()
        .iter()
        .map(|e| e["summary"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(titles, vec!["before-restart", "across-restart"]);
    Ok(())
}

/// The server crashes while the call is in flight, before it displays.
#[tokio::test(flavor = "multi_thread")]
async fn server_crash_before_display_is_retried_once_on_the_new_server() -> Result<()> {
    if !available() {
        return Ok(());
    }
    let mut world = world_with_daemon(true).await?;
    world.control("stall-before", 60)?;
    world
        .submit("crash-before", "b", CapturedEnv::default())
        .await?;
    wait_for("the call to arrive", Duration::from_secs(10), || {
        world.count("received") == 1
    })
    .await?;
    world.kill_server()?;
    let second = world.start_server().await?;
    world.await_all_recorded(1, Duration::from_secs(60)).await?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    world.report("server_crash_before_display");
    world.assert_receipts_match().await?;
    let displays = world.displays();
    assert_eq!(displays.len(), 1);
    assert_eq!(displays[0]["server"], second.as_str());
    Ok(())
}

/// The server crashes after displaying, before acknowledging: the daemon
/// cannot know the popup existed. The retry goes to the new server lifetime
/// (whose ID space is unrelated); a replaces_id carried over would name
/// something else entirely, so none is used.
#[tokio::test(flavor = "multi_thread")]
async fn server_crash_after_display_redelivers_to_the_new_server() -> Result<()> {
    if !available() {
        return Ok(());
    }
    let mut world = world_with_daemon(true).await?;
    world.control("stall-after", 60)?;
    world
        .submit("crash-after", "b", CapturedEnv::default())
        .await?;
    wait_for("the display", Duration::from_secs(10), || {
        world.count("displayed") == 1
    })
    .await?;
    let first = world.displays()[0]["server"].clone();
    world.kill_server()?;
    let second = world.start_server().await?;
    world.await_all_recorded(1, Duration::from_secs(60)).await?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    world.report("server_crash_after_display");
    world.assert_receipts_match().await?;
    let displays = world.displays();
    assert_eq!(
        displays.len(),
        2,
        "documented at-least-once residual across server lifetimes"
    );
    assert_eq!(displays[0]["server"], first);
    assert_eq!(displays[1]["server"], second.as_str());
    assert!(
        displays.iter().all(|d| d["replaces_id"] == 0),
        "never replace across lifetimes"
    );
    Ok(())
}

/// Server restarted between two notifications: both IDs are 1, and both
/// notifications must still be shown.
#[tokio::test(flavor = "multi_thread")]
async fn server_restart_between_notifications_keeps_both() -> Result<()> {
    if !available() {
        return Ok(());
    }
    let mut world = world_with_daemon(true).await?;
    world
        .submit("first-lifetime", "b", CapturedEnv::default())
        .await?;
    world.await_all_recorded(1, Duration::from_secs(20)).await?;
    world.kill_server()?;
    world.start_server().await?;
    world
        .submit("second-lifetime", "b", CapturedEnv::default())
        .await?;
    world.await_all_recorded(2, Duration::from_secs(45)).await?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    world.report("server_restart_between");
    world.assert_receipts_match().await?;
    let displays = world.displays();
    assert_eq!(displays.len(), 2);
    assert_eq!(displays[0]["id"], 1);
    assert_eq!(displays[1]["id"], 1, "a fresh server lifetime reuses IDs");
    assert_ne!(displays[0]["server"], displays[1]["server"]);
    assert!(displays.iter().all(|d| d["replaces_id"] == 0));
    Ok(())
}

/// Legitimately repeated reminders (identical text, separate durable rows)
/// are each shown; deduplication is per row, never by content. The captured
/// environment never reaches the bus.
#[tokio::test(flavor = "multi_thread")]
async fn identical_reminders_stay_distinct_and_env_is_not_sent() -> Result<()> {
    if !available() {
        return Ok(());
    }
    let world = world_with_daemon(true).await?;
    let mut env = CapturedEnv::default();
    env.vars
        .insert("CUED_TEST_SECRET".into(), "hunter2-value".into());
    for _ in 0..3 {
        world.submit("Stretch", "Take a break", env.clone()).await?;
    }
    world.await_all_recorded(3, Duration::from_secs(20)).await?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    world.report("identical_reminders");
    world.assert_receipts_match().await?;
    let displays = world.displays();
    assert_eq!(displays.len(), 3);
    let mut ids: Vec<u64> = displays.iter().filter_map(|d| d["id"].as_u64()).collect();
    ids.dedup();
    assert_eq!(ids, vec![1, 2, 3], "three separate popups");
    let raw = std::fs::read_to_string(world.server_root().join("events.jsonl"))?;
    assert!(
        !raw.contains("hunter2") && !raw.contains("CUED_TEST_SECRET"),
        "captured env reached the bus"
    );
    assert!(!world.daemon_log().contains("hunter2"));
    Ok(())
}

// ---------------------------------------------------------------------------
// The transport itself (DesktopNotifier), with short limits
// ---------------------------------------------------------------------------

use cued::model::{DeliveryReceipt, NotifySpec};
use cued::notify::{Delivery, DesktopNotifier, Notifier};

fn spec(title: &str) -> NotifySpec {
    NotifySpec {
        title: title.into(),
        body: "b".into(),
    }
}

async fn world_with_server() -> Result<(World, String)> {
    let mut world = World::new()?;
    world.start_bus()?;
    let server = world.start_server().await?;
    Ok((world, server))
}

fn notifier(world: &World, ack_wait_ms: u64, abandon_ms: u64) -> DesktopNotifier {
    DesktopNotifier::with_limits(
        Some(world.bus_address()),
        Duration::from_millis(ack_wait_ms),
        Duration::from_millis(abandon_ms),
    )
}

async fn settled(notifier: &DesktopNotifier) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(20), notifier.settled())
        .await
        .context("the overdue call never settled")
}

#[tokio::test(flavor = "multi_thread")]
async fn transport_prompt_ack_returns_the_servers_receipt() -> Result<()> {
    if !available() {
        return Ok(());
    }
    let (world, server) = world_with_server().await?;
    let notifier = notifier(&world, 5_000, 60_000);
    let first = notifier.attempt(1, &spec("one")).await?;
    let second = notifier.attempt(2, &spec("one")).await?;
    assert_eq!(
        first,
        Delivery::Shown(Some(DeliveryReceipt {
            server: server.clone(),
            id: 1
        }))
    );
    assert_eq!(
        second,
        Delivery::Shown(Some(DeliveryReceipt { server, id: 2 })),
        "distinct rows, distinct popups"
    );
    assert_eq!(world.count("displayed"), 2);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn transport_holds_an_overdue_call_until_its_late_ack() -> Result<()> {
    if !available() {
        return Ok(());
    }
    let (world, server) = world_with_server().await?;
    world.control("stall-after", 2)?;
    let notifier = notifier(&world, 300, 60_000);
    assert_eq!(
        notifier.attempt(7, &spec("late-ack")).await?,
        Delivery::TimedOut
    );
    assert_eq!(world.count("displayed"), 1, "it is on screen already");
    assert_eq!(
        notifier.attempt(7, &spec("late-ack")).await?,
        Delivery::Awaiting,
        "not re-sent"
    );
    settled(&notifier).await?;
    assert_eq!(
        notifier.attempt(7, &spec("late-ack")).await?,
        Delivery::Shown(Some(DeliveryReceipt { server, id: 1 }))
    );
    assert_eq!(world.count("received"), 1);
    assert_eq!(world.count("displayed"), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn transport_holds_a_call_whose_display_comes_late() -> Result<()> {
    if !available() {
        return Ok(());
    }
    let (world, server) = world_with_server().await?;
    world.control("stall-before", 2)?;
    let notifier = notifier(&world, 300, 60_000);
    assert_eq!(
        notifier.attempt(3, &spec("late-display")).await?,
        Delivery::TimedOut
    );
    assert_eq!(world.count("displayed"), 0);
    settled(&notifier).await?;
    assert_eq!(
        notifier.attempt(3, &spec("late-display")).await?,
        Delivery::Shown(Some(DeliveryReceipt { server, id: 1 }))
    );
    assert_eq!(world.count("received"), 1);
    assert_eq!(world.count("displayed"), 1);
    Ok(())
}

/// The escape hatch: a call unanswered past `abandon_after` no longer holds
/// its row. Re-sending is at-least-once by design — the old call may yet
/// show.
#[tokio::test(flavor = "multi_thread")]
async fn transport_resends_after_abandoning_an_unanswered_call() -> Result<()> {
    if !available() {
        return Ok(());
    }
    let (world, _server) = world_with_server().await?;
    world.control("stall-after", 30)?;
    let notifier = notifier(&world, 200, 1_000);
    assert_eq!(
        notifier.attempt(1, &spec("wedged")).await?,
        Delivery::TimedOut
    );
    assert_eq!(
        notifier.attempt(1, &spec("wedged")).await?,
        Delivery::Awaiting
    );
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    assert!(matches!(
        notifier.attempt(1, &spec("wedged")).await?,
        Delivery::Shown(Some(_))
    ));
    assert_eq!(
        world.count("displayed"),
        2,
        "documented at-least-once after abandonment"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn transport_resends_when_the_server_dies_mid_call() -> Result<()> {
    if !available() {
        return Ok(());
    }
    for (control, expected_displays) in [("stall-before", 1), ("stall-after", 2)] {
        let (mut world, first) = world_with_server().await?;
        world.control(control, 60)?;
        let notifier = notifier(&world, 300, 600_000);
        assert_eq!(
            notifier.attempt(1, &spec(control)).await?,
            Delivery::TimedOut
        );
        world.kill_server()?;
        // The bus answers the orphaned call (NoReply) — well before the
        // abandonment limit.
        settled(&notifier).await?;
        let second = world.start_server().await?;
        assert_ne!(first, second);
        assert_eq!(
            notifier.attempt(1, &spec(control)).await?,
            Delivery::Shown(Some(DeliveryReceipt {
                server: second.clone(),
                id: 1
            })),
            "{control}"
        );
        let displays = world.displays();
        assert_eq!(displays.len(), expected_displays, "{control}: {displays:?}");
        assert!(
            displays.iter().all(|d| d["replaces_id"] == 0),
            "no replacement across lifetimes"
        );
        eprintln!(
            "CASE transport_server_death_{control}: displays={}",
            displays.len()
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn transport_without_a_server_is_an_error_not_an_overdue_call() -> Result<()> {
    if !available() {
        return Ok(());
    }
    let mut world = World::new()?;
    world.start_bus()?;
    let notifier = notifier(&world, 2_000, 60_000);
    assert!(notifier.attempt(1, &spec("nobody")).await.is_err());
    world.start_server().await?;
    assert!(matches!(
        notifier.attempt(1, &spec("nobody")).await?,
        Delivery::Shown(Some(_))
    ));
    assert_eq!(world.count("displayed"), 1);
    Ok(())
}

/// An on-demand notifier (D-Bus activation) must still be started by a
/// delivery, and its receipt must name the activated server.
#[tokio::test(flavor = "multi_thread")]
async fn transport_still_activates_an_on_demand_server() -> Result<()> {
    if !available() {
        return Ok(());
    }
    let mut world = World::new()?;
    world.start_bus_with_activation()?;
    let notifier = notifier(&world, 10_000, 60_000);
    let Delivery::Shown(Some(receipt)) = notifier.attempt(1, &spec("activated")).await? else {
        bail!(
            "not shown; server log:\n{}",
            std::fs::read_to_string(world.root().join("server.log")).unwrap_or_default()
        );
    };
    let activated = std::fs::read_to_string(world.server_root().join("ready"))?;
    assert_eq!(
        receipt,
        DeliveryReceipt {
            server: activated,
            id: 1
        }
    );
    assert_eq!(world.count("displayed"), 1);
    Ok(())
}
