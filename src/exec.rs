//! The `Spawner` trait (DESIGN.md §11) and real process execution (§2.2, §2.3).
//!
//! Execution rules (§2.2): every step child gets its own session/process
//! group (`setsid`); timeout, cancel, deadline, and clean shutdown all
//! terminate the *group* with SIGTERM → `kill_grace` → SIGKILL; stdin is
//! /dev/null. Capture is two pipes (§2.3): both interleave into the single
//! per-attempt log file, while each stream keeps its own capped tail for
//! Stdout/Stderr condition matching.
//!
//! Kill triggers (§2.3): a step's own `timeout`, and the cancellation handle
//! the daemon's live-execution registry holds. Both run the identical §2.2
//! sequence — the handle exists so `cued cancel` (and later `deadline`
//! expiry and clean shutdown) reach a live process group without any of them
//! knowing how a kill works.
//!
//! TODO(§2.3): the suspend backstop — a tick sweep over the registry's
//! absolute kill-at instants, for when `CLOCK_MONOTONIC` stalls across a
//! suspend — lands with `deadline`.

use std::collections::BTreeMap;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use jiff::SignedDuration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};

/// §3.2: how much of each stream is retained for condition matching.
/// Provisional number (§12).
const OUTPUT_TAIL_CAP: usize = 256 * 1024;

/// How long we'll wait for pipe EOF after the child is reaped. Normally
/// instant; only a grandchild that escaped the process group (double-fork +
/// setsid) can hold the pipes open longer, and §2.2 already declares escaped
/// processes out of scope.
const DRAIN_GRACE: Duration = Duration::from_secs(5);

/// §2.3: the handle a live attempt is reachable by. The daemon's registry
/// holds one per running step; triggering it runs the one §2.2 TERM →
/// `kill_grace` → KILL sequence against that step's process group.
///
/// A watch channel rather than a oneshot because the signal must latch: a
/// cancel that arrives in the window between the claim and the spawn has to
/// still be visible when the spawned task first looks.
#[derive(Debug, Clone)]
pub struct Cancel(tokio::sync::watch::Sender<bool>);

impl Default for Cancel {
    fn default() -> Self {
        Self::new()
    }
}

impl Cancel {
    pub fn new() -> Self {
        Self(tokio::sync::watch::channel(false).0)
    }

    /// Ask the attempt to die. Idempotent; safe once the attempt is gone.
    pub fn signal(&self) {
        // `send` fails (and does not update the stored value) while there
        // are no receivers. Keep this a latch even if a signal races handle
        // creation, so a later subscriber still observes cancellation.
        self.0.send_replace(true);
    }

    /// The half that rides along in an [`ExecRequest`].
    pub fn signal_handle(&self) -> CancelSignal {
        CancelSignal(self.0.subscribe())
    }
}

/// The receiving half of a [`Cancel`].
#[derive(Debug, Clone)]
pub struct CancelSignal(tokio::sync::watch::Receiver<bool>);

impl Default for CancelSignal {
    /// A request nobody can cancel — what the tests and any non-registry
    /// caller want.
    fn default() -> Self {
        Cancel::new().signal_handle()
    }
}

impl CancelSignal {
    /// True if cancellation was already asked for — checked before the spawn
    /// so a cancel that lands during the claim doesn't start a process only
    /// to kill it.
    pub fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }

    /// Resolves when cancellation is requested, and *never* if the handle is
    /// simply dropped: the registry drops it when a step ends normally, and
    /// that must not read as "kill this".
    pub async fn cancelled(&mut self) {
        if self.0.wait_for(|cancelled| *cancelled).await.is_err() {
            std::future::pending::<()>().await
        }
    }
}

