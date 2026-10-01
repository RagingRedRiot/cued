//! `cued wait` and `--wait`: block until a run settles and exit with its
//! outcome, printing status and exit codes but never the captured output.
//! Driven as real processes, because the exit status is the interface.

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

struct Home(tempfile::TempDir);

impl Home {
    fn new() -> Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir()?;
        // paths.rs refuses a socket directory that isn't 0700.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))?;
        Ok(Self(dir))
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn cued(&self, args: &[&str]) -> Result<Output> {
        self.cued_env(args, &[])
    }

    fn cued_env(&self, args: &[&str], vars: &[(&str, &str)]) -> Result<Output> {
        let home = self.path();
        Ok(Command::new(env!("CARGO_BIN_EXE_cued"))
            .args(args)
            .envs(vars.iter().copied())
            .env("HOME", home)
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("CUED_SOCKET_DIR", home)
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}", home.join("no-bus").display()),
            )
            .stdin(Stdio::null())
            .output()?)
    }

    /// Write the MCP policy the daemon reads (per request, so no restart).
    fn policy(&self, text: &str) -> Result<()> {
        let dir = self.path().join(".config/cued");
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("mcp.toml"), text)?;
        Ok(())
    }

    fn daemon_pid(&self) -> Option<i32> {
        let want = format!("HOME={}", self.path().display());
        for entry in std::fs::read_dir("/proc").ok()? {
            let entry = entry.ok()?;
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
                continue;
            };
            let (Ok(environ), Ok(cmdline)) = (
                std::fs::read(entry.path().join("environ")),
                std::fs::read(entry.path().join("cmdline")),
            ) else {
                continue;
            };
            let is_daemon = cmdline.split(|b| *b == 0).any(|arg| arg == b"daemon");
            if is_daemon && environ.split(|b| *b == 0).any(|v| v == want.as_bytes()) {
                return Some(pid);
            }
        }
        None
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        if let Some(pid) = self.daemon_pid() {
            // SAFETY: signalling the daemon this test started, by pid.
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn exit_status_is_the_run_outcome() -> Result<()> {
    let home = Home::new()?;
    home.policy("logs = \"on\"\n")?;

    // Failure: 3, the failing step's exit code shown, its output not.
    let chained = home.cued(&["chain", "echo secret-output", "--then", "exit 3", "--wait"])?;
    let text = stdout(&chained);
    assert_eq!(chained.status.code(), Some(3), "{text}");
    assert!(text.contains("j1.r1 failed"), "{text}");
    assert!(text.contains("exit 3"), "{text}");
    assert!(
        !text.contains("secret-output"),
        "wait must not print logs: {text}"
    );

    // Waiting on a run that already ended answers at once, same status.
    let again = home.cued(&["wait", "j1"])?;
    assert_eq!(again.status.code(), Some(3));

    // Success: 0, and JSON says the same thing.
    let ok = home.cued(&["at", "--wait", "in 1s", "--", "true"])?;
    assert_eq!(ok.status.code(), Some(0), "{}", stdout(&ok));
    let json = home.cued(&["wait", "j2", "--json"])?;
    assert_eq!(json.status.code(), Some(0));
    let value: serde_json::Value = serde_json::from_str(stdout(&json).trim())?;
    assert_eq!(value["status"], "done");
    assert_eq!(value["exit"], 0);

    // Timeout: 124, and the job is untouched.
    home.cued(&["at", "in 1h", "--", "true"])?;
    let timed = home.cued(&["wait", "j3", "--timeout", "1s"])?;
    assert_eq!(timed.status.code(), Some(124));

    // Cancelled: 5.
    home.cued(&["cancel", "j3"])?;
    let cancelled = home.cued(&["wait", "j3"])?;
    assert_eq!(cancelled.status.code(), Some(5), "{}", stdout(&cancelled));

    // cued's own failure stays 1, and a usage error is clap's 2 — neither
    // can pass for a run's outcome.
    let missing = home.cued(&["wait", "j99"])?;
    assert_eq!(missing.status.code(), Some(1));
    for args in [
        &["wait", "--tiemout", "5m", "j1"][..],
        &["wait", "j1", "--timeout", "0s"],
        &["wait", "j1", "--timeout", "banana"],
        &["wait", "j1", "--timeout", "9223372036854775807s"],
    ] {
        let usage = home.cued(args)?;
        assert_eq!(usage.status.code(), Some(2), "{args:?}");
    }
    Ok(())
}

#[test]
fn a_recurring_job_waits_for_its_next_run() -> Result<()> {
    let home = Home::new()?;
    home.cued(&["every", "2s", "--", "true"])?;
    let started = Instant::now();
    let waited = home.cued(&["wait", "j1", "--timeout", "30s"])?;
    assert_eq!(waited.status.code(), Some(0), "{}", stdout(&waited));
    assert!(started.elapsed() < Duration::from_secs(30));
    Ok(())
}

/// A run interrupted by a daemon crash parks as Held (§3.4); that needs a
/// decision, so the waiter wakes with 4 rather than waiting for a run that
/// can't start.
#[test]
fn a_held_run_wakes_the_waiter() -> Result<()> {
    let home = Home::new()?;
    home.cued(&["chain", "sleep 30"])?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let logs = home.cued(&["logs", "j1", "--json"])?;
        if stdout(&logs).contains("\"ended_at\": null") {
            break;
        }
        if Instant::now() > deadline {
            bail!("step never started");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let pid = home.daemon_pid().expect("daemon running");
    // SAFETY: killing the daemon this test started, by pid.
    unsafe { libc::kill(pid, libc::SIGKILL) };
    std::thread::sleep(Duration::from_millis(200));

    // `wait` never spawns a daemon; any other command does, and the new
    // one reconciles.
    home.cued(&["list"])?;
    let waited = home.cued(&["wait", "j1", "--timeout", "30s"])?;
    let text = stdout(&waited);
    assert_eq!(waited.status.code(), Some(4), "{text}");
    assert!(text.contains("cued continue j1"), "{text}");
    Ok(())
}

/// Stop this home's daemon and start a fresh one with `vars` in its
/// environment — the one place the `read` gate takes settings from.
fn restart_daemon(home: &Home, vars: &[(&str, &str)]) -> Result<()> {
    if let Some(pid) = home.daemon_pid() {
        // SAFETY: signalling the daemon this test started, by pid.
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while home.daemon_pid().is_some() {
        if Instant::now() > deadline {
            bail!("daemon didn't stop");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    home.cued_env(&["list"], vars)?;
    Ok(())
}

/// `cued wait` is the shell command a model restricted to MCP may be
/// allowed, so `read = "off"` closes it too. The daemon enforces it from
/// its own environment, so nothing the waiter's shell sets can reopen it.
#[test]
fn read_off_closes_wait() -> Result<()> {
    let home = Home::new()?;
    home.cued(&["chain", "true", "--wait"])?;
    let config_dir = home.path().join(".config/cued");
    std::fs::create_dir_all(&config_dir)?;
    let policy = config_dir.join("mcp.toml");

    std::fs::write(&policy, "read = \"off\"\n")?;
    let refused = home.cued(&["wait", "j1"])?;
    assert_eq!(refused.status.code(), Some(1));
    let message = String::from_utf8_lossy(&refused.stderr);
    assert!(message.contains("read"), "{message}");
    assert!(
        stdout(&refused).is_empty(),
        "nothing reported: {}",
        stdout(&refused)
    );

    // The waiter's environment counts for nothing: not the MCP overrides,
    // and not a config dir pointed somewhere without a policy.
    let elsewhere = home.path().join("open.toml");
    std::fs::write(&elsewhere, "read = \"on\"\n")?;
    let empty = home.path().join("empty");
    for vars in [
        vec![
            ("CUED_MCP_READ", "on"),
            ("CUED_MCP_CONFIG", elsewhere.to_str().unwrap()),
        ],
        vec![("XDG_CONFIG_HOME", empty.to_str().unwrap())],
    ] {
        let overridden = home.cued_env(&["wait", "j1"], &vars)?;
        assert_eq!(overridden.status.code(), Some(1), "{vars:?}");
    }

    // `--wait` meets the same gate — checked before submitting, so no job
    // is created that nothing will watch.
    let refused = home.cued(&["chain", "true", "--wait"])?;
    assert_eq!(refused.status.code(), Some(1));
    assert!(stdout(&refused).is_empty(), "{}", stdout(&refused));
    let message = String::from_utf8_lossy(&refused.stderr);
    assert!(message.contains("not submitted"), "{message}");
    let list = home.cued(&["list", "--all", "--json"])?;
    assert!(!stdout(&list).contains("\"j2\""), "{}", stdout(&list));

    // The daemon's own environment can tighten it.
    std::fs::write(&policy, "read = \"on\"\n")?;
    assert_eq!(home.cued(&["wait", "j1"])?.status.code(), Some(0));
    restart_daemon(&home, &[("CUED_MCP_READ", "off")])?;
    assert_eq!(home.cued(&["wait", "j1"])?.status.code(), Some(1));
    std::fs::write(&elsewhere, "read = \"off\"\n")?;
    restart_daemon(&home, &[("CUED_MCP_CONFIG", elsewhere.to_str().unwrap())])?;
    assert_eq!(home.cued(&["wait", "j1"])?.status.code(), Some(1));

    // A policy that can't be read fails closed, as an MCP call would.
    restart_daemon(&home, &[])?;
    std::fs::write(&policy, "read = \"maybe\"\n")?;
    assert_eq!(home.cued(&["wait", "j1"])?.status.code(), Some(1));
    Ok(())
}

/// A daemon started by `wait` would take the waiter's environment, and with
/// it the policy it enforces — so `wait` never starts one.
#[test]
fn wait_never_spawns_a_daemon() -> Result<()> {
    let home = Home::new()?;
    home.cued(&["chain", "true", "--wait"])?;
    restart_daemon(&home, &[])?;
    let pid = home.daemon_pid().expect("daemon running");
    // SAFETY: signalling the daemon this test started, by pid.
    unsafe { libc::kill(pid, libc::SIGTERM) };
    while home.daemon_pid().is_some() {
        std::thread::sleep(Duration::from_millis(50));
    }
    // No daemon is no answer, not a timeout: nothing will finish the run,
    // so it's a cued error (1), even when --timeout runs out first.
    let waited = home.cued(&["wait", "j1", "--timeout", "1s"])?;
    assert_eq!(waited.status.code(), Some(1), "{}", stdout(&waited));
    let message = String::from_utf8_lossy(&waited.stderr);
    assert!(message.contains("no daemon is running"), "{message}");
    assert!(home.daemon_pid().is_none(), "wait started a daemon");
    Ok(())
}

/// A waiter on a recurring job's next run, when the job stops before that
/// run comes, ends as "ended" (5) — not by reporting the earlier run that
/// had already settled before the wait began.
#[test]
fn a_job_that_stops_before_its_next_run_ends_the_wait() -> Result<()> {
    let home = Home::new()?;
    // Not an hour off: the waiter sleeps toward the next firing (up to
    // 30s), and only notices the cancel when it wakes.
    home.cued(&["every", "6s", "--at", "in 1s", "--", "true"])?;
    // The first firing settles; the next is ~6s off.
    let first = home.cued(&["wait", "j1", "--timeout", "30s"])?;
    assert_eq!(first.status.code(), Some(0), "{}", stdout(&first));

    let exe = env!("CARGO_BIN_EXE_cued");
    let root = home.path();
    let waiter = Command::new(exe)
        .args(["wait", "j1"])
        .env("HOME", root)
        .env("XDG_DATA_HOME", root.join(".local/share"))
        .env("XDG_CONFIG_HOME", root.join(".config"))
        .env("CUED_SOCKET_DIR", root)
        .stdout(Stdio::piped())
        .spawn()?;
    std::thread::sleep(Duration::from_millis(1500));
    home.cued(&["cancel", "j1"])?;
    let ended = waiter.wait_with_output()?;
    let text = stdout(&ended);
    assert_eq!(ended.status.code(), Some(5), "{text}");
    assert!(text.contains("no run left"), "{text}");

    // Asked again once it's over: cancelled has ended, whatever its last
    // run did — which the summary still says.
    let after = home.cued(&["wait", "j1"])?;
    let text = stdout(&after);
    assert_eq!(after.status.code(), Some(5), "{text}");
    assert!(text.contains("last run, j1.r1, was done"), "{text}");
    Ok(())
}

/// Under the default `overlap = skip`, firings during a long run become
/// Skipped rows with ids above it. They record firings, not runs: the
/// waiter must stay on the run in progress and report it.
#[test]
fn skipped_firings_are_passed_over() -> Result<()> {
    let home = Home::new()?;
    home.cued(&["every", "1s", "--", "sleep", "3"])?;
    // Let a firing or two land as Skipped behind the running r1.
    std::thread::sleep(Duration::from_millis(2500));
    let waited = home.cued(&["wait", "j1", "--timeout", "30s"])?;
    let text = stdout(&waited);
    assert_eq!(waited.status.code(), Some(0), "{text}");
    assert!(text.contains("j1.r1 done"), "{text}");
    home.cued(&["cancel", "j1"])?;
    Ok(())
}

/// A job that ends without ever running is "ended" (5), whether that
/// happens during a wait or before one starts — not a cued error (1).
#[test]
fn a_job_that_never_ran_has_ended() -> Result<()> {
    let home = Home::new()?;
    home.cued(&["every", "1h", "--at", "in 1h", "--", "true"])?;
    home.cued(&["cancel", "j1"])?;
    let waited = home.cued(&["wait", "j1", "--json"])?;
    assert_eq!(waited.status.code(), Some(5), "{}", stdout(&waited));
    let value: serde_json::Value = serde_json::from_str(stdout(&waited).trim())?;
    assert_eq!(value["status"], "cancelled");
    assert_eq!(value["run"], serde_json::Value::Null);
    Ok(())
}

/// Per-step results are what MCP `logs` governs (off by default), so the
/// daemon sends them to `cued wait` only while it's on. The outcome itself
/// is status, under `read`.
#[test]
fn step_results_follow_logs() -> Result<()> {
    let home = Home::new()?;
    home.cued(&["chain", "true", "--then", "exit 3", "--wait"])?;

    let withheld = home.cued(&["wait", "j1"])?;
    let text = stdout(&withheld);
    assert_eq!(withheld.status.code(), Some(3), "{text}");
    assert!(text.contains("j1.r1 failed"), "{text}");
    assert!(
        !text.contains("exit 3"),
        "steps shown with logs off: {text}"
    );
    let json = home.cued(&["wait", "j1", "--json"])?;
    let value: serde_json::Value = serde_json::from_str(stdout(&json).trim())?;
    assert_eq!(value["attempts"], serde_json::Value::Null);

    home.policy("logs = \"on\"\n")?;
    let shown = home.cued(&["wait", "j1"])?;
    assert!(stdout(&shown).contains("exit 3"), "{}", stdout(&shown));
    // Nothing in the waiter's shell turns it on.
    home.policy("logs = \"off\"\n")?;
    let overridden = home.cued_env(&["wait", "j1"], &[("CUED_MCP_LOGS", "on")])?;
    assert!(
        !stdout(&overridden).contains("exit 3"),
        "{}",
        stdout(&overridden)
    );
    Ok(())
}

/// A daemon that accepts but never answers can't hold a waiter past its
/// `--timeout`: each exchange's I/O limit is clamped to the time left.
#[test]
fn a_hung_daemon_cannot_outlast_the_timeout() -> Result<()> {
    let home = Home::new()?;
    let listener = std::os::unix::net::UnixListener::bind(home.path().join("cued.sock"))?;
    // Accept and hold every connection open, never replying.
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming().flatten() {
            held.push(stream);
        }
    });
    let started = Instant::now();
    let waited = home.cued(&["wait", "j1", "--timeout", "1s"])?;
    assert_eq!(waited.status.code(), Some(124), "{}", stdout(&waited));
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "held for {:?}",
        started.elapsed()
    );
    Ok(())
}

/// `cued wait` is answered under the daemon's policy, so an MCP server made
/// stricter only through its own launch environment says so at startup.
#[test]
fn mcp_warns_when_wait_would_be_looser() -> Result<()> {
    let home = Home::new()?;
    let quiet = home.cued(&["mcp"])?;
    let message = String::from_utf8_lossy(&quiet.stderr);
    assert!(!message.contains("warning"), "{message}");

    let stricter = home.cued_env(&["mcp"], &[("CUED_MCP_READ", "off")])?;
    let message = String::from_utf8_lossy(&stricter.stderr);
    assert!(message.contains("read off here"), "{message}");
    assert!(message.contains("CUED_MCP_READ=off"), "{message}");

    // Each switch is blamed on the layer that actually set it.
    let open = home.path().join("open.toml");
    std::fs::write(&open, "read = \"on\"\n")?;
    let from_env = home.cued_env(
        &["mcp"],
        &[
            ("CUED_MCP_CONFIG", open.to_str().unwrap()),
            ("CUED_MCP_READ", "off"),
        ],
    )?;
    let message = String::from_utf8_lossy(&from_env.stderr);
    assert!(message.contains("CUED_MCP_READ=off"), "{message}");
    assert!(!message.contains("open.toml"), "{message}");

    let strict = home.path().join("strict.toml");
    std::fs::write(&strict, "read = \"off\"\n")?;
    let from_file = home.cued_env(&["mcp"], &[("CUED_MCP_CONFIG", strict.to_str().unwrap())])?;
    let message = String::from_utf8_lossy(&from_file.stderr);
    assert!(message.contains("strict.toml"), "{message}");
    Ok(())
}

const RUNNING: &str = r#"{"result":"job_run","job":1,"status":"active","more":false,"run":{"id":1,"status":"running","ended_at":null,"fail_reason":null},"last_id":1,"steps":null}"#;
const DONE: &str = r#"{"result":"job_run","job":1,"status":"active","more":false,"run":{"id":1,"status":"done","ended_at":null,"fail_reason":null},"last_id":1,"steps":null}"#;

/// A daemon double on `listener`: each accepted connection gets its own
/// thread, which reads the request line and hands the stream and the
/// connection's index to `answer`.
fn fake_daemon(
    listener: std::os::unix::net::UnixListener,
    answer: impl Fn(&std::os::unix::net::UnixStream, usize) + Send + Sync + 'static,
) {
    use std::io::{BufRead, BufReader};
    let answer = std::sync::Arc::new(answer);
    std::thread::spawn(move || {
        for (n, stream) in listener.incoming().flatten().enumerate() {
            let answer = std::sync::Arc::clone(&answer);
            std::thread::spawn(move || {
                let mut request = String::new();
                if BufReader::new(&stream).read_line(&mut request).unwrap_or(0) > 0 {
                    answer(&stream, n);
                }
            });
        }
    });
}

fn send(stream: &std::os::unix::net::UnixStream, text: &str) {
    use std::io::Write;
    let _ = (&*stream).write_all(text.as_bytes());
}

/// A run that settles after `--timeout` has passed is a timeout, not an
/// outcome reported late: the pause between polls is clamped to the time
/// left, and the deadline is checked again before polling.
#[test]
fn an_outcome_after_the_deadline_is_a_timeout() -> Result<()> {
    let home = Home::new()?;
    // First answer at ~0.4s says running; anything polled after says done.
    // A full-second pause would poll again at ~1.4s and report "done".
    let listener = std::os::unix::net::UnixListener::bind(home.path().join("cued.sock"))?;
    fake_daemon(listener, |stream, n| {
        if n == 0 {
            std::thread::sleep(Duration::from_millis(400));
            send(stream, &format!("{RUNNING}\n"));
        } else {
            send(stream, &format!("{DONE}\n"));
        }
    });
    let started = Instant::now();
    let waited = home.cued(&["wait", "j1", "--timeout", "1s"])?;
    assert_eq!(waited.status.code(), Some(124), "{}", stdout(&waited));
    assert!(
        started.elapsed() < Duration::from_millis(1400),
        "{:?}",
        started.elapsed()
    );
    Ok(())
}

/// Connect without blocking; `Err` with EAGAIN means the queue is full.
fn connect_nonblocking(path: &Path) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;
    use std::os::unix::ffi::OsStrExt;
    // SAFETY: sockaddr_un is plain old data; all-zero is valid.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = path.as_os_str().as_bytes();
    for (slot, byte) in addr.sun_path.iter_mut().zip(bytes) {
        *slot = *byte as libc::c_char;
    }
    let len = (std::mem::size_of::<libc::sa_family_t>() + bytes.len() + 1) as libc::socklen_t;
    // SAFETY: plain socket/connect calls on a socket this function owns.
    unsafe {
        let raw = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_NONBLOCK, 0);
        if raw < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let socket = std::os::fd::OwnedFd::from_raw_fd(raw);
        let addr = (&raw const addr).cast::<libc::sockaddr>();
        if libc::connect(std::os::fd::AsRawFd::as_raw_fd(&socket), addr, len) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(socket)
    }
}

