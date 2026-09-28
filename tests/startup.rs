//! Daemon startup permutations against the real binary (§5.2): the
//! single-instance guards, the socket they protect, and auto-spawn races.
//! Every process runs with a cleared environment rooted in a temp dir, so
//! nothing here can reach the user's daemon, socket, or session bus.

use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_cued");

/// One isolated user: private HOME/XDG dirs, a private socket dir, and a
/// token in every child's environment so cleanup can find auto-spawned
/// daemons that aren't our children.
struct Sandbox {
    root: tempfile::TempDir,
    token: String,
}

impl Sandbox {
    fn new() -> Self {
        let root = tempfile::Builder::new()
            .prefix("cued-startup-")
            .tempdir()
            .unwrap();
        for dir in ["home", "config", "sock", "fakebin"] {
            let dir = root.path().join(dir);
            std::fs::create_dir_all(&dir).unwrap();
            // paths.rs refuses a socket directory that isn't private.
            std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))
                .unwrap();
        }
        // `any_installed` consults crontab during auto-spawn; keep the host's
        // crontab out of it.
        let crontab = root.path().join("fakebin/crontab");
        std::fs::write(&crontab, "#!/bin/sh\necho 'no crontab for test' >&2\nexit 1\n").unwrap();
        std::fs::set_permissions(&crontab, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
        let token = format!(
            "startup-{}-{}",
            std::process::id(),
            root.path().file_name().unwrap().to_string_lossy()
        );
        Self { root, token }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.path().join(rel)
    }

    fn socket(&self) -> PathBuf {
        self.path("sock/cued.sock")
    }

    /// A command for the user whose store lives in `data`.
    fn cued(&self, data: &str) -> Command {
        let mut command = Command::new(BIN);
        command
            .env_clear()
            .env("HOME", self.path("home"))
            .env("PATH", format!("{}:/usr/bin:/bin", self.path("fakebin").display()))
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("XDG_DATA_HOME", self.path(data))
            .env("CUED_SOCKET_DIR", self.path("sock"))
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}", self.path("no-bus").display()),
            )
            .env("TZ", "UTC")
            .env("CUED_TEST_TOKEN", &self.token)
            .stdin(Stdio::null());
        command
    }

    fn foreground_daemon(&self, data: &str) -> Child {
        self.cued(data)
            .args(["daemon", "--foreground"])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn run(&self, data: &str, args: &[&str]) -> Output {
        self.cued(data).args(args).output().unwrap()
    }

    /// Every live process carrying this sandbox's token.
    fn owned_pids(&self) -> Vec<i32> {
        let needle = format!("CUED_TEST_TOKEN={}", self.token);
        let mut pids = Vec::new();
        for entry in std::fs::read_dir("/proc").unwrap().flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
                continue;
            };
            let Ok(environ) = std::fs::read(entry.path().join("environ")) else {
                continue;
            };
            if environ.split(|b| *b == 0).any(|var| var == needle.as_bytes()) {
                pids.push(pid);
            }
        }
        pids
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        for pid in self.owned_pids() {
            // SAFETY: signalling a process we identified by our own token.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

fn wait_for_socket(socket: &Path, daemon: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Some(status) = daemon.try_wait().unwrap() {
            panic!("daemon exited before binding: {status}");
        }
        if UnixStream::connect(socket).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("socket {} never accepted a connection", socket.display());
}

fn wait_exit(child: &mut Child, within: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

fn stop(mut child: Child) {
    // SAFETY: our own child.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    if wait_exit(&mut child, Duration::from_secs(10)).is_none() {
        child.kill().unwrap();
        child.wait().unwrap();
    }
}

/// The reproduced defect: the DB flock was the only guard, but the socket
/// path is independent of XDG_DATA_HOME. A second daemon for another data
/// directory held a *different* lock, treated the live socket as stale,
/// unlinked it and bound over it — the first daemon kept running its jobs
/// with no way for any client to reach, list, or cancel them.
#[test]
fn a_daemon_for_another_data_dir_cannot_take_over_a_live_socket() {
    let sandbox = Sandbox::new();
    let mut first = sandbox.foreground_daemon("data-a");
    wait_for_socket(&sandbox.socket(), &mut first);
    let inode = std::fs::metadata(sandbox.socket()).unwrap().ino();

    let submitted = sandbox.run("data-a", &["at", "in 1h", "--", "/bin/true"]);
    assert!(submitted.status.success(), "{submitted:?}");

    let mut second = sandbox.foreground_daemon("data-b");
    let status = wait_exit(&mut second, Duration::from_secs(10))
        .expect("the second daemon must refuse the socket, not keep running");
    assert!(!status.success(), "losing the socket lock is a startup failure");
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut second.stderr.take().unwrap(), &mut stderr).unwrap();
    assert!(stderr.contains("another cued daemon is serving"), "{stderr}");

    assert_eq!(
        std::fs::metadata(sandbox.socket()).unwrap().ino(),
        inode,
        "the live socket was replaced"
    );
    assert!(first.try_wait().unwrap().is_none(), "the first daemon died");
    let listed = sandbox.run("data-a", &["list", "--json"]);
    assert!(listed.status.success(), "{listed:?}");
    let jobs: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(jobs.as_array().map(Vec::len), Some(1), "{jobs}");
    stop(first);
}

/// The socket lock must not outlive its daemon: flock releases on death, so
/// a SIGKILLed daemon's leftover socket file is still just stale.
#[test]
fn a_killed_daemons_socket_and_locks_do_not_block_the_next_one() {
    let sandbox = Sandbox::new();
    let mut first = sandbox.foreground_daemon("data");
    wait_for_socket(&sandbox.socket(), &mut first);
    first.kill().unwrap();
    first.wait().unwrap();
    assert!(sandbox.socket().exists(), "SIGKILL leaves the socket file behind");
    assert!(UnixStream::connect(sandbox.socket()).is_err());

    let mut next = sandbox.foreground_daemon("data");
    wait_for_socket(&sandbox.socket(), &mut next);
    stop(next);
}

/// §5.2 auto-spawn from cold, many clients at once: every client gets an
/// answer and exactly one daemon survives the flock race.
#[test]
fn concurrent_cold_clients_share_one_auto_spawned_daemon() {
    let sandbox = Sandbox::new();
    let clients: Vec<Child> = (0..12)
        .map(|_| {
            sandbox
                .cued("data")
                .args(["list", "--json"])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    for client in clients {
        let output = client.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "[]");
    }

    // Losing daemons exit on the flock; give them a moment, then count.
    let deadline = Instant::now() + Duration::from_secs(5);
    let daemons = loop {
        let daemons: Vec<i32> = sandbox
            .owned_pids()
            .into_iter()
            .filter(|pid| {
                std::fs::read(format!("/proc/{pid}/cmdline"))
                    .is_ok_and(|cmd| cmd.split(|b| *b == 0).any(|arg| arg == b"daemon"))
            })
            .collect();
        if daemons.len() <= 1 || Instant::now() > deadline {
            break daemons;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(daemons.len(), 1, "surviving daemons: {daemons:?}");
}