/// Everything needed to run one step attempt.
#[derive(Debug, Clone)]
pub struct ExecRequest {
    /// Canonical argv (§2.2) — sh -c desugaring happened at submit.
    pub argv: Vec<String>,
    pub cwd: String,
    pub env: BTreeMap<String, String>,
    pub timeout: Option<SignedDuration>,
    pub kill_grace: SignedDuration,
    /// Per-attempt log file (§2.1): logs/<job>/<run>/<step>.<attempt>.log
    pub log_file: PathBuf,
    /// §2.3: how `cued cancel` reaches this attempt's process group.
    pub cancel: CancelSignal,
}

/// What happened, in exactly the terms conditions are evaluated over (§3.2).
#[derive(Debug, Clone)]
pub struct ExecResult {
    /// None = killed by a signal (treated as failure, §2.2).
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// Tail of output retained for Stdout/Stderr matching, capped (§3.2).
    pub stdout: String,
    pub stderr: String,
    /// The attempt was killed by its cancellation handle (§2.3), not by its
    /// own timeout and not by finishing.
    pub cancelled: bool,
}

/// Process execution behind a trait so tests script step outcomes without
/// real processes (§11).
#[allow(async_fn_in_trait)] // internal seam; no dyn use planned
pub trait Spawner: Send + Sync {
    async fn run(&self, request: ExecRequest) -> Result<ExecResult>;
}

/// The real spawner: tokio::process + process groups.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemSpawner;

impl Spawner for SystemSpawner {
    async fn run(&self, mut request: ExecRequest) -> Result<ExecResult> {
        ensure!(!request.argv.is_empty(), "empty argv"); // §6.3 should have caught it

        // §2.3: a cancel that landed between the claim and here means don't
        // start at all — cheaper and cleaner than spawning to immediately
        // kill, and it keeps `cued cancel` from producing a process the user
        // never saw running.
        if request.cancel.is_cancelled() {
            return Ok(ExecResult {
                exit_code: None,
                timed_out: false,
                stdout: String::new(),
                stderr: String::new(),
                cancelled: true,
            });
        }

        // The log file exists before the process does, so `cued logs` has
        // something to tail the moment the attempt starts. 0600 (§7.5).
        if let Some(dir) = request.log_file.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating log dir {}", dir.display()))?;
        }
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&request.log_file)
            .with_context(|| format!("opening log file {}", request.log_file.display()))?;
        let log = Arc::new(tokio::sync::Mutex::new(tokio::fs::File::from_std(log)));

        let mut command = Command::new(&request.argv[0]);
        command
            .args(&request.argv[1..])
            .current_dir(&request.cwd)
            .env_clear()
            .envs(&request.env)
            // §2.2: stdin is /dev/null — steps are non-interactive.
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // §2.2/§2.3: a full session of its own, so the TERM→KILL sequence
        // reaches everything the step spawned, not just the direct child.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let mut child = command
            .spawn()
            .with_context(|| format!("spawning {:?} in {:?}", request.argv[0], request.cwd))?;
        // After setsid, the child's pid IS the process-group (and session) id.
        let pgid = child.id().context("spawned child has no pid")? as i32;

        let stdout_tail = Arc::new(Mutex::new(Vec::new()));
        let stderr_tail = Arc::new(Mutex::new(Vec::new()));
        let stdout_pipe = child.stdout.take().expect("stdout was piped");
        let stderr_pipe = child.stderr.take().expect("stderr was piped");
        let stdout_task = tokio::spawn(drain(
            stdout_pipe,
            Arc::clone(&log),
            Arc::clone(&stdout_tail),
        ));
        let stderr_task = tokio::spawn(drain(
            stderr_pipe,
            Arc::clone(&log),
            Arc::clone(&stderr_tail),
        ));

        // Wait, racing the two kill triggers (§2.3): the step's own timeout
        // and the cancellation handle. Both end in the same §2.2 sequence —
        // the only difference is what we record about why.
        let grace = to_std(request.kill_grace).unwrap_or_default();
        let timeout = async {
            match request.timeout.and_then(to_std) {
                Some(timeout) => tokio::time::sleep(timeout).await,
                // No timeout is "this trigger never fires", not "fire now".
                None => std::future::pending().await,
            }
        };
        tokio::pin!(timeout);

        let mut timed_out = false;
        let mut cancelled = false;
        let status = tokio::select! {
            status = child.wait() => status?,
            _ = &mut timeout => {
                timed_out = true;
                kill_group(&mut child, pgid, grace).await?
            }
            _ = request.cancel.cancelled() => {
                cancelled = true;
                kill_group(&mut child, pgid, grace).await?
            }
        };

        // Pipes hit EOF once every group member holding them is gone —
        // normally immediate. Bound the wait so an escaped grandchild that
        // inherited them can't wedge the daemon.
        for mut task in [stdout_task, stderr_task] {
            if tokio::time::timeout(DRAIN_GRACE, &mut task).await.is_err() {
                // Dropping a JoinHandle detaches the task; it does not stop
                // it. Abort explicitly, or it keeps appending the escapee's
                // output to this finished attempt's log for as long as the
                // escapee lives. The tails hold whatever arrived.
                task.abort();
            }
        }

        let stdout = take_lossy(&stdout_tail);
        let stderr = take_lossy(&stderr_tail);
        Ok(ExecResult {
            // §2.2: killed by a signal → no exit code → treated as Failed.
            exit_code: status.code(),
            timed_out,
            stdout,
            stderr,
            cancelled,
        })
    }
}