/// A daemon whose accept queue is full makes a blocking connect wait with
/// no limit. The waiter's connect is bounded, so `--timeout` still holds.
#[test]
fn a_full_accept_queue_cannot_outlast_the_timeout() -> Result<()> {
    let home = Home::new()?;
    let socket = home.path().join("cued.sock");
    // Listening, never accepting.
    let _listener = std::os::unix::net::UnixListener::bind(&socket)?;
    let mut queued = Vec::new();
    loop {
        match connect_nonblocking(&socket) {
            Ok(connection) => queued.push(connection),
            Err(error) if error.raw_os_error() == Some(libc::EAGAIN) => break,
            Err(error) => return Err(error.into()),
        }
        if queued.len() > 100_000 {
            bail!("accept queue never filled");
        }
    }
    let started = Instant::now();
    let waited = home.cued(&["wait", "j1", "--timeout", "1s"])?;
    assert_eq!(waited.status.code(), Some(124), "{}", stdout(&waited));
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    Ok(())
}

/// The deadline bounds the whole exchange, not each call in it: a reply
/// that trickles in, each piece inside a fresh read timeout, is still a
/// timeout once `--timeout` has passed.
#[test]
fn a_trickled_reply_cannot_outlast_the_timeout() -> Result<()> {
    let home = Home::new()?;
    let listener = std::os::unix::net::UnixListener::bind(home.path().join("cued.sock"))?;
    fake_daemon(listener, |stream, _| {
        let (head, tail) = DONE.split_at(DONE.len() / 2);
        std::thread::sleep(Duration::from_millis(300));
        send(stream, head);
        std::thread::sleep(Duration::from_millis(900));
        send(stream, &format!("{tail}\n"));
    });
    let started = Instant::now();
    let waited = home.cued(&["wait", "j1", "--timeout", "1s"])?;
    assert_eq!(waited.status.code(), Some(124), "{}", stdout(&waited));
    assert!(
        started.elapsed() < Duration::from_millis(1150),
        "{:?}",
        started.elapsed()
    );
    Ok(())
}

