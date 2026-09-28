//! Clean-shutdown tests (DESIGN.md §2.2, §2.3). Driven as a subprocess with
//! real signals, because that is the only way to exercise the thing: an
//! in-process test cannot send itself SIGTERM without ending the test.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

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

fn runs(db: &std::path::Path) -> Vec<(i64, String, String)> {
    let Ok(conn) = rusqlite_lite::open(db) else {
        return Vec::new();
    };
    conn
}

/// Minimal read-only peek at the store without adding a dependency: shell
/// out to the daemon's own CLI would need a daemon, and we have just killed
/// it, so read the file through sqlx on a throwaway runtime instead.
mod rusqlite_lite {
    use anyhow::Result;
    pub fn open(db: &std::path::Path) -> Result<Vec<(i64, String, String)>> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        rt.block_on(async {
            let pool =
                sqlx::SqlitePool::connect(&format!("sqlite://{}?mode=ro", db.display())).await?;
            let rows: Vec<(i64, String, String)> =
                sqlx::query_as("SELECT id, status, cursor_kind FROM runs ORDER BY id")
                    .fetch_all(&pool)
                    .await?;
            Ok(rows)
        })
    }
}

/// §2.2 lists four things that terminate a step the same way; a clean daemon
/// shutdown is the fourth, and it went unimplemented. A `systemctl restart`
/// or a Ctrl-C left the step's whole `setsid` process group running, orphaned
/// and invisible, while the next daemon reconciled the run as interrupted.
///
/// §3.4 Case 2 is the *other* half of the contract: the run must be left
/// exactly as an unclean kill would leave it — cursor still `Running` — so
/// the next startup applies `on_interrupt` rather than the shutdown deciding
/// the run's fate. Closing it here would write a maintenance restart up as a
/// failure and fire `on_failure`.
#[test]
fn sigterm_kills_the_process_group_and_leaves_the_run_for_reconciliation() -> Result<()> {
    let dir = private_tempdir()?;
    let home = dir.path();
    let db = home.join(".local/share/cued/cued.db");
    let marker = home.join("grandchild-ran");
    let exe = env!("CARGO_BIN_EXE_cued");

    let cued = |args: &[&str]| {
        isolated(Command::new(exe), home)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
    };

    // A step that also backgrounds a grandchild: if only the direct child
    // were signalled, the grandchild would outlive the shutdown (§2.2).
    let script = format!("(sleep 4; touch {}) & sleep 300", marker.display());
    cued(&[
        "at", "in 1s", "--name", "longrun", "--", "/bin/sh", "-c", &script,
    ])?;

    wait_until(
        || runs(&db).iter().any(|(_, _, cursor)| cursor == "running"),
        Duration::from_secs(20),
        "the step to start",
    )?;

    // The daemon was auto-spawned; find it by the HOME it is serving.
    let pid = daemon_pid(home).expect("the auto-spawned daemon");
    // SAFETY: signalling a process this test started, by pid.
    unsafe { libc::kill(pid, libc::SIGTERM) };

    wait_until(
        || unsafe { libc::kill(pid, 0) } != 0,
        Duration::from_secs(20),
        "the daemon to exit",
    )?;

    // §3.4 Case 2: left mid-run, for the next startup to resolve.
    let after = runs(&db);
    assert_eq!(
        after.first().map(|(_, _, cursor)| cursor.as_str()),
        Some("running"),
        "the shutdown decided the run's fate instead of leaving it: {after:?}"
    );

    // §2.2: the whole group went, grandchild included.
    std::thread::sleep(Duration::from_secs(5));
    assert!(!marker.exists(), "a grandchild outlived the shutdown");

    // And the next daemon parks it, with the §3.5 notification.
    cued(&["list", "--all"])?;
    wait_until(
        || runs(&db).iter().any(|(_, status, _)| status == "held"),
        Duration::from_secs(20),
        "reconciliation to park the run",
    )?;

    if let Some(pid) = daemon_pid(home) {
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
    Ok(())
}

/// codex #1: shutdown signalled only the registry snapshot it took, and a
/// step being claimed at that moment registers *after* it — then spawns a
/// process the daemon can no longer reach and is about to stop waiting for.
///
/// Racing that window directly would be flaky, so this drives the many-steps
/// case instead: several jobs starting at once, SIGTERM mid-flight, and
/// nothing of theirs left running afterwards.
#[test]
fn a_shutdown_mid_launch_leaves_no_process_behind() -> Result<()> {
    let dir = private_tempdir()?;
    let home = dir.path();
    let db = home.join(".local/share/cued/cued.db");
    let exe = env!("CARGO_BIN_EXE_cued");

    let cued = |args: &[&str]| {
        isolated(Command::new(exe), home)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
    };

    // Eight jobs all due at once, each leaving a marker if it outlives the
    // shutdown. Staggering the claims is what opens the window.
    for n in 0..8 {
        let marker = home.join(format!("survivor-{n}"));
        let script = format!("(sleep 3; touch {}) & sleep 300", marker.display());
        cued(&[
            "at",
            "in 2s",
            "--name",
            &format!("j{n}"),
            "--",
            "/bin/sh",
            "-c",
            &script,
        ])?;
    }

    // Signal as the wave starts, so some steps are mid-claim.
    wait_until(
        || runs(&db).iter().any(|(_, _, cursor)| cursor == "running"),
        Duration::from_secs(20),
        "the first step to start",
    )?;
    let pid = daemon_pid(home).expect("the daemon");
    unsafe { libc::kill(pid, libc::SIGTERM) };
    wait_until(
        || unsafe { libc::kill(pid, 0) } != 0,
        Duration::from_secs(30),
        "the daemon to exit",
    )?;

    std::thread::sleep(Duration::from_secs(5));
    let survivors: Vec<String> = (0..8)
        .filter(|n| home.join(format!("survivor-{n}")).exists())
        .map(|n| format!("j{n}"))
        .collect();
    assert!(
        survivors.is_empty(),
        "these steps outlived the shutdown: {survivors:?}"
    );
    Ok(())
}

/// Find the daemon serving this HOME, so a stray daemon from another test
/// can never be the one we signal.
/// paths.rs refuses a socket directory that isn't mode 0700, and
/// `tempdir()` honours the umask — under the common 0002 that is 0775, the
/// CLI exits 1 before spawning anything, and the tests then time out waiting
/// for a step that was never scheduled.
fn private_tempdir() -> Result<tempfile::TempDir> {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir()?;
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

/// Keep the daemon off the host's data and session bus: an inherited
/// XDG_DATA_HOME would point it at the real store, and an inherited
/// DBUS_SESSION_BUS_ADDRESS would deliver the parked run's on_hold
/// notification to the real desktop.
fn isolated(mut command: Command, home: &std::path::Path) -> Command {
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

fn daemon_pid(home: &std::path::Path) -> Option<i32> {
    let want = format!("HOME={}", home.display());
    for entry in std::fs::read_dir("/proc").ok()? {
        let entry = entry.ok()?;
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        let Ok(environ) = std::fs::read(entry.path().join("environ")) else {
            continue;
        };
        let Ok(comm) = std::fs::read_to_string(entry.path().join("comm")) else {
            continue;
        };
        if comm.trim() == "cued" && environ.split(|b| *b == 0).any(|v| v == want.as_bytes()) {
            return Some(pid);
        }
    }
    None
}
