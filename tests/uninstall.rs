//! `cued uninstall` (DESIGN.md §8.2): the backend, the daemon and its
//! running steps, the store, logs and socket all go; the user's config
//! stays unless purged; and nothing is deleted without a confirmation.
//!
//! `crontab`, `systemctl` and `loginctl` are fakes on PATH backed by files
//! in the test's HOME, so these tests can install and remove backends
//! without touching the host's real crontab or user units.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

const FAKE_CRONTAB: &str = r#"#!/bin/sh
f="$HOME/fake-crontab"
case "$1" in
  -l) [ -f "$f" ] && { cat "$f"; exit 0; }; echo "no crontab for test" >&2; exit 1 ;;
  -r) [ -f "$f" ] || { echo "no crontab for test" >&2; exit 1; }; rm "$f" ;;
  -) cat > "$f" ;;
esac
"#;

/// Logs every call; `disable --now` also stops this HOME's daemon and
/// waits for it to exit, as the real `systemctl` does for a running unit.
const FAKE_SYSTEMCTL: &str = r#"#!/bin/sh
echo "$*" >> "$HOME/systemctl.log"
case "$*" in
  *"disable --now"*)
    for p in $(pgrep -u "$(id -u)" -x cued); do
      if tr '\0' '\n' < "/proc/$p/environ" 2>/dev/null | grep -qx "HOME=$HOME" \
         && tr '\0' '\n' < "/proc/$p/cmdline" 2>/dev/null | grep -qx daemon; then
        kill "$p"
        while kill -0 "$p" 2>/dev/null; do sleep 0.1; done
      fi
    done ;;
esac
"#;

const FAKE_LOGINCTL: &str = r#"#!/bin/sh
echo "Linger=no"
"#;

struct Home {
    dir: tempfile::TempDir,
    fakes: PathBuf,
}