/// Time spent waiting for the accept queue comes out of the same budget
/// as the reply: a slow accept followed by a slow reply is a timeout.
#[test]
fn a_slow_accept_and_reply_cannot_outlast_the_timeout() -> Result<()> {
    let home = Home::new()?;
    let socket = home.path().join("cued.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket)?;
    let mut queued = Vec::new();
    loop {
        match connect_nonblocking(&socket) {
            Ok(connection) => queued.push(connection),
            Err(error) if error.raw_os_error() == Some(libc::EAGAIN) => break,
            Err(error) => return Err(error.into()),
        }
        if queued.len() > 100_000 {
            bail!("accept queue never filled");
        }
    }
    // Start accepting at ~0.6s; answer the waiter ~0.6s after that.
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(600));
        fake_daemon(listener, |stream, _| {
            std::thread::sleep(Duration::from_millis(600));
            send(stream, &format!("{DONE}\n"));
        });
    });
    let started = Instant::now();
    let waited = home.cued(&["wait", "j1", "--timeout", "1s"])?;
    assert_eq!(waited.status.code(), Some(124), "{}", stdout(&waited));
    assert!(
        started.elapsed() < Duration::from_millis(1150),
        "{:?}",
        started.elapsed()
    );
    drop(queued);
    Ok(())
}

