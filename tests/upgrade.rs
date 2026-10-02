//! `cued upgrade` (DESIGN.md §5.2): a new binary renamed over the daemon's
//! path is taken up in place — same PID, same store, same socket — and a
//! step that was running when the upgrade was asked for finishes rather than
//! being interrupted. Driven as real processes, because the thing under test
//! is an exec.
//!
//! Each test runs the daemon from its own copy of the binary, so it can
//! "install a new build" by renaming a fresh copy over that path, the way
//! `cargo install` does, without touching the real one.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

/// A HOME with the daemon's binary copied into it.
struct Install {
    dir: tempfile::TempDir,
    exe: PathBuf,
}

impl Install {
    fn new() -> Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir()?;
        // paths.rs refuses a socket directory that isn't 0700.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))?;
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin)?;
        let exe = bin.join("cued");
        std::fs::copy(env!("CARGO_BIN_EXE_cued"), &exe)?;
        Ok(Self { dir, exe })
    }

    fn home(&self) -> &Path {
        self.dir.path()
    }

    fn db(&self) -> PathBuf {
        self.home().join(".local/share/cued/cued.db")
    }

    /// What `cargo install` does: write elsewhere, rename over. The running
    /// daemon keeps the old inode; the path now names a new one.
    fn install_new_build(&self) -> Result<()> {
        let next = self.exe.with_file_name("cued.next");
        std::fs::copy(env!("CARGO_BIN_EXE_cued"), &next)?;
        std::fs::rename(&next, &self.exe)?;
        Ok(())
    }

    fn cued(&self, args: &[&str]) -> Result<Output> {
        // Another test thread forking while a copy above still had its file
        // open for writing makes exec fail with ETXTBSY for a moment.
        for _ in 0..50 {
            match isolated(Command::new(&self.exe), self.home())
                .args(args)
                .stdin(Stdio::null())
                .output()
            {
                Err(error) if error.raw_os_error() == Some(libc::ETXTBSY) => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                result => return Ok(result?),
            }
        }
        bail!("{} stayed busy", self.exe.display())
    }

    fn daemon_pid(&self) -> Option<i32> {
        let want = format!("HOME={}", self.home().display());
        for entry in std::fs::read_dir("/proc").ok()? {
            let entry = entry.ok()?;
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
                continue;
            };
            let Ok(environ) = std::fs::read(entry.path().join("environ")) else {
                continue;
            };
            let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) else {
                continue;
            };
            let is_daemon = cmdline.split(|b| *b == 0).any(|arg| arg == b"daemon");
            if is_daemon && environ.split(|b| *b == 0).any(|v| v == want.as_bytes()) {
                return Some(pid);
            }
        }
        None
    }

    /// Whether `pid` is executing the inode currently at our path.
    fn runs_installed_build(&self, pid: i32) -> bool {
        let (Ok(running), Ok(installed)) = (
            std::fs::metadata(format!("/proc/{pid}/exe")),
            std::fs::metadata(&self.exe),
        ) else {
            return false;
        };
        (running.dev(), running.ino()) == (installed.dev(), installed.ino())
    }
}

impl Drop for Install {
    fn drop(&mut self) {
        if let Some(pid) = self.daemon_pid() {
            // SAFETY: signalling the daemon this test started, by pid.
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
    }
}

/// Keep the daemon off the host's data and session bus.
fn isolated(mut command: Command, home: &Path) -> Command {
    command
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("CUED_SOCKET_DIR", home)
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={}", home.join("no-bus").display()),
        );
    command
}

fn wait_until(mut check: impl FnMut() -> bool, within: Duration, what: &str) -> Result<()> {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if check() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("timed out waiting for {what}")
}