impl Home {
    fn new() -> Result<Self> {
        let dir = tempfile::tempdir()?;
        // paths.rs refuses a socket directory that isn't 0700.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))?;
        let fakes = dir.path().join("fakes");
        std::fs::create_dir(&fakes)?;
        for (name, script) in [
            ("crontab", FAKE_CRONTAB),
            ("systemctl", FAKE_SYSTEMCTL),
            ("loginctl", FAKE_LOGINCTL),
        ] {
            let path = fakes.join(name);
            std::fs::write(&path, script)?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
        }
        Ok(Self { dir, fakes })
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn data_dir(&self) -> PathBuf {
        self.path().join(".local/share/cued")
    }

    fn config_dir(&self) -> PathBuf {
        self.path().join(".config/cued")
    }

    fn unit(&self) -> PathBuf {
        self.path().join(".config/systemd/user/cued.service")
    }

    fn crontab(&self) -> String {
        std::fs::read_to_string(self.path().join("fake-crontab")).unwrap_or_default()
    }

    fn cued(&self, args: &[&str]) -> Result<Output> {
        let home = self.path();
        // Fakes are freshly written: another test thread forking while one
        // was open for writing makes exec fail with ETXTBSY for a moment.
        for _ in 0..50 {
            match Command::new(env!("CARGO_BIN_EXE_cued"))
                .args(args)
                .env("HOME", home)
                .env("XDG_DATA_HOME", home.join(".local/share"))
                .env("XDG_CONFIG_HOME", home.join(".config"))
                .env("CUED_SOCKET_DIR", home)
                .env("PATH", format!("{}:/usr/bin:/bin", self.fakes.display()))
                .env(
                    "DBUS_SESSION_BUS_ADDRESS",
                    format!("unix:path={}", home.join("no-bus").display()),
                )
                .stdin(Stdio::null())
                .output()
            {
                Ok(output) if text(&output).contains("Text file busy") => {}
                Err(error) if error.raw_os_error() == Some(libc::ETXTBSY) => {}
                result => return Ok(result?),
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        bail!("cued stayed busy")
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

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn ok(output: Output) -> Result<String> {
    if !output.status.success() {
        bail!("cued failed:\n{}", text(&output));
    }
    Ok(text(&output))
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

#[test]
fn removes_backend_daemon_and_store_but_keeps_config_and_other_cron_lines() -> Result<()> {
    let home = Home::new()?;
    std::fs::write(
        home.path().join("fake-crontab"),
        "0 9 * * 1 /usr/bin/weekly\n",
    )?;
    std::fs::create_dir_all(home.config_dir())?;
    std::fs::write(home.config_dir().join("mcp.toml"), "exec = 'approve'\n")?;
    ok(home.cued(&["setup", "--backend", "cron"])?)?;
    assert!(home.crontab().contains("@reboot"), "{}", home.crontab());

    // A running step, with a grandchild that would leave a marker if the
    // step's group outlived the uninstall.
    let marker = home.path().join("survived");
    let script = format!("(sleep 3; touch {}) & sleep 300", marker.display());
    ok(home.cued(&["at", "in 1s", "--", "/bin/sh", "-c", &script])?)?;
    let pid = home.daemon_pid().expect("the auto-spawned daemon");
    wait_until(
        || {
            home.cued(&["list"])
                .is_ok_and(|output| text(&output).contains("running"))
        },
        Duration::from_secs(20),
        "the step to start",
    )?;

    let output = ok(home.cued(&["uninstall", "--yes"])?)?;
    assert!(output.contains("terminating 1 running step"), "{output}");
    assert!(output.contains("cargo uninstall cued"), "{output}");

    assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "the daemon survived");
    assert!(!home.data_dir().exists(), "the store survived");
    assert!(
        !home.path().join("cued.sock").exists(),
        "the socket survived"
    );
    assert!(
        !home.path().join("cued.sock.lock").exists(),
        "the socket lock survived"
    );
    assert!(
        home.config_dir().join("mcp.toml").exists(),
        "config was removed without --purge"
    );
    assert_eq!(
        home.crontab(),
        "0 9 * * 1 /usr/bin/weekly\n",
        "the crontab wasn't restored"
    );

    std::thread::sleep(Duration::from_secs(4));
    assert!(
        !marker.exists(),
        "a step's process group outlived the uninstall"
    );
    Ok(())
}

#[test]
fn purge_removes_config_and_systemd_is_disabled_through_systemctl() -> Result<()> {
    let home = Home::new()?;
    std::fs::create_dir_all(home.config_dir())?;
    std::fs::write(home.config_dir().join("config.toml"), "")?;
    ok(home.cued(&["setup", "--backend", "systemd"])?)?;
    assert!(home.unit().exists());
    // The fake systemctl starts nothing; give it a daemon to stop.
    ok(home.cued(&["list"])?)?;
    let pid = home.daemon_pid().expect("the auto-spawned daemon");

    // The daemon the plan names is stopped by the backend removal, before
    // uninstall gets to it: there must be nothing left for it to signal
    // (that pid may by now be another process's), and nothing to wait on.
    let output = ok(home.cued(&["uninstall", "--yes", "--purge"])?)?;
    assert!(
        output.contains(&format!("the daemon (pid {pid})")),
        "{output}"
    );
    let kept = output.split("Kept:").nth(1).unwrap_or_default();
    assert!(
        !kept.contains("your config"),
        "--purge kept the config:\n{output}"
    );
    assert_eq!(output.matches("Removing systemd").count(), 1, "{output}");

    let systemctl = std::fs::read_to_string(home.path().join("systemctl.log"))?;
    assert!(
        systemctl.contains("--user disable --now cued.service"),
        "{systemctl}"
    );
    assert!(!home.unit().exists(), "the unit survived");
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "the daemon survived");
    assert!(!home.data_dir().exists(), "the store survived");
    assert!(!home.config_dir().exists(), "--purge left the config");
    Ok(())
}

#[test]
fn nothing_is_deleted_without_confirmation() -> Result<()> {
    let home = Home::new()?;
    ok(home.cued(&["at", "in 1h", "--", "/bin/true"])?)?;
    let pid = home.daemon_pid().expect("the auto-spawned daemon");

    // stdin is not a terminal, so there is no one to ask.
    let refused = home.cued(&["uninstall"])?;
    assert!(!refused.status.success(), "{}", text(&refused));
    assert!(text(&refused).contains("--yes"), "{}", text(&refused));
    // The plan was still shown, so the user knows what --yes would do.
    assert!(
        text(&refused).contains("1 job, 1 still scheduled"),
        "{}",
        text(&refused)
    );

    assert_eq!(
        home.daemon_pid(),
        Some(pid),
        "the daemon was stopped anyway"
    );
    assert!(
        home.data_dir().join("cued.db").exists(),
        "the store was deleted anyway"
    );
    Ok(())
}

#[test]
fn uninstalling_twice_is_harmless_and_never_starts_a_daemon() -> Result<()> {
    let home = Home::new()?;
    ok(home.cued(&["list"])?)?;
    ok(home.cued(&["uninstall", "--yes"])?)?;
    ok(home.cued(&["uninstall", "--yes"])?)?;
    assert_eq!(home.daemon_pid(), None, "uninstall auto-spawned a daemon");
    assert!(!home.path().join("cued.sock").exists());
    Ok(())
}