/// The startup warning only reads the default policy; a broken default file
/// that this server doesn't use must not stop it starting.
#[test]
fn mcp_starts_despite_a_broken_unused_default_policy() -> Result<()> {
    let home = Home::new()?;
    home.policy("read = \"maybe\"\n")?;
    let used = home.path().join("used.toml");
    std::fs::write(&used, "read = \"on\"\n")?;
    let started = home.cued_env(&["mcp"], &[("CUED_MCP_CONFIG", used.to_str().unwrap())])?;
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    Ok(())
}

/// A paused job starts no new work, so a wait that would need some is
/// refused rather than left to last until someone resumes it. A run that
/// has already settled is still reported.
#[test]
fn a_paused_job_cannot_be_waited_on() -> Result<()> {
    let home = Home::new()?;
    home.cued(&["every", "1h", "--at", "in 1s", "--", "true"])?;
    let first = home.cued(&["wait", "j1", "--timeout", "30s"])?;
    assert_eq!(first.status.code(), Some(0), "{}", stdout(&first));
    home.cued(&["pause", "j1"])?;

    let refused = home.cued(&["wait", "j1"])?;
    assert_eq!(refused.status.code(), Some(1), "{}", stdout(&refused));
    let message = String::from_utf8_lossy(&refused.stderr);
    assert!(message.contains("paused"), "{message}");
    assert!(message.contains("cued resume j1"), "{message}");

    let settled = home.cued(&["wait", "j1", "--run", "1"])?;
    assert_eq!(settled.status.code(), Some(0), "{}", stdout(&settled));
    Ok(())
}