/// (status, cursor_kind) per run, read straight from the store.
fn runs(db: &Path) -> Vec<(String, String)> {
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return Vec::new();
    };
    rt.block_on(async {
        let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}?mode=ro", db.display()))
            .await
            .ok()?;
        sqlx::query_as("SELECT status, cursor_kind FROM runs ORDER BY job_id, id")
            .fetch_all(&pool)
            .await
            .ok()
    })
    .unwrap_or_default()
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// The point of the feature: a step running when the upgrade is asked for
/// runs to completion under the old image, and the run ends `done` — not
/// `held`, which is what a restart would have left it as.
#[test]
fn a_running_step_finishes_and_the_daemon_execs_the_new_build_in_place() -> Result<()> {
    let install = Install::new()?;
    let marker = install.home().join("finished");
    let script = format!("sleep 3; touch {}", marker.display());
    install.cued(&["at", "in 1s", "--", "/bin/sh", "-c", &script])?;
    wait_until(
        || {
            runs(&install.db())
                .iter()
                .any(|(_, cursor)| cursor == "running")
        },
        Duration::from_secs(20),
        "the step to start",
    )?;
    let pid = install.daemon_pid().expect("the auto-spawned daemon");

    install.install_new_build()?;
    assert!(
        !install.runs_installed_build(pid),
        "the swap didn't change the inode"
    );

    let upgraded = install.cued(&["upgrade", "--wait", "30s"])?;
    assert!(upgraded.status.success(), "{}", text(&upgraded));
    assert!(
        text(&upgraded).contains("upgraded in place"),
        "{}",
        text(&upgraded)
    );

    // The step was not cut short, and the upgrade waited for it.
    assert!(
        marker.exists(),
        "the upgrade returned before the step finished"
    );
    // Same process, new image: the supervisor sees no restart.
    assert_eq!(
        install.daemon_pid(),
        Some(pid),
        "the daemon was replaced, not re-executed"
    );
    assert!(
        install.runs_installed_build(pid),
        "the daemon is still on the old build"
    );
    // The run closed normally.
    assert_eq!(runs(&install.db()), vec![("done".into(), "done".into())]);

    // And the new image serves — and keeps the adopted locks and socket
    // out of its children: a step holding the lock open would keep a dead
    // daemon's flock alive.
    let fds = install.home().join("fds");
    let script = format!("ls -l /proc/self/fd/ > {}", fds.display());
    let submitted = install.cued(&["at", "in 1s", "--", "/bin/sh", "-c", &script])?;
    assert!(submitted.status.success(), "{}", text(&submitted));
    wait_until(
        || fds.exists(),
        Duration::from_secs(20),
        "a step under the new image",
    )?;
    std::thread::sleep(Duration::from_millis(200));
    let open = std::fs::read_to_string(&fds)?;
    for leaked in ["cued.lock", "sock.lock", "socket:"] {
        assert!(!open.contains(leaked), "a step inherited {leaked}:\n{open}");
    }
    Ok(())
}

/// A step that outlasts `--wait` makes the upgrade back off with nothing
/// changed; `--force` then goes ahead and the step is interrupted exactly
/// as by any restart (§3.4 Case 2: held for a human by default).
#[test]
fn a_step_outlasting_the_wait_abandons_unless_forced() -> Result<()> {
    let install = Install::new()?;
    install.cued(&["at", "in 1s", "--", "/bin/sh", "-c", "sleep 300"])?;
    wait_until(
        || {
            runs(&install.db())
                .iter()
                .any(|(_, cursor)| cursor == "running")
        },
        Duration::from_secs(20),
        "the step to start",
    )?;
    let pid = install.daemon_pid().expect("the auto-spawned daemon");
    install.install_new_build()?;

    let abandoned = install.cued(&["upgrade", "--wait", "1s"])?;
    assert!(!abandoned.status.success(), "{}", text(&abandoned));
    assert!(
        text(&abandoned).contains("abandoned"),
        "{}",
        text(&abandoned)
    );
    assert_eq!(install.daemon_pid(), Some(pid));
    assert!(
        !install.runs_installed_build(pid),
        "abandoned, yet the daemon re-executed"
    );
    assert_eq!(
        runs(&install.db()),
        vec![("running".into(), "running".into())]
    );

    // Not stuck draining: the daemon still starts new work.
    let marker = install.home().join("after-abandon");
    install.cued(&[
        "at",
        "in 1s",
        "--",
        "/usr/bin/touch",
        &marker.to_string_lossy(),
    ])?;
    wait_until(
        || marker.exists(),
        Duration::from_secs(20),
        "work after an abandoned upgrade",
    )?;

    let forced = install.cued(&["upgrade", "--wait", "1s", "--force"])?;
    assert!(forced.status.success(), "{}", text(&forced));
    assert_eq!(install.daemon_pid(), Some(pid));
    assert!(install.runs_installed_build(pid));
    wait_until(
        || {
            runs(&install.db())
                .first()
                .is_some_and(|(status, _)| status == "held")
        },
        Duration::from_secs(20),
        "the interrupted run to be held",
    )?;
    Ok(())
}

#[test]
fn nothing_new_installed_is_a_no_op() -> Result<()> {
    let install = Install::new()?;
    install.cued(&["list"])?;
    let pid = install.daemon_pid().expect("the auto-spawned daemon");

    let upgraded = install.cued(&["upgrade"])?;
    assert!(upgraded.status.success(), "{}", text(&upgraded));
    assert!(
        text(&upgraded).contains("already running"),
        "{}",
        text(&upgraded)
    );
    assert_eq!(install.daemon_pid(), Some(pid));
    Ok(())
}