/// How often, once the leader is gone, we look for group members that are
/// still around. Only affects how quickly a cooperative teardown returns.
const GROUP_POLL: Duration = Duration::from_millis(25);

/// §2.2: SIGTERM (then SIGCONT) to the group → grace → SIGKILL to the
/// group, then reap.
///
/// The grace is the *group's*, not the leader's: a leader that exits on TERM
/// while a child that ignores it lives on must not end the sequence, or the
/// child outlives the step — exactly the leak §2.2 signals the group to
/// prevent. So after the leader is reaped we keep watching the group until
/// it empties or the grace runs out, and KILL whatever is left.
async fn kill_group(
    child: &mut Child,
    pgid: i32,
    grace: Duration,
) -> std::io::Result<std::process::ExitStatus> {
    signal_group(pgid, libc::SIGTERM);
    // A stopped member can't act on the TERM until it runs again, so the
    // grace would pass with the signal pending and KILL would be the first
    // thing it saw. Continue the group so the grace means something.
    signal_group(pgid, libc::SIGCONT);
    let deadline = tokio::time::Instant::now() + grace;

    let status = tokio::select! {
        status = child.wait() => status?,
        _ = tokio::time::sleep_until(deadline) => {
            signal_group(pgid, libc::SIGKILL);
            return child.wait().await;
        }
    };

    // Orphaned members are reparented and reaped outside the daemon, so an
    // empty group reads as ESRCH promptly. Stopping at the first ESRCH also
    // keeps us from signalling the id after it could be reused.
    while group_has_members(pgid) {
        if tokio::time::Instant::now() >= deadline {
            signal_group(pgid, libc::SIGKILL);
            break;
        }
        tokio::time::sleep(GROUP_POLL.min(deadline - tokio::time::Instant::now())).await;
    }
    Ok(status)
}

fn signal_group(pgid: i32, signal: i32) {
    // ESRCH (group already gone) is fine — the race with a clean exit is
    // expected, and child.wait() still reaps the real status.
    unsafe {
        libc::killpg(pgid, signal);
    }
}