/// `--run N` means that row, whatever it is: a Skipped one is reported as
/// itself, and one not created yet is waited for while the job can fire.
#[test]
fn run_n_is_exact() -> Result<()> {
    let home = Home::new()?;
    home.cued(&["every", "1s", "--", "sleep", "3"])?;
    // r1 runs; the firings behind it land as Skipped rows.
    std::thread::sleep(Duration::from_millis(2500));
    let skipped = home.cued(&["wait", "j1", "--run", "2"])?;
    let text = stdout(&skipped);
    assert_eq!(skipped.status.code(), Some(5), "{text}");
    assert!(text.contains("j1.r2 skipped"), "{text}");

    let future = home.cued(&["wait", "j1", "--run", "99", "--timeout", "1s"])?;
    assert_eq!(future.status.code(), Some(124), "{}", stdout(&future));

    home.cued(&["cancel", "j1"])?;
    let never = home.cued(&["wait", "j1", "--run", "99"])?;
    assert_eq!(never.status.code(), Some(1));
    let message = String::from_utf8_lossy(&never.stderr);
    assert!(message.contains("won't create one"), "{message}");
    Ok(())
}

/// Only a missing socket or a refused connection means "no daemon". A
/// socket that can't be used at all fails at once with the real cause,
/// instead of being retried and then blamed on a daemon that isn't there.
#[test]
fn an_unusable_socket_is_not_no_daemon() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        // root ignores the permission this test relies on.
        return Ok(());
    }
    let home = Home::new()?;
    let socket = home.path().join("cued.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&socket)?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o000))?;
    let started = Instant::now();
    let waited = home.cued(&["wait", "j1"])?;
    assert_eq!(waited.status.code(), Some(1));
    let message = String::from_utf8_lossy(&waited.stderr);
    assert!(!message.contains("no daemon is running"), "{message}");
    assert!(message.contains("connecting to"), "{message}");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "retried: {:?}",
        started.elapsed()
    );
    Ok(())
}