/// A change subscriber (§5.1) stays connected indefinitely, so it must not
/// count as work an upgrade drains: the upgrade goes ahead at once, the
/// exec ends the stream, and the subscriber picks up on the new image.
#[test]
fn a_change_subscriber_does_not_hold_up_an_upgrade() -> Result<()> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    let install = Install::new()?;
    install.cued(&["list"])?;
    let pid = install.daemon_pid().expect("the auto-spawned daemon");
    let socket = install.home().join("cued.sock");
    let subscribe = || -> Result<BufReader<UnixStream>> {
        let mut stream = UnixStream::connect(&socket)?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        let mut line = serde_json::to_string(&cued::proto::Request {
            proto: cued::proto::PROTO_VERSION,
            body: cued::proto::RequestBody::Subscribe,
        })?;
        line.push('\n');
        stream.write_all(line.as_bytes())?;
        let mut reader = BufReader::new(stream);
        let mut reply = String::new();
        reader.read_line(&mut reply)?;
        assert!(reply.contains("subscribed"), "{reply}");
        Ok(reader)
    };
    let mut subscriber = subscribe()?;

    install.install_new_build()?;
    let started = Instant::now();
    let upgraded = install.cued(&["upgrade", "--wait", "5s"])?;
    assert!(upgraded.status.success(), "{}", text(&upgraded));
    assert!(
        text(&upgraded).contains("upgraded in place"),
        "{}",
        text(&upgraded)
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the upgrade waited out the subscriber"
    );
    assert_eq!(install.daemon_pid(), Some(pid));
    assert!(install.runs_installed_build(pid));

    let mut rest = String::new();
    assert_eq!(
        subscriber.read_line(&mut rest)?,
        0,
        "the stream outlived the exec: {rest}"
    );
    subscribe()?;
    Ok(())
}

#[test]
fn no_daemon_means_nothing_to_upgrade() -> Result<()> {
    let install = Install::new()?;
    let upgraded = install.cued(&["upgrade"])?;
    assert!(upgraded.status.success(), "{}", text(&upgraded));
    assert!(
        text(&upgraded).contains("no daemon is running"),
        "{}",
        text(&upgraded)
    );
    assert_eq!(install.daemon_pid(), None, "upgrade must not auto-spawn");
    Ok(())
}

/// #1: answering a ping is not proof of an upgrade. Here the new build is
/// fine at preflight but unexecutable by the time of the exec, so the
/// daemon falls back to its old image — which answers too, and must not be
/// reported as upgraded. It also keeps its install path, so fixing the
/// install and upgrading again works without a restart.
#[test]
fn a_failed_exec_is_reported_and_the_next_upgrade_still_works() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let install = Install::new()?;
    // Ready before anything is timed, so the swap below is one rename.
    let broken = install.exe.with_file_name("cued.broken");
    std::fs::copy(env!("CARGO_BIN_EXE_cued"), &broken)?;
    std::fs::set_permissions(&broken, std::fs::Permissions::from_mode(0o644))?;

    install.cued(&["at", "in 1s", "--", "/bin/sh", "-c", "sleep 8"])?;
    wait_until(
        || {
            runs(&install.db())
                .iter()
                .any(|(_, cursor)| cursor == "running")
        },
        Duration::from_secs(20),
        "the step to start",
    )?;
    let pid = install.daemon_pid().expect("the auto-spawned daemon");
    install.install_new_build()?;
    let daemon_log = install.home().join(".local/share/cued/daemon.log");

    let upgrade = isolated(Command::new(&install.exe), install.home())
        .args(["upgrade", "--wait", "30s"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    // Past preflight, mid-drain (the daemon says so once preflight has
    // passed): swap in a build that can't be executed.
    wait_until(
        || {
            std::fs::read_to_string(&daemon_log)
                .is_ok_and(|log| log.contains("upgrade requested — draining"))
        },
        Duration::from_secs(20),
        "the drain to begin",
    )?;
    std::fs::rename(&broken, &install.exe)?;
    assert_eq!(
        runs(&install.db())
            .first()
            .map(|(_, cursor)| cursor.as_str()),
        Some("running"),
        "the step ended before the swap; the test raced itself"
    );

    let failed = upgrade.wait_with_output()?;
    assert!(!failed.status.success(), "{}", text(&failed));
    assert!(
        text(&failed).contains("still running the previous build"),
        "{}",
        text(&failed)
    );
    assert_eq!(
        install.daemon_pid(),
        Some(pid),
        "the fallback should keep the pid"
    );
    // Our install path is the broken file now; ask through the original.
    let listed = isolated(Command::new(env!("CARGO_BIN_EXE_cued")), install.home())
        .arg("list")
        .stdin(Stdio::null())
        .output()?;
    assert!(listed.status.success(), "{}", text(&listed));

    install.install_new_build()?;
    let upgraded = install.cued(&["upgrade", "--wait", "30s"])?;
    assert!(upgraded.status.success(), "{}", text(&upgraded));
    assert!(install.runs_installed_build(pid));
    Ok(())
}

/// #2: `wait_secs` comes off the socket, from any same-uid client. The
/// largest value used to overflow the drain deadline and panic the daemon.
#[test]
fn an_enormous_wait_is_capped_rather_than_crashing_the_daemon() -> Result<()> {
    use std::io::{BufRead, BufReader, Write};
    let install = Install::new()?;
    install.cued(&[
        "at",
        "in 1s",
        "--name",
        "long",
        "--",
        "/bin/sh",
        "-c",
        "sleep 300",
    ])?;
    wait_until(
        || {
            runs(&install.db())
                .iter()
                .any(|(_, cursor)| cursor == "running")
        },
        Duration::from_secs(20),
        "the step to start",
    )?;
    let pid = install.daemon_pid().expect("the auto-spawned daemon");
    install.install_new_build()?;

    let mut stream = std::os::unix::net::UnixStream::connect(install.home().join("cued.sock"))?;
    writeln!(
        stream,
        r#"{{"proto":1,"body":{{"cmd":"upgrade","wait_secs":{},"force":false}}}}"#,
        u64::MAX
    )?;
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        0,
        "the daemon died on the request"
    );
    // Draining, still serving — and ending the step completes the drain.
    let cancelled = install.cued(&["cancel", "long"])?;
    assert!(cancelled.status.success(), "{}", text(&cancelled));

    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    let mut reply = String::new();
    BufReader::new(&stream).read_line(&mut reply)?;
    assert!(reply.contains("upgrading"), "{reply}");
    wait_until(
        || install.runs_installed_build(pid),
        Duration::from_secs(20),
        "the daemon to re-execute",
    )?;
    Ok(())
}