/// Signal 0 delivers nothing; it only asks whether the group has anyone left.
/// EPERM would mean a member we can't signal — still a member.
fn group_has_members(pgid: i32) -> bool {
    let alive = unsafe { libc::killpg(pgid, 0) } == 0;
    alive || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Pump one pipe: every chunk is appended to the shared log file (the §2.1
/// merged, best-effort-chronological view) and to this stream's own capped
/// tail (§2.3's match buffer). Flushed per chunk so `logs -f` is live.
async fn drain(
    mut pipe: impl tokio::io::AsyncRead + Unpin,
    log: Arc<tokio::sync::Mutex<tokio::fs::File>>,
    tail: Arc<Mutex<Vec<u8>>>,
) {
    let mut buf = [0u8; 8192];
    loop {
        let read = match pipe.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        {
            let mut file = log.lock().await;
            let _ = file.write_all(&buf[..read]).await;
            let _ = file.flush().await;
        }
        let mut tail = tail.lock().expect("tail lock");
        tail.extend_from_slice(&buf[..read]);
        if tail.len() > OUTPUT_TAIL_CAP {
            let excess = tail.len() - OUTPUT_TAIL_CAP;
            tail.drain(..excess);
        }
    }
}

fn take_lossy(tail: &Arc<Mutex<Vec<u8>>>) -> String {
    let bytes = std::mem::take(&mut *tail.lock().expect("tail lock"));
    String::from_utf8_lossy(&bytes).into_owned()
}

fn to_std(duration: SignedDuration) -> Option<Duration> {
    Duration::try_from(duration).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(argv: &[&str], log_file: PathBuf) -> ExecRequest {
        ExecRequest {
            argv: argv.iter().map(|s| s.to_string()).collect(),
            cwd: "/".into(),
            env: BTreeMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]),
            timeout: None,
            kill_grace: SignedDuration::from_millis(200),
            log_file,
            cancel: CancelSignal::default(),
        }
    }

    #[tokio::test]
    async fn cancellation_is_latched_before_subscribing() {
        let cancel = Cancel::new();
        cancel.signal();

        let mut handle = cancel.signal_handle();
        assert!(handle.is_cancelled());
        tokio::time::timeout(Duration::from_millis(50), handle.cancelled())
            .await
            .expect("a late subscriber must observe the earlier signal");
    }

    #[tokio::test]
    async fn cancellation_signal_is_idempotent() {
        let cancel = Cancel::new();
        let mut handle = cancel.signal_handle();
        cancel.signal();
        cancel.signal();

        assert!(handle.is_cancelled());
        tokio::time::timeout(Duration::from_millis(50), handle.cancelled())
            .await
            .expect("repeated signals must leave cancellation latched");
    }

    #[tokio::test]
    async fn captures_exit_code_and_streams() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let log = dir.path().join("step.1.log");
        let result = SystemSpawner
            .run(request(
                &["/bin/sh", "-c", "echo out; echo err >&2; exit 3"],
                log.clone(),
            ))
            .await?;

        assert_eq!(result.exit_code, Some(3));
        assert!(!result.timed_out);
        // §2.3: separate tails for matching…
        assert_eq!(result.stdout, "out\n");
        assert_eq!(result.stderr, "err\n");
        // …and one merged log file holding both.
        let merged = std::fs::read_to_string(&log)?;
        assert!(
            merged.contains("out\n") && merged.contains("err\n"),
            "{merged:?}"
        );
        // §7.5: log file is 0600.
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&log)?.permissions().mode() & 0o777, 0o600);
        Ok(())
    }

    #[tokio::test]
    async fn timeout_kills_the_whole_group() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let marker = dir.path().join("grandchild-ran");
        // The step spawns a backgrounded grandchild; if only the direct
        // child were killed, the grandchild would survive to write the
        // marker after the step "ended" (§2.2's leak).
        let script = format!("(sleep 2; touch {}) & sleep 10", marker.display());
        let mut req = request(&["/bin/sh", "-c", &script], dir.path().join("t.log"));
        req.timeout = Some(SignedDuration::from_millis(300));

        let started = std::time::Instant::now();
        let result = SystemSpawner.run(req).await?;

        assert!(result.timed_out);
        assert_eq!(result.exit_code, None, "killed by signal → no exit code");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "TERM→grace→KILL, not the full sleep"
        );
        tokio::time::sleep(Duration::from_millis(2200)).await;
        assert!(!marker.exists(), "grandchild escaped the group kill");
        Ok(())
    }

    #[tokio::test]
    async fn a_term_ignoring_grandchild_is_killed_after_the_leader_exits() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let marker = dir.path().join("grandchild-ran");
        // The leader dies on TERM at once; the grandchild ignores it. The
        // leader's exit must not end the sequence — the group's grace does,
        // and the KILL after it reaches the grandchild (§2.2).
        let script = format!(
            "(trap '' TERM; sleep 1; touch {}) & sleep 10",
            marker.display()
        );
        let mut req = request(&["/bin/sh", "-c", &script], dir.path().join("t.log"));
        req.timeout = Some(SignedDuration::from_millis(300));

        let started = std::time::Instant::now();
        let result = SystemSpawner.run(req).await?;

        assert!(result.timed_out);
        // Grace (200ms) after the timeout (300ms), plus the KILL and drain —
        // not DRAIN_GRACE's 5s, which is what waiting on a survivor's pipes
        // would cost.
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(
            !marker.exists(),
            "TERM-ignoring grandchild outlived the teardown"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_cooperative_group_returns_without_waiting_out_the_grace() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut req = request(
            &["/bin/sh", "-c", "sleep 10 & sleep 10"],
            dir.path().join("t.log"),
        );
        req.timeout = Some(SignedDuration::from_millis(100));
        req.kill_grace = SignedDuration::from_secs(5);

        let started = std::time::Instant::now();
        let result = SystemSpawner.run(req).await?;

        assert!(result.timed_out);
        // Every member exits on TERM, so the group empties long before the
        // 5s grace — watching the group must not turn into sleeping it out.
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        Ok(())
    }

    /// §2.2: TERM → grace → KILL gives a step its grace to clean up. A
    /// stopped process can't act on a TERM until it is continued, so without
    /// a SIGCONT the grace passes with the signal pending and the KILL is the
    /// first thing it ever sees — a stopped step never got its grace at all.
    #[tokio::test]
    async fn a_stopped_step_still_gets_its_grace() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut req = request(
            &[
                "/bin/sh",
                "-c",
                "trap 'exit 7' TERM; kill -STOP $$; sleep 10",
            ],
            dir.path().join("t.log"),
        );
        req.timeout = Some(SignedDuration::from_millis(300));
        req.kill_grace = SignedDuration::from_secs(3);

        let started = std::time::Instant::now();
        let result = SystemSpawner.run(req).await?;

        assert!(result.timed_out);
        assert_eq!(result.exit_code, Some(7), "the TERM handler never ran");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        Ok(())
    }

    /// A grandchild that escaped the group (its own `setsid`) and kept the
    /// pipes is out of §2.2's reach — but once `DRAIN_GRACE` gives up on it,
    /// the attempt is over. Its drain tasks must end with it, not keep
    /// feeding a finished attempt's log for as long as the escapee lives.
    #[tokio::test]
    async fn an_abandoned_drain_stops_writing_the_finished_attempts_log() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let pid_file = dir.path().join("escapee");
        let log = dir.path().join("t.log");
        let script = format!(
            "setsid /bin/sh -c 'echo $$ > {}; while :; do echo x; sleep 0.05; done' &",
            pid_file.display()
        );
        let result = SystemSpawner
            .run(request(&["/bin/sh", "-c", &script], log.clone()))
            .await;

        let escapee: i32 = std::fs::read_to_string(&pid_file)?.trim().parse()?;
        let before = std::fs::metadata(&log)?.len();
        tokio::time::sleep(Duration::from_millis(500)).await;
        let after = std::fs::metadata(&log)?.len();
        unsafe { libc::kill(escapee, libc::SIGKILL) };

        assert_eq!(result?.exit_code, Some(0));
        assert_eq!(before, after, "a drain task outlived its attempt");
        Ok(())
    }

    #[tokio::test]
    async fn spawn_failure_is_an_error() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let result = SystemSpawner
            .run(request(&["/nonexistent/binary"], dir.path().join("x.log")))
            .await;
        assert!(result.is_err());
        Ok(())
    }
}