/// While nothing can happen before the next firing, the waiter sleeps
/// toward it (up to 30s at a time) instead of polling every second.
#[test]
fn a_far_off_firing_is_not_polled_every_second() -> Result<()> {
    const QUIET: &str = r#"{"result":"job_run","job":1,"status":"active","more":true,"run":null,"last_id":0,"quiet_until":"2100-01-01T00:00:00Z","steps":null}"#;
    let home = Home::new()?;
    let listener = std::os::unix::net::UnixListener::bind(home.path().join("cued.sock"))?;
    let polls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = std::sync::Arc::clone(&polls);
    fake_daemon(listener, move |stream, _| {
        counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        send(stream, &format!("{QUIET}\n"));
    });
    let waited = home.cued(&["wait", "j1", "--timeout", "3s"])?;
    assert_eq!(waited.status.code(), Some(124), "{}", stdout(&waited));
    // The latest-run poll, then the next-run poll; then one long sleep.
    let polls = polls.load(std::sync::atomic::Ordering::SeqCst);
    assert!(polls <= 2, "polled {polls} times in 3s");
    Ok(())
}

/// Pausing lets an executing step finish, so a wait on it still reports
/// the outcome. Once the run would need a new step to start, it's refused.
#[test]
fn a_paused_job_is_waited_out_only_while_a_step_executes() -> Result<()> {
    let home = Home::new()?;
    home.cued(&["chain", "sleep 2"])?;
    std::thread::sleep(Duration::from_millis(500));
    home.cued(&["pause", "j1"])?;
    let finished = home.cued(&["wait", "j1", "--timeout", "10s"])?;
    assert_eq!(finished.status.code(), Some(0), "{}", stdout(&finished));

    home.cued(&["chain", "sleep 2", "--then", "true"])?;
    std::thread::sleep(Duration::from_millis(500));
    home.cued(&["pause", "j2"])?;
    let stalled = home.cued(&["wait", "j2", "--timeout", "10s"])?;
    assert_eq!(stalled.status.code(), Some(1), "{}", stdout(&stalled));
    let message = String::from_utf8_lossy(&stalled.stderr);
    assert!(message.contains("cued resume j2"), "{message}");
    Ok(())
}