/// #3: a daemon from before this command can't decode it. The CLI's usual
/// stale-daemon advice is "run `cued upgrade`" — the command that just
/// failed — so this case has to say what actually helps.
#[test]
fn a_daemon_predating_upgrade_gets_restart_advice() -> Result<()> {
    use std::io::{BufRead, BufReader, Write};
    let install = Install::new()?;
    let socket = std::os::unix::net::UnixListener::bind(install.home().join("cued.sock"))?;
    let old_daemon = std::thread::spawn(move || -> Result<()> {
        let (mut stream, _) = socket.accept()?;
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line)?;
        writeln!(
            stream,
            r#"{{"result":"error","message":"bad request: unknown variant `upgrade`"}}"#
        )?;
        Ok(())
    });

    let refused = install.cued(&["upgrade"])?;
    old_daemon.join().expect("fake daemon")?;
    assert!(!refused.status.success(), "{}", text(&refused));
    assert!(
        text(&refused).contains("predates `cued upgrade`"),
        "{}",
        text(&refused)
    );
    assert!(
        !text(&refused).contains("run `cued upgrade`,"),
        "{}",
        text(&refused)
    );
    Ok(())
}

/// #4: a stop asked for while a forced upgrade is interrupting steps used
/// to wait in tokio's queue and vanish at the exec, so the "stopped" daemon
/// carried on as the new build. Here the step ignores SIGTERM, which holds
/// the interrupt open for the whole default 10s kill_grace.
#[test]
fn a_stop_during_a_forced_upgrade_stops_the_daemon() -> Result<()> {
    let install = Install::new()?;
    install.cued(&[
        "at",
        "in 1s",
        "--",
        "/bin/sh",
        "-c",
        "trap '' TERM; sleep 300",
    ])?;
    wait_until(
        || {
            runs(&install.db())
                .iter()
                .any(|(_, cursor)| cursor == "running")
        },
        Duration::from_secs(20),
        "the step to start",
    )?;
    let pid = install.daemon_pid().expect("the auto-spawned daemon");
    install.install_new_build()?;

    let upgrade = isolated(Command::new(&install.exe), install.home())
        .args(["upgrade", "--wait", "1s", "--force"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    // Past the 1s drain, inside the kill_grace of the forced interrupt.
    std::thread::sleep(Duration::from_secs(3));
    // SAFETY: signalling the daemon this test started, by pid.
    unsafe { libc::kill(pid, libc::SIGTERM) };

    let stopped = upgrade.wait_with_output()?;
    assert!(!stopped.status.success(), "{}", text(&stopped));
    assert!(
        text(&stopped).contains("shutting down"),
        "{}",
        text(&stopped)
    );
    wait_until(
        || unsafe { libc::kill(pid, 0) } != 0,
        Duration::from_secs(30),
        "the daemon to exit",
    )?;
    Ok(())
}