/// A reply cut off mid-line goes to the decoder, which says what that
/// usually means, instead of being reported as no reply at all.
#[test]
fn a_cut_off_reply_is_decoded_not_dropped() -> Result<()> {
    let home = Home::new()?;
    let listener = std::os::unix::net::UnixListener::bind(home.path().join("cued.sock"))?;
    fake_daemon(listener, |stream, _| {
        send(stream, &DONE[..DONE.len() / 2]);
        let _ = stream.shutdown(std::net::Shutdown::Both);
    });
    let waited = home.cued(&["wait", "j1"])?;
    assert_eq!(waited.status.code(), Some(1));
    let message = String::from_utf8_lossy(&waited.stderr);
    assert!(message.contains("older"), "{message}");
    Ok(())
}

/// A `CUED_MCP_CONFIG` the daemon can't find is refused, not read as "no
/// file, defaults": that would quietly open what the file meant to close.
#[test]
fn a_missing_named_policy_refuses_wait() -> Result<()> {
    let home = Home::new()?;
    home.cued(&["chain", "true", "--wait"])?;
    restart_daemon(&home, &[("CUED_MCP_CONFIG", "mcp-strict.toml")])?;
    let refused = home.cued(&["wait", "j1"])?;
    assert_eq!(refused.status.code(), Some(1), "{}", stdout(&refused));
    let message = String::from_utf8_lossy(&refused.stderr);
    assert!(message.contains("isn't a file"), "{message}");
    assert!(message.contains("relative"), "{message}");
    Ok(())
}

/// A notification step has no exit code because it runs no process; JSON
/// says so, so an agent doesn't read it as killed.
#[test]
fn json_marks_notification_steps() -> Result<()> {
    const NOTIFIED: &str = r#"{"result":"job_run","job":1,"status":"done","more":false,"run":{"id":1,"status":"done","ended_at":null,"fail_reason":null},"last_id":1,"steps":{"attempts":[{"step":"build","attempt":1,"started_at":"2026-01-01T00:00:00Z","ended_at":"2026-01-01T00:00:01Z","running":false,"exit_code":0,"timed_out":false},{"step":"tell","attempt":1,"started_at":"2026-01-01T00:00:01Z","ended_at":"2026-01-01T00:00:01Z","running":false,"exit_code":null,"timed_out":false}],"notify":["tell"]}}"#;
    let home = Home::new()?;
    let listener = std::os::unix::net::UnixListener::bind(home.path().join("cued.sock"))?;
    fake_daemon(listener, |stream, _| send(stream, &format!("{NOTIFIED}\n")));
    let waited = home.cued(&["wait", "j1", "--json"])?;
    assert_eq!(waited.status.code(), Some(0), "{}", stdout(&waited));
    let value: serde_json::Value = serde_json::from_str(stdout(&waited).trim())?;
    assert_eq!(value["attempts"][0]["notify"], false);
    assert_eq!(value["attempts"][1]["notify"], true);
    Ok(())
}

/// `--json` prints an object on every exit, a refusal included, so a
/// script piping it to `jq` never gets empty input.
#[test]
fn json_errors_are_json() -> Result<()> {
    let home = Home::new()?;
    // No daemon at all: a cued error, exit 1.
    let waited = home.cued(&["wait", "j1", "--json", "--timeout", "1s"])?;
    assert_eq!(waited.status.code(), Some(1));
    let value: serde_json::Value = serde_json::from_str(stdout(&waited).trim())?;
    assert_eq!(value["status"], "error");
    assert_eq!(value["exit"], 1);
    assert!(
        value["error"]
            .as_str()
            .is_some_and(|error| error.contains("no daemon"))
    );
    Ok(())
}

/// A cued flag typed after `at`'s or `every`'s command would silently
/// become part of the command. Without `--` that's refused (usage, 2)
/// before anything is submitted; after `--` it's the command's own
/// argument, so it goes through with a note.
#[test]
fn flags_after_the_command_are_caught() -> Result<()> {
    let home = Home::new()?;
    for (args, before) in [
        (&["at", "in 1h", "./backup.sh", "--wait"][..], "time"),
        (&["at", "in 1h", "make test", "--name", "nightly"], "time"),
        (&["at", "in 1h", "./x", "--zone=UTC"], "time"),
        (&["every", "1h", "./sync.sh", "--name", "sync"], "schedule"),
    ] {
        let refused = home.cued(args)?;
        assert_eq!(refused.status.code(), Some(2), "{args:?}");
        let message = String::from_utf8_lossy(&refused.stderr);
        assert!(
            message.contains(&format!("flags go before the {before}")),
            "{args:?}: {message}"
        );
    }
    let list = home.cued(&["list", "--all", "--json"])?;
    assert!(!stdout(&list).contains("\"j1\""), "{}", stdout(&list));

    let passed = home.cued(&["at", "in 1h", "--", "./x", "--name", "arg"])?;
    assert_eq!(passed.status.code(), Some(0));
    let message = String::from_utf8_lossy(&passed.stderr);
    assert!(message.contains("goes to the command"), "{message}");
    assert!(stdout(&passed).contains("j1"), "{}", stdout(&passed));
    Ok(())
}
