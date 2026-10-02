//! The daemon (DESIGN.md §5.2): owns the min-heap of pending
//! `(run, next_step, at)` entries, ticks against the wall clock, reconciles
//! the store on startup, and serves the control socket.
//!
//! Startup order is load-bearing:
//!   flock (§5.2)  →  open store + migrate (§5.3)  →  reconcile (§3.4, §4.2)
//!   →  bind + serve socket (§5.1, §7.3)  →  tick loop
//! The lock precedes the store because the flock is what enforces SQLite's
//! single-writer assumption; the socket comes last so clients never see a
//! daemon that isn't ready.

use crate::schedule::due_instants;

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap};
use std::fs::File;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use jiff::{SignedDuration, Timestamp};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

use crate::cli::DaemonArgs;
use crate::clock::{Clock, SystemClock};
use crate::config::{Config, Retention};
use crate::exec::{Cancel, ExecRequest, Spawner, SystemSpawner};
use crate::model::{
    Action, Condition, DeliveryReceipt, Effect, Graph, HeldReason, Job, JobId, JobSpec, MissedWait,
    NotifySpec, OnInterrupt, Outcome, OutputMatch, RunId, RunStatus, Step, StepId, Wait,
};
use crate::model::{CatchUp, Overlap};
use crate::notify::{Delivery, DesktopNotifier, Notifier};
use crate::paths::Paths;
use crate::proto::{JobEntry, PROTO_VERSION, Request, RequestBody, Response, RunEntry, RunSteps};
use crate::store::{Claim, DueStep, Fired, Firing, GcOutcome, NextCursor, StepClose, Store};
use crate::{schedule, submit};

/// The wall-clock tick cap (§2.1): the scheduler never sleeps longer than
/// this, so a suspend/clock-jump leaves a due target unnoticed for at most
/// one tick. (The §2.3 registry sweep for in-flight timeouts rides the same
/// tick once it lands.)
const MAX_TICK: Duration = Duration::from_secs(30);

/// How far past its frozen target a popped entry may be before it counts as
/// "the wait expired while we weren't looking" for `missed_wait` purposes
/// (§3.4 Case 1). Normal operation is at most MAX_TICK late; anything past
/// this grace means downtime or suspend. Provisional number (§12).
const MISSED_GRACE: SignedDuration = SignedDuration::from_secs(60);

/// How many times a due step whose run failed with an *internal* error (a
/// store write that lost a race, say — not the step's own exit code) is
/// re-armed before the run is parked for a human. Provisional number (§12).
const STEP_RETRY_BUDGET: u32 = 5;

/// The first such retry's delay; it doubles each time (250ms, 500ms, 1s, 2s,
/// 4s — under eight seconds of trying before we park). Short on purpose: the
/// failure this recovers is a lost write race, which clears in milliseconds,
/// and anything still failing after several seconds is broken rather than
/// busy. Provisional number (§12).
const STEP_RETRY_BACKOFF: SignedDuration = SignedDuration::from_millis(250);

/// What a clean shutdown allows a teardown *beyond* its own `kill_grace`:
/// the TERM→KILL race plus §2.3's pipe drain. The wait itself is derived
/// from the graces of the attempts actually running, because `kill_grace` is
/// configurable per job and per step — a fixed ceiling would abandon a
/// teardown the user had deliberately made longer. Provisional (§12).
const SHUTDOWN_MARGIN: Duration = Duration::from_secs(10);

/// §10.2: how often retention is enforced without being asked. Daily, plus
/// once at startup — a machine that's only on for an hour a day still gets
/// swept, and one that runs for months doesn't wait for a restart.
const GC_TICK: Duration = Duration::from_secs(24 * 3600);

/// The §3.5 slow tick: how often the notification backlog is retried when
/// nothing nudges it sooner (a bus appearing at login has no push signal).
/// Provisional number (§12).
const SLOW_TICK: Duration = Duration::from_secs(30);

pub fn run(args: DaemonArgs) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new().context("starting tokio runtime")?;
    runtime.block_on(run_async(args))
}

async fn run_async(args: DaemonArgs) -> Result<()> {
    let paths = Paths::resolve()?;

    // Before anything that can fail: a backgrounded daemon is spawned with
    // its stderr on /dev/null (§5.2 auto-spawn), so without this every
    // startup failure — a lost flock race, a migration error, a socket it
    // can't bind — is invisible, and the client can only report "daemon
    // unreachable". Redirecting fd 2 rather than threading a logger means
    // the whole process, panics included, lands in the file.
    if !args.foreground {
        redirect_stderr_to_log(&paths.daemon_log)?;
    }
    eprintln!("cued: daemon starting (pid {})", std::process::id());

    // The path an upgrade re-executes: the one we were started from, which
    // is also what the persistence backend names. Resolved now, while it
    // still names this image — once a new build is renamed over it,
    // /proc/self/exe reads "… (deleted)".
    let exe = std::env::current_exe()
        .ok()
        .map(|exe| {
            // A fallback after a failed upgrade exec is started through
            // /proc/self/exe, whose path then reads "… (deleted)". The path
            // without the suffix is still where the install lives, which is
            // what the next upgrade should look at.
            use std::os::unix::ffi::OsStrExt;
            match exe.as_os_str().as_bytes().strip_suffix(b" (deleted)") {
                Some(path) => std::path::PathBuf::from(std::ffi::OsStr::from_bytes(path)),
                None => exe,
            }
        })
        .and_then(|exe| exe.canonicalize().ok());

    // §5.2: the authoritative single-instance guard. Held for the daemon's
    // lifetime; a second daemon (leftover cron @reboot vs systemd) fails
    // here, before it can touch the store. An upgraded daemon never let go
    // of either lock, so it adopts them rather than racing for them again.
    let (lock, socket_lock, listener) = match args.handoff.as_deref() {
        Some(fds) => {
            let (lock, socket_lock, listener) = adopt_handoff(&paths, fds)?;
            eprintln!("cued: resumed after upgrade from inherited locks and socket");
            (lock, socket_lock, Some(listener))
        }
        None => (
            acquire_instance_lock(&paths)?,
            acquire_socket_lock(&paths)?,
            None,
        ),
    };

    let config = Config::load(&paths.config_file)?;
    let store = Store::open(&paths.db_file).await?;
    let reexec = Reexec {
        exe,
        foreground: args.foreground,
        lock,
        socket_lock,
    };
    serve_inner(
        paths,
        config,
        store,
        DesktopNotifier::default(),
        listener,
        Some(reexec),
    )
    .await
}

/// Everything after the lock + store open: reconcile, bind, tick — with the
/// real desktop notifier.
pub async fn serve(paths: Paths, config: Config, store: Store) -> Result<()> {
    serve_with(paths, config, store, DesktopNotifier::default()).await
}

/// `serve` with the §3.5 transport injected — the end-to-end tests (§11)
/// run a real daemon against temp paths without popping real desktop
/// notifications.
pub async fn serve_with<N: Notifier + 'static>(
    paths: Paths,
    config: Config,
    store: Store,
    notifier: N,
) -> Result<()> {
    // In-process: there is no binary of ours to re-execute, so no upgrades.
    serve_inner(paths, config, store, notifier, None, None).await
}

async fn serve_inner<N: Notifier + 'static>(
    paths: Paths,
    config: Config,
    store: Store,
    notifier: N,
    inherited: Option<std::os::unix::net::UnixListener>,
    reexec: Option<Reexec>,
) -> Result<()> {
    let (arm, arm_rx) = mpsc::unbounded_channel();

    // §3.4: resolve every non-terminal run before serving anyone — held
    // runs are parked (with their on_hold notification), interrupted-but-
    // restart-safe steps re-armed, and every waiting cursor rebuilt into
    // the heap. Still TODO: recurring catch-up (§4.2) with recurrence.
    for due in reconcile(&store, &SystemClock.now()).await? {
        let _ = arm.send(due);
    }

    // An inherited listener was never closed, so clients that connected
    // while we re-executed are already queued on it.
    let listener = match inherited {
        Some(listener) => {
            listener
                .set_nonblocking(true)
                .context("adopting the socket")?;
            UnixListener::from_std(listener).context("adopting the socket")?
        }
        None => bind_socket(&paths)?,
    };
    // A second descriptor for the same socket, so it outlives the accept
    // task when an upgrade stops that task, and can cross the exec.
    let listener_fd = {
        use std::os::fd::AsFd;
        listener
            .as_fd()
            .try_clone_to_owned()
            .context("duplicating the listening socket")?
    };
    let (nudge, nudge_rx) = mpsc::unbounded_channel();
    let (upgrade_tx, mut upgrade_rx) = mpsc::unbounded_channel();
    let ctx = Arc::new(Ctx {
        store,
        paths,
        config,
        clock: SystemClock,
        spawner: SystemSpawner,
        arm,
        nudge,
        live: Registry::default(),
        stopping: Arc::default(),
        work: Arc::new(()),
        draining: Arc::default(),
        wake: Arc::default(),
        upgrades: reexec.is_some().then_some(upgrade_tx),
        upgrading: Arc::default(),
        reply_written: Arc::default(),
    });

    // §3.5: the delivery task's immediate first pass drains any backlog —
    // including on_hold notifications reconciliation just enqueued.
    let delivery = tokio::spawn(delivery_loop(ctx.store.clone(), notifier, nudge_rx));
    let accept = tokio::spawn(accept_loop(Arc::clone(&ctx), listener));
    let gc = tokio::spawn(gc_loop(Arc::clone(&ctx)));
    let mut tasks = Some(DaemonTasks {
        delivery,
        accept,
        gc,
    });

    let scheduler = scheduler(Arc::clone(&ctx), arm_rx);
    tokio::pin!(scheduler);
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    // The upgrade in progress, and when its drain gives up.
    let mut pending: Option<(UpgradeOrder, tokio::time::Instant)> = None;

    loop {
        // §2.2 names four things that terminate a step the same way: a
        // timeout, `cued cancel`, a `deadline`, and a clean daemon
        // shutdown. The first three went through the §2.3 registry; this
        // is the fourth.
        tokio::select! {
            result = &mut scheduler => return result,
            signal = &mut shutdown => {
                eprintln!("cued: {signal} — shutting down");
                if let Some((order, _)) = pending.take() {
                    let _ = order.reply.send(Response::UpgradeAbandoned {
                        reason: format!("the daemon received {signal} and is shutting down"),
                    });
                }
                terminate_live_steps(&ctx).await;
                return Ok(());
            }
            Some(order) = upgrade_rx.recv(), if pending.is_none() => {
                let reexec = reexec.as_ref().expect("upgrades are only sent with a reexec");
                // Runs the candidate binary, so keep the scheduler polled
                // meanwhile (see the forced-interrupt note below).
                let preflight = tokio::select! {
                    result = &mut scheduler => return result,
                    preflight = upgrade_preflight(reexec) => preflight,
                };
                match preflight {
                    Err(reply) => {
                        ctx.upgrading.store(false, SeqCst);
                        let _ = order.reply.send(reply);
                    }
                    Ok(()) => {
                        eprintln!(
                            "cued: upgrade requested — draining (up to {:?}{})",
                            order.wait,
                            if order.force { ", then interrupting" } else { "" },
                        );
                        // Stop starting work; what is running finishes.
                        ctx.draining.store(true, SeqCst);
                        let deadline = tokio::time::Instant::now() + order.wait;
                        pending = Some((order, deadline));
                    }
                }
            }
            _ = tokio::time::sleep(DRAIN_POLL), if pending.is_some() => {
                let deadline = pending.as_ref().expect("guarded").1;
                if !is_idle(&ctx) && tokio::time::Instant::now() < deadline {
                    continue;
                }
                let (order, _) = pending.take().expect("guarded");
                if !is_idle(&ctx) {
                    let running = describe_live(&ctx);
                    if !order.force {
                        eprintln!("cued: upgrade abandoned — still running: {running}");
                        ctx.draining.store(false, SeqCst);
                        ctx.upgrading.store(false, SeqCst);
                        ctx.wake.notify_one();
                        let _ = order.reply.send(Response::UpgradeAbandoned {
                            reason: format!(
                                "still running after {}: {running}. Nothing was changed. Retry \
                                 with a longer --wait, or --force to interrupt them (they then \
                                 reconcile per on_interrupt, as after any restart)",
                                crate::timeparse::describe_duration(
                                    SignedDuration::try_from(order.wait).unwrap_or_default()
                                ),
                            ),
                        });
                        continue;
                    }
                    eprintln!("cued: upgrade forced — interrupting: {running}");
                    // Keep the scheduler polled throughout: it can be
                    // parked mid-transaction on the single writer, and
                    // the tasks being waited for may need that writer.
                    let interrupt = async {
                        terminate_live_steps(&ctx).await;
                        wait_idle(&ctx, SHUTDOWN_MARGIN).await;
                    };
                    tokio::select! {
                        result = &mut scheduler => return result,
                        // A stop asked for mid-upgrade wins: finish the
                        // teardown already under way and exit, not exec.
                        signal = &mut shutdown => {
                            eprintln!("cued: {signal} during upgrade — shutting down instead");
                            let _ = order.reply.send(Response::UpgradeAbandoned {
                                reason: format!("the daemon received {signal} and is shutting down"),
                            });
                            terminate_live_steps(&ctx).await;
                            return Ok(());
                        }
                        () = interrupt => {}
                    }
                }
                let reexec = reexec.as_ref().expect("an upgrade was accepted");
                let tasks = tasks.take().expect("one upgrade reaches exec");
                let handoff = hand_over(&ctx, reexec, tasks, order, &listener_fd);
                tokio::select! {
                    result = &mut scheduler => return result,
                    // Dropping the handoff drops the order, so the requester
                    // hears the upgrade did not finish.
                    signal = &mut shutdown => {
                        eprintln!("cued: {signal} during upgrade — shutting down instead");
                        terminate_live_steps(&ctx).await;
                        return Ok(());
                    }
                    error = handoff => {
                        // Locks, socket and store are already given up;
                        // the only way back to a working daemon is a new
                        // image of some kind.
                        reexec_failed(reexec, &listener_fd, error);
                    }
                }
            }
        }
    }
}

/// How often a drain checks whether the running work has finished.
const DRAIN_POLL: Duration = Duration::from_millis(50);

/// How long the last stage of an upgrade waits for each of: request
/// handlers in flight, the requester's reply, the store's close.
const HANDOFF_STEP: Duration = Duration::from_secs(5);

use std::sync::atomic::Ordering::SeqCst;

/// What an upgrade needs from startup: what to re-execute, and the two
/// locks the new image must inherit rather than race for.
struct Reexec {
    exe: Option<std::path::PathBuf>,
    foreground: bool,
    lock: File,
    socket_lock: File,
}

struct DaemonTasks {
    delivery: tokio::task::JoinHandle<()>,
    accept: tokio::task::JoinHandle<()>,
    gc: tokio::task::JoinHandle<()>,
}

/// One `cued upgrade`, from the request handler to the main loop.
struct UpgradeOrder {
    wait: Duration,
    force: bool,
    reply: tokio::sync::oneshot::Sender<Response>,
}

/// Before draining anything: is there a different, runnable binary to
/// become? Draining a busy daemon only to find nothing to exec would be
/// a pointless pause.
async fn upgrade_preflight(reexec: &Reexec) -> std::result::Result<(), Response> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let Some(exe) = &reexec.exe else {
        return Err(Response::UpgradeAbandoned {
            reason: "this daemon could not resolve the path it was started from; \
                     restart it once (`systemctl --user restart cued`, or stop it and \
                     run any cued command) and upgrades will work from then on"
                .into(),
        });
    };
    let installed = match std::fs::metadata(exe) {
        Ok(metadata) => metadata,
        Err(error) => {
            return Err(Response::UpgradeAbandoned {
                reason: format!("nothing to upgrade to at {}: {error}", exe.display()),
            });
        }
    };
    if !installed.is_file() || installed.permissions().mode() & 0o111 == 0 {
        return Err(Response::UpgradeAbandoned {
            reason: format!("{} is not an executable file", exe.display()),
        });
    }
    // The magic link resolves to the image actually running, even after
    // its path was renamed over.
    if let Ok(running) = std::fs::metadata("/proc/self/exe")
        && (running.dev(), running.ino()) == (installed.dev(), installed.ino())
    {
        return Err(Response::UpgradeCurrent { exe: exe.clone() });
    }
    // Wrong architecture, missing libraries, a half-written file: find out
    // now, while backing off costs nothing, rather than at the exec.
    let probe = tokio::process::Command::new(exe)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .output();
    let failure = match tokio::time::timeout(PREFLIGHT_TIMEOUT, probe).await {
        Ok(Ok(output)) if output.status.success() => return Ok(()),
        Ok(Ok(output)) => format!(
            "{} ({})",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Ok(Err(error)) => error.to_string(),
        Err(_) => format!("no answer within {PREFLIGHT_TIMEOUT:?}"),
    };
    Err(Response::UpgradeAbandoned {
        reason: format!(
            "{} does not run: `--version` failed: {failure}",
            exe.display()
        ),
    })
}

/// How long the candidate binary gets to answer `--version`.
const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(10);

/// Spawn work an upgrade's drain must wait for. Every task that claims,
/// runs or closes steps, or otherwise writes the store, goes through here:
/// `is_idle` can only see what holds a `work` token, so a bare
/// `tokio::spawn` of such work would let an upgrade exec in the middle of
/// it.
fn spawn_tracked<F>(ctx: &Ctx, work: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let token = WorkToken::new(ctx);
    tokio::spawn(async move {
        let _token = token;
        work.await
    });
}

/// Proof of in-flight work, held for as long as the work runs (see
/// `spawn_tracked`, and `is_idle`, which counts these).
struct WorkToken {
    _held: Arc<()>,
}

impl WorkToken {
    fn new(ctx: &Ctx) -> Self {
        Self {
            _held: Arc::clone(&ctx.work),
        }
    }
}

/// Nothing the scheduler started is still in flight — no step task, no
/// firing, no request handler — and no process group is held.
fn is_idle(ctx: &Ctx) -> bool {
    Arc::strong_count(&ctx.work) == 1 && ctx.live.lock().expect("registry").is_empty()
}

async fn wait_idle(ctx: &Ctx, within: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + within;
    while !is_idle(ctx) {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(DRAIN_POLL).await;
    }
    true
}

/// "j4.r2 attempt 1, j9.r7 attempt 3" — what an upgrade is waiting on.
fn describe_live(ctx: &Ctx) -> String {
    let mut attempts: Vec<String> = ctx
        .live
        .lock()
        .expect("registry")
        .keys()
        .map(|(job, run, attempt)| format!("{job}.r{} attempt {attempt}", run.0))
        .collect();
    attempts.sort();
    if attempts.is_empty() {
        // Between steps: a claim, a close, or a request handler.
        "internal work (no step processes)".into()
    } else {
        attempts.join(", ")
    }
}

/// The drained daemon's last act: stop accepting (the socket stays open,
/// so new clients queue for the next image), answer the requester, release
/// the store, and exec. Returns only if the exec failed.
async fn hand_over(
    ctx: &Ctx,
    reexec: &Reexec,
    tasks: DaemonTasks,
    order: UpgradeOrder,
    listener: &std::os::fd::OwnedFd,
) -> std::io::Error {
    tasks.accept.abort();
    let _ = tasks.accept.await;
    // A request accepted just before may still be mid-dispatch.
    if !wait_idle(ctx, HANDOFF_STEP).await {
        eprintln!("cued: request handlers still busy; re-executing anyway");
    }
    let exe = reexec.exe.clone().expect("preflight checked");
    eprintln!("cued: drained — re-executing {}", exe.display());
    let notified = ctx.reply_written.notified();
    let _ = order.reply.send(Response::Upgrading { exe: exe.clone() });
    let _ = tokio::time::timeout(HANDOFF_STEP, notified).await;

    tasks.delivery.abort();
    tasks.gc.abort();
    let _ = tasks.delivery.await;
    let _ = tasks.gc.await;
    // A clean close checkpoints the WAL. Bounded: the scheduler may hold
    // the writer mid-sweep, and SQLite's own journal makes abandoning that
    // transaction at exec safe anyway.
    let _ = tokio::time::timeout(HANDOFF_STEP, ctx.store.close()).await;

    exec_daemon(&exe, reexec, listener)
}

/// Become `exe`, handing it the locks and socket. Only returns on failure.
fn exec_daemon(
    exe: &std::path::Path,
    reexec: &Reexec,
    listener: &std::os::fd::OwnedFd,
) -> std::io::Error {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    let fds = [
        reexec.lock.as_raw_fd(),
        reexec.socket_lock.as_raw_fd(),
        listener.as_raw_fd(),
    ];
    for fd in fds {
        if let Err(error) = set_cloexec(fd, false) {
            return error;
        }
    }
    let mut command = std::process::Command::new(exe);
    command
        .arg("daemon")
        .arg("--handoff")
        .arg(format!("{},{},{}", fds[0], fds[1], fds[2]));
    if reexec.foreground {
        command.arg("--foreground");
    }
    let error = command.exec();
    // Still us: put the descriptors back out of reach of future children.
    for fd in fds {
        let _ = set_cloexec(fd, true);
    }
    error
}

/// The new binary would not exec. The old image is still on disk as
/// /proc/self/exe whatever happened to its path, so become that instead;
/// failing even that, exit and let the supervisor (or the next client)
/// start a daemon.
fn reexec_failed(reexec: &Reexec, listener: &std::os::fd::OwnedFd, error: std::io::Error) -> ! {
    eprintln!("cued: upgrade exec failed ({error}); restarting the running version");
    let error = exec_daemon(std::path::Path::new("/proc/self/exe"), reexec, listener);
    eprintln!("cued: could not restart either ({error}); exiting");
    std::process::exit(1)
}

fn set_cloexec(fd: i32, on: bool) -> std::io::Result<()> {
    // SAFETY: F_GETFD/F_SETFD on a descriptor this process owns.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags == -1 {
            return Err(std::io::Error::last_os_error());
        }
        let flags = if on {
            flags | libc::FD_CLOEXEC
        } else {
            flags & !libc::FD_CLOEXEC
        };
        if libc::fcntl(fd, libc::F_SETFD, flags) == -1 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// The new image's side of `exec_daemon`: take ownership of the inherited
/// descriptors, after checking they really are this deployment's lock
/// files and socket — `--handoff` is a CLI flag, and a wrong fd adopted as
/// a lock would let two daemons share a store.
fn adopt_handoff(
    paths: &Paths,
    fds: &str,
) -> Result<(File, File, std::os::unix::net::UnixListener)> {
    use std::os::fd::FromRawFd;
    use std::os::unix::fs::MetadataExt;

    let fds: Vec<i32> = fds
        .split(',')
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()
        .context("--handoff takes three file descriptors")?;
    let [lock, socket_lock, listener] = fds[..] else {
        anyhow::bail!("--handoff takes three file descriptors");
    };
    for fd in [lock, socket_lock, listener] {
        ensure!(fd > 2, "--handoff fd {fd} is a standard stream");
        // Before anything else: no job's child may inherit these.
        set_cloexec(fd, true).with_context(|| format!("--handoff fd {fd} is not open"))?;
    }

    let adopt_lock = |fd: i32, path: &std::path::Path| -> Result<File> {
        // SAFETY: fd was checked open above, and is ours from here on.
        let file = unsafe { File::from_raw_fd(fd) };
        let held = file.metadata()?;
        let named =
            std::fs::metadata(path).with_context(|| format!("checking {}", path.display()))?;
        ensure!(
            (held.dev(), held.ino()) == (named.dev(), named.ino()),
            "--handoff fd {fd} is not {}",
            path.display()
        );
        // Re-locking through the same open file description we inherited
        // succeeds; any other holder would make this fail.
        file.try_lock()
            .with_context(|| format!("re-asserting the lock on {}", path.display()))?;
        Ok(file)
    };
    let lock = adopt_lock(lock, &paths.lock_file)?;
    let socket_lock = adopt_lock(socket_lock, &paths.socket_lock())?;

    // SAFETY: as above.
    let listener = unsafe { std::os::unix::net::UnixListener::from_raw_fd(listener) };
    let bound = listener
        .local_addr()
        .context("inspecting the inherited socket")?;
    ensure!(
        bound.as_pathname() == Some(paths.socket_file.as_path()),
        "the inherited socket is not {}",
        paths.socket_file.display()
    );
    Ok((lock, socket_lock, listener))
}

/// Resolve on SIGTERM (what a service manager sends) or SIGINT (what a
/// terminal sends), naming which arrived.
async fn shutdown_signal() -> &'static str {
    let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(term) => term,
        // Without a handler there is nothing to wait for; the default
        // disposition still ends the process, just without the teardown.
        Err(error) => {
            eprintln!("cued: cannot listen for SIGTERM ({error}); shutdown will be abrupt");
            std::future::pending::<()>().await;
            unreachable!()
        }
    };
    tokio::select! {
        _ = term.recv() => "SIGTERM",
        _ = tokio::signal::ctrl_c() => "SIGINT",
    }
}

/// Reach every attempt still holding a process group and run the one §2.2
/// sequence against it, then wait for those sequences to finish.
///
/// The waiting is the load-bearing part. Each teardown is TERM → grace →
/// KILL, and the timer and the final KILL live inside the exec task —
/// returning from here drops the runtime and takes any unfinished teardown
/// with it, so a step that ignores SIGTERM would survive the very shutdown
/// meant to stop it. The registry emptying is the signal that they are done,
/// because each task removes its own entry once the process is reaped.
///
/// Run cursors are deliberately left at `Running`: see the `stopping` check
/// in `try_run_step`. We killed these steps mid-execution and cannot know how
/// far they got, which is exactly §3.4's Case 2 — the next startup applies
/// `on_interrupt` and, by default, parks them in `Held` for a human. An
/// *unclean* kill (SIGKILL, power loss) reaches none of this and lands in the
/// same place, which is why that case had to exist anyway (§2.2).
async fn terminate_live_steps(ctx: &Ctx) {
    // Before any handle is triggered, so no attempt can be killed and then
    // reach `close_step` before it knows why it died.
    ctx.stopping
        .store(true, std::sync::atomic::Ordering::SeqCst);

    let (signalled, longest) = {
        let registry = ctx.live.lock().expect("registry");
        for attempt in registry.values() {
            attempt.cancel.signal();
        }
        let longest = registry
            .values()
            .map(|attempt| attempt.kill_grace)
            .max()
            .unwrap_or(Duration::ZERO);
        (registry.len(), longest)
    };
    if signalled == 0 {
        // Nothing was holding a process group. A step still being claimed
        // sees `stopping` — set above, before this snapshot — and bails
        // before spawning, so there is nothing left to wait for.
        return;
    }
    eprintln!("cued: terminating {signalled} running step(s) (§2.2 TERM → grace → KILL)");

    let budget = longest + SHUTDOWN_MARGIN;
    let deadline = tokio::time::Instant::now() + budget;
    while tokio::time::Instant::now() < deadline {
        if ctx.live.lock().expect("registry").is_empty() {
            eprintln!("cued: all steps terminated");
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let stuck = ctx.live.lock().expect("registry").len();
    eprintln!(
        "cued: {stuck} step(s) did not finish terminating within {budget:?};          they may outlive this daemon and will reconcile as interrupted (§3.4)"
    );
}

// ---------------------------------------------------------------------------
// Notification delivery (§3.5)
// ---------------------------------------------------------------------------

/// Deliver, then wait for whichever comes first: a nudge from an enqueueing
/// path (a fresh popup should not wait for the tick), a late acknowledgement
/// settling, or the slow tick (the backlog retry that makes "arrives when you
/// next log in" true).
async fn delivery_loop<N: Notifier>(
    store: Store,
    notifier: N,
    mut nudge: mpsc::UnboundedReceiver<()>,
) {
    let mut ledger = DeliveryLedger::default();
    loop {
        if let Err(error) =
            deliver_pending(&store, &notifier, &mut ledger, &SystemClock.now()).await
        {
            eprintln!("cued: notification delivery pass failed: {error:#}");
        }
        tokio::select! {
            _ = tokio::time::sleep(SLOW_TICK) => {}
            _ = notifier.settled() => {}
            received = nudge.recv() => {
                if received.is_none() {
                    return; // daemon shutting down
                }
            }
        }
    }
}

/// Rows this daemon has seen acknowledged but could not yet record (§3.5).
/// Keyed by queue row id, which is never reused, so a repeated reminder — a
/// separate row — is never mistaken for one already shown. In memory only:
/// if the daemon dies first, the row is still undelivered and is shown
/// again (at-least-once; nothing durable can say "shown" when the write
/// saying so is what failed).
#[derive(Debug, Default)]
pub struct DeliveryLedger {
    shown: HashMap<i64, Option<DeliveryReceipt>>,
}

impl DeliveryLedger {
    /// Acknowledged, awaiting its durable record.
    pub fn unrecorded(&self) -> usize {
        self.shown.len()
    }
}

/// One pass over the §3.5 backlog, oldest first. Public so tests drive it
/// like `reconcile`/`fire_job`. No bus ends the pass (nothing else will
/// land either); a single failed row is logged and skipped, staying queued
/// rather than damming everything behind it.
///
/// A row is marked only after its display was acknowledged. A row already
/// acknowledged this daemon lifetime is only ever re-*recorded*, never
/// re-shown; while recording fails, nothing new goes on screen (each more
/// would be another row a restart re-shows).
pub async fn deliver_pending(
    store: &Store,
    notifier: &impl Notifier,
    ledger: &mut DeliveryLedger,
    now: &Timestamp,
) -> Result<usize> {
    let pending = store.undelivered_notifications().await?;
    let pending_ids: std::collections::HashSet<_> = pending.iter().map(|row| row.id).collect();
    notifier.retain_pending(&pending_ids);
    // Recorded elsewhere or deleted (retention) since: nothing to remember.
    ledger.shown.retain(|id, _| pending_ids.contains(id));

    let (shown, unshown): (Vec<_>, Vec<_>) = pending
        .into_iter()
        .partition(|row| ledger.shown.contains_key(&row.id));

    let mut delivered = 0;
    let mut recording = true;
    for row in shown {
        let receipt = ledger.shown[&row.id].clone();
        if record(store, row.id, receipt.as_ref(), now).await {
            ledger.shown.remove(&row.id);
            delivered += 1;
        } else {
            recording = false;
        }
    }
    if !recording {
        return Ok(delivered);
    }

    for row in unshown {
        match notifier.attempt(row.id, &row.spec).await {
            Ok(Delivery::Shown(receipt)) => {
                if record(store, row.id, receipt.as_ref(), now).await {
                    delivered += 1;
                } else {
                    ledger.shown.insert(row.id, receipt);
                    break;
                }
            }
            Ok(Delivery::Awaiting) => {}
            Ok(Delivery::TimedOut) => {
                eprintln!(
                    "cued: notification {} not acknowledged yet; awaiting its answer instead of re-sending",
                    row.id
                );
                break;
            }
            Ok(Delivery::Unavailable) => break,
            Err(error) => {
                eprintln!("cued: failed to deliver notification {}: {error:#}", row.id);
            }
        }
    }
    Ok(delivered)
}

async fn record(
    store: &Store,
    id: i64,
    receipt: Option<&DeliveryReceipt>,
    now: &Timestamp,
) -> bool {
    match store.mark_delivered(id, receipt, now).await {
        Ok(()) => true,
        Err(error) => {
            eprintln!(
                "cued: notification {id} was shown but recording it failed: {error:#}; \
                 will retry the record, not the display"
            );
            false
        }
    }
}

/// Point this process's stderr at the daemon log, so every `eprintln!`
/// (and any panic message) is durable instead of going to the /dev/null the
/// auto-spawn hands us. Appended, never rotated — 0600 like everything else
/// holding job data (§7.5).
fn redirect_stderr_to_log(log_file: &std::path::Path) -> Result<()> {
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(log_file)
        .with_context(|| format!("opening daemon log {}", log_file.display()))?;
    // SAFETY: dup2 onto fd 2 with a file we own and keep alive across the
    // call; the only failure mode is EBADF/EINTR, which we surface.
    let duplicated = unsafe { libc::dup2(log.as_raw_fd(), libc::STDERR_FILENO) };
    if duplicated == -1 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("redirecting stderr to {}", log_file.display()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Retention (§10.2)
// ---------------------------------------------------------------------------

/// Sweep at startup, then daily. A GC failure is logged and the loop keeps
/// going: retention falling behind is untidy, but a daemon that stopped
/// scheduling because it couldn't tidy up would be a great deal worse.
async fn gc_loop(ctx: Arc<Ctx>) {
    loop {
        let work = WorkToken::new(&ctx);
        let swept = collect_garbage(
            &ctx.store,
            &ctx.paths,
            &ctx.config.retention,
            &ctx.clock.now(),
        )
        .await;
        drop(work);
        match swept {
            Ok(outcome) if !outcome.runs.is_empty() || !outcome.jobs.is_empty() => {
                eprintln!(
                    "cued: gc pruned {} run(s) and {} job(s)",
                    outcome.runs.len(),
                    outcome.jobs.len()
                );
            }
            Ok(_) => {}
            Err(error) => eprintln!("cued: gc pass failed: {error:#}"),
        }
        tokio::time::sleep(GC_TICK).await;
    }
}

/// One §10.2 sweep: the store decides what goes, then the log files follow.
/// Takes its pieces rather than the daemon's context so the retention tests
/// drive the identical path, the way `reconcile` and `fire_job` do (§11).
pub async fn collect_garbage(
    store: &Store,
    paths: &Paths,
    retention: &Retention,
    now: &Timestamp,
) -> Result<GcOutcome> {
    let mut outcome = GcOutcome::default();
    let swept = store.gc_into(retention, now, &mut outcome).await;

    // The rows are gone; the bytes should follow, or "pruned" would only
    // mean "invisible" and the logs directory would grow forever (§2.1).
    // Even when the sweep failed part-way: its earlier batches committed.
    // Removing after the commit is safe because a pruned run's id is never
    // handed out again (`jobs.run_seq`), so no new run can own these paths.
    for (job, run) in &outcome.runs {
        remove_dir_if_present(&paths.run_log_dir(*job, *run));
    }
    for job in &outcome.jobs {
        remove_dir_if_present(&paths.job_log_dir(*job));
    }
    swept.map(|()| outcome)
}

/// A log directory that isn't there is the expected case for a run that
/// never spawned a process — a reminder, a skipped firing (§3.5, §4.2).
fn remove_dir_if_present(dir: &std::path::Path) {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => eprintln!("cued: could not remove {}: {error}", dir.display()),
    }
}

/// Try to take the exclusive flock; a held lock means another daemon is
/// alive, which is a clean, expected exit — not an error to retry.
fn acquire_instance_lock(paths: &Paths) -> Result<File> {
    crate::paths::try_lock(&paths.lock_file)?
        .with_context(|| format!("another cued daemon holds {}", paths.lock_file.display()))
}

/// The data-directory flock alone doesn't own the socket: the socket's
/// directory is chosen independently (`/run/user/<uid>`, or
/// `CUED_SOCKET_DIR`), so two daemons with different `XDG_DATA_HOME` hold
/// different data locks yet share one socket path. Without this second
/// lock the later one unlinked the live socket and bound over it, leaving
/// the first daemon running its jobs unreachable. Locking a file beside the
/// socket makes "we hold the lock" true of the socket too.
fn acquire_socket_lock(paths: &Paths) -> Result<File> {
    crate::paths::try_lock(&paths.socket_lock())?.with_context(|| {
        format!(
            "another cued daemon is serving {} (possibly for a different data directory)",
            paths.socket_file.display()
        )
    })
}

/// §5.2: we hold both flocks, so any existing socket file is stale by
/// definition — unlink and bind fresh.
fn bind_socket(paths: &Paths) -> Result<UnixListener> {
    match std::fs::remove_file(&paths.socket_file) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("removing stale socket {}", paths.socket_file.display()));
        }
    }
    UnixListener::bind(&paths.socket_file)
        .with_context(|| format!("binding {}", paths.socket_file.display()))
}

/// One attempt currently holding a process group (§2.3).
struct LiveAttempt {
    cancel: Cancel,
    /// This attempt's §2.2 TERM→KILL grace, so a shutdown can wait exactly
    /// as long as the teardowns it started may take. A fixed constant was
    /// wrong the moment `kill_grace` became configurable — and it is
    /// configurable per job *and* per step.
    kill_grace: Duration,
    /// The absolute instant this attempt must not outlive — the run's
    /// `deadline`, frozen to wall clock at spawn, the same discipline §3.2
    /// applies to waits. `None` = no cap.
    ///
    /// The in-task timer in `try_run_step` is the primary trigger; this copy
    /// exists so the tick can sweep for overdue instants, because
    /// `CLOCK_MONOTONIC` stalls across a suspend and a timer that slept
    /// through one would fire late by however long the machine was out.
    kill_at: Option<Timestamp>,
}

/// Owns the registry entry from registration through process teardown. Before
/// the execution task is launched, dropping this guard means there can be no
/// child, so it can remove the entry itself. After handoff, it only signals;
/// the execution task removes the entry after reaping the process group.
struct AttemptOwnership {
    live: Registry,
    job: JobId,
    run: RunId,
    attempt: u32,
    cancel: Cancel,
    armed: bool,
    handed_off: bool,
}

impl AttemptOwnership {
    fn new(live: Registry, job: JobId, run: RunId, attempt: u32, cancel: Cancel) -> Self {
        Self {
            live,
            job,
            run,
            attempt,
            cancel,
            armed: true,
            handed_off: false,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }

    fn hand_off(&mut self) {
        self.handed_off = true;
    }
}

impl Drop for AttemptOwnership {
    fn drop(&mut self) {
        if self.armed {
            self.cancel.signal();
            if !self.handed_off {
                unregister_live(&self.live, self.job, self.run, self.attempt);
            }
        }
    }
}

/// §2.3's live-execution registry, keyed by attempt rather than just run.
/// Daemon state rather than spawner state on purpose — the spawner only
/// knows how to die, not who is allowed to ask. `cued cancel` and `deadline`
/// expiry reach live process groups through here; clean shutdown is the
/// third §2.3 name for the same reach.
///
/// A run can have two attempts alive at once: `cued cancel` puts the running
/// one into its `kill_grace` teardown, and a `cued retry` right behind it
/// claims the same step again (§3.4) and spawns its replacement while the
/// first process is still dying. Keyed by `(job, run)` alone, the newer
/// attempt's handle overwrote the older one's, and the older task's cleanup
/// then removed the *newer* attempt's handle on its way out — leaving a live
/// process nothing could signal, so the next `cued cancel` reported success
/// and killed nothing. Same lesson as the store's `Claim` fence, one layer
/// up: identify the attempt, not just the run.
type Registry = Arc<Mutex<HashMap<(JobId, RunId, u32), LiveAttempt>>>;

/// Shared daemon state. Clock and spawner are the §11 seams; they stay
/// concrete here until the reconciliation milestone needs to inject them.
struct Ctx {
    store: Store,
    paths: Paths,
    config: Config,
    clock: SystemClock,
    spawner: SystemSpawner,
    arm: mpsc::UnboundedSender<Arm>,
    /// Wakes the §3.5 delivery task after a path that may have enqueued.
    nudge: mpsc::UnboundedSender<()>,
    live: Registry,
    /// Set once a shutdown begins, so a step killed *by* the shutdown is not
    /// then written up as a step that failed (§3.4).
    stopping: Arc<std::sync::atomic::AtomicBool>,
    /// One clone per in-flight piece of work an upgrade must not cut short:
    /// each step or firing task the scheduler spawns, each request being
    /// dispatched, each GC pass. Idle when only this one remains.
    work: Arc<()>,
    /// An upgrade is draining: the scheduler keeps its heap but pops nothing.
    draining: Arc<std::sync::atomic::AtomicBool>,
    /// Wakes the scheduler when a drain is called off.
    wake: Arc<tokio::sync::Notify>,
    /// To the main loop; `None` when there is no binary to re-execute
    /// (an in-process daemon).
    upgrades: Option<mpsc::UnboundedSender<UpgradeOrder>>,
    /// One upgrade at a time.
    upgrading: Arc<std::sync::atomic::AtomicBool>,
    /// The `Upgrading` reply reached the requester's socket, so the exec
    /// that follows can't cut it off.
    reply_written: Arc<tokio::sync::Notify>,
}

// ---------------------------------------------------------------------------
// The tick loop (§3.2, §5.2)
// ---------------------------------------------------------------------------

/// One heap entry's worth of future work: run a specific step of a specific
/// run, or fire a recurring job's next instant (§4.2) — the two things a
/// tick can owe the world.
#[derive(Debug, Clone)]
pub enum Arm {
    Step(DueStep),
    /// A step whose previous run attempt failed with an internal error,
    /// re-armed at `at` (§3.3). `due.at` stays the step's *frozen* target so
    /// the `missed_wait` check keeps measuring against the real deadline;
    /// `at` is just when to try again.
    RetryStep {
        due: DueStep,
        at: Timestamp,
        retry: u32,
    },
    Fire {
        job: JobId,
        at: Timestamp,
    },
    /// A firing whose attempt failed with an internal error, re-armed at
    /// `at`. `claiming` stays the *original* instant, because that is what
    /// the store is still expecting — a retry has to claim the same firing,
    /// not invent a later one.
    RetryFire {
        job: JobId,
        claiming: Timestamp,
        at: Timestamp,
        retry: u32,
    },
}

impl Arm {
    fn at(&self) -> Timestamp {
        match self {
            Arm::Step(due) => due.at,
            Arm::RetryStep { at, .. } | Arm::Fire { at, .. } | Arm::RetryFire { at, .. } => *at,
        }
    }
}

/// Min-heap key: instant first, insertion order as the tiebreak.
struct HeapEntry {
    at: Timestamp,
    seq: u64,
    arm: Arm,
}

impl PartialEq for HeapEntry {
    fn eq(&self, other: &Self) -> bool {
        (self.at, self.seq) == (other.at, other.seq)
    }
}
impl Eq for HeapEntry {}
impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.at, self.seq).cmp(&(other.at, other.seq))
    }
}

async fn scheduler(ctx: Arc<Ctx>, mut arm: mpsc::UnboundedReceiver<Arm>) -> Result<()> {
    let mut heap: BinaryHeap<Reverse<HeapEntry>> = BinaryHeap::new();
    let mut seq = 0u64;
    loop {
        // Due = stored target ≤ wall clock, never a countdown (§9).
        let wall = ctx.clock.now();
        sweep_deadlines(&ctx.live, &wall);
        if let Err(error) = ctx.store.expire_pending(&wall).await {
            eprintln!("cued: approval expiry sweep failed: {error:#}");
        }
        let now = wall;
        // An upgrade is draining: due entries stay on the heap. Nothing is
        // lost if the exec goes ahead — the store has every one of them,
        // and the next image's `reconcile` rebuilds the heap from it.
        let draining = ctx.draining.load(SeqCst);
        while !draining && heap.peek().is_some_and(|Reverse(entry)| entry.at <= now) {
            let Reverse(entry) = heap.pop().expect("peeked");
            match entry.arm {
                Arm::Step(due) => spawn_tracked(&ctx, run_step(Arc::clone(&ctx), due, 0)),
                Arm::RetryStep { due, retry, .. } => {
                    spawn_tracked(&ctx, run_step(Arc::clone(&ctx), due, retry))
                }
                Arm::Fire { job, at } => {
                    spawn_tracked(&ctx, fire_job_task(Arc::clone(&ctx), job, at, 0))
                }
                Arm::RetryFire {
                    job,
                    claiming,
                    retry,
                    ..
                } => spawn_tracked(&ctx, fire_job_task(Arc::clone(&ctx), job, claiming, retry)),
            }
        }

        let sleep_for = match heap.peek() {
            // Overdue entries would otherwise make this a busy loop.
            _ if draining => MAX_TICK,
            Some(Reverse(entry)) => Duration::try_from(entry.at.duration_since(now))
                .unwrap_or(Duration::ZERO)
                .min(MAX_TICK),
            None => MAX_TICK,
        };

        tokio::select! {
            _ = tokio::time::sleep(sleep_for) => {}
            // A drain was called off; overdue entries go now.
            _ = ctx.wake.notified() => {}
            received = arm.recv() => match received {
                Some(arm) => {
                    seq += 1;
                    heap.push(Reverse(HeapEntry { at: arm.at(), seq, arm }));
                }
                // Every sender dropped — the daemon is shutting down.
                None => return Ok(()),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Running one due step (§3.2, §3.3)
// ---------------------------------------------------------------------------

/// What conditions are evaluated over, uniform across action types (§3.2).
struct StepOutcome {
    exit_code: Option<i32>,
    timed_out: bool,
    success: bool,
    stdout: String,
    stderr: String,
}

/// Run one due step, and — the point of the `retry` argument — make sure an
/// internal failure can't quietly cost the run its place on the heap.
///
/// The entry was popped before this task was spawned, so returning early on
/// an error would strand the run: its cursor still says `waiting`, nothing
/// holds a heap entry for it, and only a daemon restart (where `reconcile`
/// re-reads waiting cursors) would ever pick it up again. A job that never
/// fires and never says why is the one failure this tool cannot have.
///
/// Re-arming is safe to do blindly because of §3.3's ordering: `begin_step`
/// claims the step with a conditional cursor update, so a re-attempt after
/// the process already spawned finds the cursor at `running`, claims
/// nothing, and returns without a second spawn. That same no-op is why this
/// retry only covers the *pre*-claim window: after the claim it could never
/// move the run again, so everything past it retries in place instead (see
/// `after_claim`) rather than leaving the run `running` until a restart.
///
/// Once the budget is gone we park the run ourselves rather than leave it
/// silently stuck — the same "unknown state, a human decides" destination
/// §3.4 gives an interrupted step.
async fn run_step(ctx: Arc<Ctx>, due: DueStep, retry: u32) {
    let Err(error) = try_run_step(&ctx, &due).await else {
        return;
    };
    eprintln!(
        "cued: internal error running {}.{} step {:?} (try {}): {error:#}",
        due.job,
        due.run,
        due.step,
        retry + 1
    );

    let next = retry + 1;
    if next < STEP_RETRY_BUDGET {
        let delay = STEP_RETRY_BACKOFF * 2i32.saturating_pow(retry);
        match ctx.clock.now().checked_add(delay) {
            Ok(at) => {
                eprintln!(
                    "cued: re-arming {}.{} step {:?} in {delay:#}",
                    due.job, due.run, due.step
                );
                let _ = ctx.arm.send(Arm::RetryStep {
                    due,
                    at,
                    retry: next,
                });
                return;
            }
            // Unreachable short of the end of representable time; fall
            // through to the park rather than drop the run.
            Err(error) => eprintln!("cued: cannot schedule a retry: {error:#}"),
        }
    }

    if let Err(error) = park_errored_run(&ctx, &due, None).await {
        // The store is the thing that's broken, so there is nowhere left to
        // record this but the log. Startup reconciliation is the backstop.
        eprintln!(
            "cued: could not park {}.{} after {STEP_RETRY_BUDGET} failed tries: {error:#}",
            due.job, due.run
        );
    }
}

/// §3.2: the run is out of budget. The run ends `Failed` with the reason on
/// record and `on_failure` fires; the attempt's own row is still closed, so
/// `cued logs` can show what the killed step managed to emit.
///
/// Marking the run terminal *first* is what stops the step it died during
/// from walking the run forward — `finish_step` is conditional on the run
/// still being live, the same guard `cued cancel` relies on.
async fn end_on_deadline(
    ctx: &Ctx,
    job: &Job,
    due: &DueStep,
    // `None` when the budget was already spent before the step was claimed
    // — there is no attempt to close, because none was started.
    attempt: Option<u32>,
    ended_at: &Timestamp,
) -> Result<()> {
    let outcome = ctx
        .store
        .fail_deadline(
            due.job,
            due.run,
            &job.graph.entry,
            ended_at,
            job.hooks.on_failure.as_ref(),
            attempt.map(|attempt| Claim {
                step: &due.step,
                attempt,
            }),
        )
        .await?;
    if let Some(drained) = outcome {
        // Released by the same commit that ended the run (§4.2).
        if let Some((run, at)) = drained {
            let _ = ctx.arm.send(Arm::Step(DueStep {
                job: due.job,
                run,
                step: job.graph.entry.clone(),
                at,
            }));
        }
        eprintln!(
            "cued: {}.{} exceeded its deadline during step {:?}",
            due.job, due.run, due.step
        );
        let _ = ctx.nudge.send(());
    }
    // The attempt row was closed by that same commit, whatever happened to
    // the run, so the audit trail records that this step ran and when it
    // stopped — with no window in which the run has ended and it has not.
    Ok(())
}

/// Store work that has to land *after* `begin_step` claimed the step.
///
/// Past the claim the cursor says `running`, and only this task can move it
/// on. So these failures must not go back to `run_step`'s retry: that
/// re-arms the whole step, whose claim finds the cursor already `running`,
/// takes that for a duplicate, and quietly does nothing — leaving the run
/// stuck `running` forever and the §4.2 queue behind it blocked. Instead
/// the operation is retried here, in place, with whatever it needs (the
/// step's outcome) still in hand, on the same budget and backoff; spent,
/// the run is parked in `Held` like any other errored step.
///
/// `None` means the operation did not land and the run has been dealt with
/// (parked, or left for restart reconciliation during a shutdown).
async fn after_claim<T, F, Fut>(
    ctx: &Ctx,
    due: &DueStep,
    claim: Claim<'_>,
    what: &str,
    mut op: F,
) -> Option<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    for retry in 0..STEP_RETRY_BUDGET {
        let error = match op().await {
            Ok(value) => return Some(value),
            Err(error) => error,
        };
        eprintln!(
            "cued: internal error {what} {}.{} step {:?} (try {}): {error:#}",
            due.job,
            due.run,
            due.step,
            retry + 1
        );
        // §3.4 Case 2 is what a shutdown leaves a claimed step as anyway —
        // the same reason the fire path stops before closing one.
        if ctx.stopping.load(std::sync::atomic::Ordering::SeqCst) {
            return None;
        }
        if retry + 1 < STEP_RETRY_BUDGET {
            let delay = STEP_RETRY_BACKOFF * 2i32.saturating_pow(retry);
            tokio::time::sleep(Duration::try_from(delay).unwrap_or(Duration::ZERO)).await;
        }
    }
    if let Err(error) = park_errored_run(ctx, due, Some(claim)).await {
        // Startup reconciliation finds a `running` cursor with no process
        // and applies `on_interrupt` — the backstop when even this fails.
        eprintln!(
            "cued: could not park {}.{} after {STEP_RETRY_BUDGET} failed tries {what}: {error:#}",
            due.job, due.run
        );
    }
    None
}

/// §3.3: the retry budget is spent — park the run in `Held` with the §3.5
/// notification, so it shows up in `cued list` and on the desktop instead of
/// sitting on a `waiting` cursor nothing is going to pop.
async fn park_errored_run(ctx: &Ctx, due: &DueStep, claim: Option<Claim<'_>>) -> Result<()> {
    let job = ctx.store.load_job(due.job).await?;
    let notify = job
        .hooks
        .on_hold
        .clone()
        .unwrap_or_else(|| on_hold_message(&job, &due.step, HeldReason::Errored));
    ctx.store
        .hold_run(
            due.job,
            due.run,
            &due.step,
            HeldReason::Errored,
            &notify,
            &ctx.clock.now(),
            claim,
        )
        .await?;
    let _ = ctx.nudge.send(());
    Ok(())
}

async fn try_run_step(ctx: &Ctx, due: &DueStep) -> Result<()> {
    let job = ctx.store.load_job(due.job).await?;
    if !job.can_start() {
        // Paused/cancelled between arming and firing — don't run.
        return Ok(());
    }
    let step = job
        .graph
        .steps
        .get(&due.step)
        .with_context(|| format!("job {} has no step {:?}", due.job, due.step))?;

    // §3.4 Case 1, enforced at fire time: an entry this far past its frozen
    // target expired while the machine was off or asleep. RunAsap (default)
    // just proceeds; Abandon marks the run Missed. Checking here rather
    // than only at startup means suspend/resume without a daemon restart
    // gets the same policy as a reboot.
    let missed_policy = step.missed_wait.unwrap_or(job.policies.missed_wait);
    let now = ctx.clock.now();
    if missed_policy == MissedWait::Abandon && now.duration_since(due.at) > MISSED_GRACE {
        let drained = ctx
            .store
            .mark_missed(
                due.job,
                due.run,
                &job.graph.entry,
                &now,
                job.hooks.on_missed.as_ref(),
            )
            .await?;
        let _ = ctx.nudge.send(());
        // Abandoned is still ended, and the §4.2 queue was released by the
        // same commit.
        if let Some((run, at)) = drained {
            let _ = ctx.arm.send(Arm::Step(DueStep {
                job: due.job,
                run,
                step: job.graph.entry.clone(),
                at,
            }));
        }
        return Ok(());
    }

    // §3.2: the deadline is measured from the *run's* start, not this
    // step's — "a late catch-up start doesn't eat the budget". Read before
    // the claim, so it is `None` on a run's first step and `Some` on every
    // step after it.
    let budget = job.policies.deadline;
    let started_at = match budget {
        Some(_) => ctx.store.run_started_at(due.job, due.run).await?,
        None => None,
    };

    // The budget can run out *between* steps, while the run sits on a
    // sleep-edge. Checked before the claim, so a step that will never run
    // leaves no attempt behind saying it did.
    if let (Some(budget), Some(started)) = (budget, started_at.as_ref())
        && started.checked_add(budget).is_ok_and(|at| now >= at)
    {
        return end_on_deadline(ctx, &job, due, None, &now).await;
    }

    // §3.3 step 1: persist intent, then act. None = the cursor moved on
    // (a stale or duplicate heap entry) — nothing to do, and no double
    // spawn.
    let Some(attempt) = ctx
        .store
        .begin_step_checked(
            due.job,
            due.run,
            &due.step,
            &now,
            job.approval.as_ref().map(|a| a.definition_hash),
        )
        .await?
    else {
        return Ok(());
    };
    // Everything below is a post-claim write, and each one has to prove this
    // attempt still owns the cursor before it moves the run (§3.3).
    let claim = Claim {
        step: &due.step,
        attempt,
    };
    crate::testhook::checkpoint("claimed", due.job, due.run, attempt).await;

    // The claim is what starts a run's clock, so a first step's budget runs
    // from the `now` it was just stamped with — no second read needed.
    let deadline_at =
        budget.and_then(|budget| started_at.as_ref().unwrap_or(&now).checked_add(budget).ok());

    let (outcome, enqueue) = match &step.action {
        Action::Shell { argv } => {
            // §2.3: registered before the spawn, so a `cued cancel` racing
            // this step can always reach it. Registering after would leave a
            // window where the process exists and nothing can kill it.
            let cancel = Cancel::new();
            let grace = step.kill_grace.unwrap_or(ctx.config.policy.kill_grace);
            let request = ExecRequest {
                argv: argv.clone(),
                cwd: step.cwd.clone().unwrap_or_else(|| job.cwd.clone()),
                env: merged_env(&job, step),
                timeout: step.timeout,
                kill_grace: grace,
                log_file: ctx.paths.step_log(due.job, due.run, &due.step, attempt),
                cancel: cancel.signal_handle(),
            };
            register_live(
                &ctx.live,
                due.job,
                due.run,
                attempt,
                cancel.clone(),
                deadline_at,
                Duration::try_from(grace).unwrap_or(Duration::ZERO),
            );
            let ownership = AttemptOwnership::new(
                Arc::clone(&ctx.live),
                due.job,
                due.run,
                attempt,
                cancel.clone(),
            );
            crate::testhook::checkpoint("registered", due.job, due.run, attempt).await;

            // A shutdown that began while this step was being claimed took
            // its registry snapshot before the entry above existed, so
            // nothing signalled it. Spawning now would leave a process the
            // daemon can no longer reach and is about to stop waiting for.
            // `stopping` is set before that snapshot, so seeing it false
            // here means the snapshot has not been taken yet — and by then
            // the registration above is visible to it.
            if ctx.stopping.load(std::sync::atomic::Ordering::SeqCst) {
                return Ok(());
            }

            // §2.3, the window the registry alone can't cover: the claim is
            // a store commit and the registration is in memory, so for a
            // moment between them a `cued cancel` finds no handle. Looking
            // again *after* registering closes it from the other side — a
            // cancel that landed in the gap has already moved the cursor,
            // and one landing from here on has a handle to reach.
            let current = after_claim(ctx, due, claim, "confirming the claim of", || {
                ctx.store.claim_is_current_checked(
                    due.job,
                    due.run,
                    claim,
                    job.approval.as_ref().map(|a| a.definition_hash),
                )
            })
            .await;
            if current != Some(true) {
                return Ok(());
            }
            crate::testhook::checkpoint("confirmed", due.job, due.run, attempt).await;

            // §2.3: the in-task timer is the primary trigger; the tick's
            // registry sweep is the backstop for a suspend it slept through.
            // Both end in the same §2.2 TERM → grace → KILL, because both go
            // through the one cancellation handle.
            let running = run_registered_attempt(ctx.spawner, ownership, request);
            tokio::pin!(running);
            let result = match deadline_at.as_ref() {
                Some(at) => {
                    let remaining = at.duration_since(ctx.clock.now());
                    let remaining = Duration::try_from(remaining).unwrap_or(Duration::ZERO);
                    tokio::select! {
                        outcome = &mut running => outcome,
                        _ = tokio::time::sleep(remaining) => {
                            cancel.signal();
                            // Let the kill sequence finish and the child be
                            // reaped, rather than abandoning the future
                            // mid-teardown.
                            (&mut running).await
                        }
                    }
                }
                None => (&mut running).await,
            };
            crate::testhook::checkpoint("exited", due.job, due.run, attempt).await;
            // §3.4 Case 2: a step the *shutdown* killed is left exactly as
            // an unclean kill would have left it — cursor still `Running`,
            // nothing closed. We stopped it mid-execution and cannot know
            // how far it got, which is the definition of that case, and the
            // §3.5 on_hold message it produces already says "machine or
            // daemon restarted". Closing it here instead would write the run
            // up as `Failed` and fire `on_failure`, paging someone for a
            // maintenance restart — and would decide the run's fate without
            // consulting the job's `on_interrupt` policy, which is the
            // setting that exists to decide exactly this.
            if ctx.stopping.load(std::sync::atomic::Ordering::SeqCst) {
                return Ok(());
            }
            let outcome = match result {
                Ok(result) => StepOutcome {
                    success: result.exit_code == Some(0) && !result.timed_out && !result.cancelled,
                    exit_code: result.exit_code,
                    timed_out: result.timed_out,
                    stdout: result.stdout,
                    stderr: result.stderr,
                },
                // Spawn infrastructure failed (bad cwd, missing binary):
                // the step failed; the run's transitions decide what's next.
                Err(error) => StepOutcome {
                    exit_code: None,
                    timed_out: false,
                    success: false,
                    stdout: String::new(),
                    stderr: format!("cued: {error:#}"),
                },
            };
            (outcome, None)
        }
        // §3.2: a Notify step succeeds once durably enqueued — the insert
        // rides the finish_step transaction below.
        Action::Notify { title, body } => (
            StepOutcome {
                exit_code: None,
                timed_out: false,
                success: true,
                stdout: String::new(),
                stderr: String::new(),
            },
            Some(NotifySpec {
                title: title.clone(),
                body: body.clone(),
            }),
        ),
    };

    let ended_at = ctx.clock.now();

    // Whichever trigger fired — in-task timer, tick sweep, or the step
    // simply running long — the wall clock is the authority on whether the
    // budget is spent (§9).
    if deadline_at.as_ref().is_some_and(|at| ended_at >= *at) {
        after_claim(ctx, due, claim, "ending at the deadline", || {
            end_on_deadline(ctx, &job, due, Some(attempt), &ended_at)
        })
        .await;
        return Ok(());
    }

    // Both of these were decided by the commit inside `close_step`, so
    // there is no window here in which one has happened and the other has
    // not: arming is now only the heap catching up with the store.
    let Some(closed) = after_claim(ctx, due, claim, "closing", || {
        close_step(
            &ctx.store,
            &job,
            due.run,
            &due.step,
            step,
            attempt,
            &outcome,
            enqueue.clone(),
            &ended_at,
        )
    })
    .await
    else {
        return Ok(());
    };
    for due in [closed.next, closed.drained].into_iter().flatten() {
        let _ = ctx.arm.send(Arm::Step(due));
    }
    // The close may have enqueued a Notify payload or a terminal hook —
    // wake the delivery task rather than wait out the slow tick (§3.5).
    let _ = ctx.nudge.send(());
    Ok(())
}

fn register_live(
    live: &Registry,
    job: JobId,
    run: RunId,
    attempt: u32,
    cancel: Cancel,
    kill_at: Option<Timestamp>,
    kill_grace: Duration,
) {
    live.lock().expect("registry").insert(
        (job, run, attempt),
        LiveAttempt {
            cancel,
            kill_at,
            kill_grace,
        },
    );
}

/// Keep process execution alive independently of the scheduler task that
/// claimed it. A panic or abort in the scheduler task drops this waiter, and
/// `AttemptOwnership` asks the execution task to tear down; only that task
/// removes the registry entry, after `Spawner::run` has reaped the child group.
async fn run_registered_attempt(
    spawner: SystemSpawner,
    mut ownership: AttemptOwnership,
    request: ExecRequest,
) -> Result<crate::exec::ExecResult> {
    let live = Arc::clone(&ownership.live);
    let (job, run, attempt) = (ownership.job, ownership.run, ownership.attempt);
    let execution = tokio::spawn(async move {
        let result = spawner.run(request).await;
        unregister_live(&live, job, run, attempt);
        result
    });
    ownership.hand_off();

    match execution.await {
        Ok(result) => {
            ownership.disarm();
            result
        }
        Err(error) => Err(error.into()),
    }
}

/// Only this attempt's own entry — never whatever is under the run now.
fn unregister_live(live: &Registry, job: JobId, run: RunId, attempt: u32) {
    live.lock().expect("registry").remove(&(job, run, attempt));
}

/// §2.3: reach the named runs' process groups, if any are still holding one.
/// Runs that aren't executing right now (waiting between steps, parked in
/// Held, or a firing that never started) simply aren't in the registry —
/// their cancellation was the store write, and there is nothing to kill.
fn cancel_live(live: &Registry, job: JobId, runs: &[RunId]) -> usize {
    let registry = live.lock().expect("registry");
    registry
        .iter()
        .filter(|((entry_job, entry_run, _), _)| *entry_job == job && runs.contains(entry_run))
        // Every live attempt of the run, not just the newest: an earlier one
        // still inside its teardown is holding a process group too.
        .map(|(_, attempt)| attempt.cancel.signal())
        .count()
}

/// §2.3's suspend backstop: the in-task deadline timer counts monotonic
/// time, which stops while the machine is suspended, so a run that slept
/// through its own deadline would keep going for however long it was out.
/// The tick compares the frozen wall-clock instants instead — §9's "durations
/// count elapsed real time; suspend counts", with no carve-out.
fn sweep_deadlines(live: &Registry, now: &Timestamp) {
    let registry = live.lock().expect("registry");
    for attempt in registry.values() {
        if attempt.kill_at.as_ref().is_some_and(|at| at <= now) {
            attempt.cancel.signal();
        }
    }
}

/// What closing a step leaves for the heap: the run's next step if it has
/// one, and a firing the §4.2 queue released if the run ended.
#[derive(Debug, Default)]
struct Closed {
    next: Option<DueStep>,
    drained: Option<DueStep>,
}

/// The shared tail of a step's execution — §3.3 step 2 plus everything that
/// rides it: choose the edge, freeze the next cursor, attach terminal
/// lifecycle hooks (§3.5), commit, and hand back the heap entry if the run
/// stays live. Used by the normal fire path and by reconciliation's
/// `on_interrupt = Fail` (which closes a killed attempt the same way).
#[allow(clippy::too_many_arguments)] // one call shape, two call sites
async fn close_step(
    store: &Store,
    job: &Job,
    run: RunId,
    step_id: &str,
    step: &Step,
    attempt: u32,
    outcome: &StepOutcome,
    enqueue: Option<NotifySpec>,
    ended_at: &Timestamp,
) -> Result<Closed> {
    let (edge, effect) = choose_edge(step, outcome);
    let next = resolve_effect(store, job, run, effect, ended_at).await?;

    // §3.5: a Notify action's payload plus any terminal hook, all durably
    // enqueued in the same transaction that closes the step.
    let mut notifications: Vec<NotifySpec> = enqueue.into_iter().collect();
    if let NextCursor::Terminal { status, .. } = &next {
        let hook = match status {
            RunStatus::Done => job.hooks.on_success.as_ref(),
            RunStatus::Failed => job.hooks.on_failure.as_ref(),
            _ => None,
        };
        notifications.extend(hook.cloned());
    }

    let closed = store
        .finish_step_checked(
            StepClose {
                job: job.id,
                entry_step: &job.graph.entry,
                run,
                step: step_id,
                attempt,
                ended_at,
                exit_code: outcome.exit_code,
                timed_out: outcome.timed_out,
                outcome_edge: edge,
                next: next.clone(),
                notifications,
            },
            job.approval.as_ref().map(|a| a.definition_hash),
        )
        .await?;

    if !closed.advanced {
        // The run went terminal underneath us — `cued cancel` (§4.2). The
        // attempt is recorded; the run is not ours to walk forward.
        return Ok(Closed::default());
    }

    Ok(Closed {
        next: match next {
            NextCursor::Waiting { step, at } => Some(DueStep {
                job: job.id,
                run,
                step,
                at,
            }),
            NextCursor::Terminal { .. } => None,
        },
        // Released by the same commit that ended the run (§4.2).
        drained: closed.drained.map(|(run, at)| DueStep {
            job: job.id,
            run,
            step: job.graph.entry.clone(),
            at,
        }),
    })
}

fn merged_env(job: &Job, step: &Step) -> BTreeMap<String, String> {
    let mut env = job.env.vars.clone();
    if let Some(overrides) = &step.env {
        env.extend(overrides.clone());
    }
    env
}

/// §3.2: edges are ordered, first match wins; no match → End derived from
/// the step (exit 0 → Success; nonzero / timeout / signal → Failure).
fn choose_edge(step: &Step, outcome: &StepOutcome) -> (Option<u32>, Effect) {
    for (index, transition) in step.transitions.iter().enumerate() {
        if condition_matches(&transition.when, outcome) {
            return (Some(index as u32), transition.then.clone());
        }
    }
    let derived = if outcome.success {
        Outcome::Success
    } else {
        Outcome::Failure
    };
    (None, Effect::End { outcome: derived })
}

fn condition_matches(condition: &Condition, outcome: &StepOutcome) -> bool {
    match condition {
        Condition::Always => true,
        // A signal-killed step has no exit code (§2.2) — it matches no
        // exit-shaped condition, only Failed / TimedOut / Always.
        Condition::ExitEq(code) => outcome.exit_code == Some(*code),
        Condition::ExitNe(code) => outcome.exit_code.is_some_and(|c| c != *code),
        Condition::ExitIn(codes) => outcome.exit_code.is_some_and(|c| codes.contains(&c)),
        Condition::Stdout(matcher) => output_matches(matcher, &outcome.stdout),
        Condition::Stderr(matcher) => output_matches(matcher, &outcome.stderr),
        Condition::TimedOut => outcome.timed_out,
        Condition::Succeeded => outcome.success,
        Condition::Failed => !outcome.success,
        Condition::All(inner) => inner.iter().all(|c| condition_matches(c, outcome)),
    }
}

fn output_matches(matcher: &OutputMatch, text: &str) -> bool {
    match matcher {
        OutputMatch::Contains(needle) => text.contains(needle),
        // TODO(§6.3): pre-compile (and thereby validate) regexes at submit;
        // until then a bad pattern simply never matches.
        OutputMatch::Regex(pattern) => regex::Regex::new(pattern)
            .map(|re| re.is_match(text))
            .unwrap_or(false),
    }
}

/// Turn the chosen effect into the next persisted cursor: End → terminal
/// status; Goto → the §3.2 frozen absolute wait target, gated by max_visits.
async fn resolve_effect(
    store: &Store,
    job: &Job,
    run: RunId,
    effect: Effect,
    ended_at: &Timestamp,
) -> Result<NextCursor> {
    match effect {
        Effect::End { outcome } => Ok(NextCursor::Terminal {
            status: match outcome {
                Outcome::Success => RunStatus::Done,
                Outcome::Failure => RunStatus::Failed,
            },
            fail_reason: None,
        }),
        Effect::Goto {
            step: target,
            after,
        } => {
            let target_step = job
                .graph
                .steps
                .get(&target)
                .with_context(|| format!("goto target {target:?} missing"))?;
            // v = the target's visit count so far in this run — the counter
            // both max_visits and Backoff read (§3.2).
            let visits = store.visit_count(job.id, run, &target).await?;
            if let Some(max) = target_step.max_visits
                && visits >= max
            {
                // §3.2: an exhausted loop bound is a fully known outcome —
                // unhandled failure, not Held.
                return Ok(NextCursor::Terminal {
                    status: RunStatus::Failed,
                    fail_reason: Some("max_visits".into()),
                });
            }

            // Freeze the absolute target now (§3.2) — never a countdown.
            let at = match after {
                None => *ended_at,
                Some(Wait::In(duration)) => ended_at
                    .checked_add(duration)
                    .context("wait target out of range")?,
                Some(Wait::Until(instant)) => instant,
                Some(Wait::Backoff { start, factor, max }) => {
                    let exponent = visits.saturating_sub(1);
                    let scaled = start.as_secs_f64() * factor.powi(exponent as i32);
                    let delay = SignedDuration::try_from_secs_f64(scaled)
                        .unwrap_or(max)
                        .min(max);
                    ended_at
                        .checked_add(delay)
                        .context("backoff target out of range")?
                }
            };
            Ok(NextCursor::Waiting { step: target, at })
        }
    }
}

// ---------------------------------------------------------------------------
// Recurring firings (§4.2)
// ---------------------------------------------------------------------------

/// Fire one recurring instant, and — the point of `retry` — make sure an
/// internal failure can't quietly cost the *schedule* its place on the heap.
///
/// This is the same hazard §3.3 describes for a due step, and the same fix:
/// the heap entry was popped before this task was spawned, so returning
/// early on an error would leave the job with a `next_fire_at` nothing holds
/// an arm for. It would then fire never — until a restart, where
/// `recurring_arms` rebuilds the heap from the store.
///
/// A recurring job makes it the worse of the two cases: a stranded step
/// loses one run, a stranded firing loses every run from here on.
///
/// Re-claiming is safe to do blindly because `record_firing` claims
/// `next_fire_at` conditionally: a retry that follows a pass which actually
/// committed finds the instant already consumed and does nothing.
async fn fire_job_task(ctx: Arc<Ctx>, job: JobId, claiming: Timestamp, retry: u32) {
    let Err(error) = fire_job(&ctx.store, job, &claiming, &ctx.clock.now())
        .await
        .map(|arms| {
            for arm in arms {
                let _ = ctx.arm.send(arm);
            }
        })
    else {
        return;
    };
    eprintln!(
        "cued: internal error firing {job} (try {}): {error:#}",
        retry + 1
    );

    let next = retry + 1;
    if next < STEP_RETRY_BUDGET {
        let delay = STEP_RETRY_BACKOFF * 2i32.saturating_pow(retry);
        if let Ok(at) = ctx.clock.now().checked_add(delay) {
            eprintln!("cued: re-arming {job}'s firing in {delay:#}");
            let _ = ctx.arm.send(Arm::RetryFire {
                job,
                claiming,
                at,
                retry: next,
            });
            return;
        }
    }

    // The budget is gone. There is no run to park in `Held` — the firing
    // never produced one — so the honest thing is to say so where the user
    // will see it, rather than let a schedule go quiet. The job stays
    // `Active` with its `next_fire_at` intact, so a daemon restart picks it
    // back up (§5.3: the heap is derived state, the store is the truth).
    let spec = NotifySpec {
        title: format!("cued: {job} stopped firing"),
        body: format!(
            "the daemon failed {STEP_RETRY_BUDGET} times to fire the run due at {}.              Its schedule is still on record — restarting the daemon re-arms it.",
            claiming.strftime("%Y-%m-%d %H:%M:%S")
        ),
    };
    match ctx
        .store
        .enqueue_job_notification(job, &spec, &ctx.clock.now())
        .await
    {
        Ok(()) => {
            let _ = ctx.nudge.send(());
        }
        // The store is the thing that's broken, so there is nowhere left to
        // record this but the log.
        Err(error) => eprintln!("cued: could not warn about {job}: {error:#}"),
    }
}

/// A recurring job's firing instant arrived — possibly long ago. Applies
/// `catch_up` to the instants that piled up while nobody was looking, then
/// `overlap` to the one firing that survives, records the whole outcome in
/// one transaction (re-arming at fire time, §4.2), and returns the arms.
/// Public so the recurrence tests can drive it like `reconcile`.
pub async fn fire_job(
    store: &Store,
    job_id: JobId,
    scheduled: &Timestamp,
    now: &Timestamp,
) -> Result<Vec<Arm>> {
    let job = store.load_job(job_id).await?;
    if !job.can_start() {
        // Paused/cancelled/finished between arming and firing: stop
        // re-arming — resume recomputes from now, never back-fills (§4.2).
        return Ok(Vec::new());
    }
    let entry = job.graph.entry.clone();

    // §4.1's firing budget, spent before anything is selected. `fired_count`
    // measures firings on record — a coalesced skip row counts its whole
    // range — so this is how many more the schedule is entitled to.
    let fired = store.fired_count(job_id).await?;
    let remaining = job.schedule.count().map(|cap| cap.saturating_sub(fired));

    if remaining == Some(0) {
        // The arithmetic's floor, not the main guard: an exhausted schedule
        // re-arms to None, so the claim in `record_firing` normally refuses
        // a firing before it gets this far. Kept because reaching here with
        // no budget and firing anyway is the one outcome worth ruling out
        // twice — and because it keeps `due_instants` from being handed a
        // limit of zero, which its own arithmetic would round back up to one
        // instant. Claim the instant so the job stops being armed, and
        // record no firing at all.
        store
            .record_firing_checked(
                Firing {
                    job: job_id,
                    entry_step: &entry,
                    claiming: scheduled,
                    // Nothing left to spend, so nothing is spent.
                    consumed: 0,
                    run_at: None,
                    skipped: None,
                    queue_at: None,
                    next_fire_at: None,
                    now,
                },
                job.approval.as_ref().map(|a| a.definition_hash),
            )
            .await?;
        return Ok(Vec::new());
    }

    // Everything due: the armed instant plus any the machine slept
    // through, truncated to the budget. `latest` is the candidate firing;
    // catch_up decides what happens to the `count - 1` before it.
    let (latest, previous, count) = due_instants(&job.schedule, scheduled, now, remaining)?;
    let overdue = now.duration_since(latest) > MISSED_GRACE;

    let mut skipped: Option<(Option<Timestamp>, Timestamp, u32)> = None;
    let candidate = if count == 1 && !overdue {
        Some(latest) // the ordinary on-time firing
    } else {
        match job.policies.catch_up {
            // §4.2 RunOnce (default): ONE catch-up run for the most recent
            // missed instant; the earlier ones compact into a single
            // skipped-range row — a week of downtime is one backup, not
            // seven, and never ten thousand rows.
            CatchUp::RunOnce => {
                if count > 1 {
                    let range_end = previous.expect("count > 1 has a previous instant");
                    skipped = Some((Some(*scheduled), range_end, count - 1));
                }
                Some(latest)
            }
            // §4.2 Skip: "only valuable at its moment" — record and move on.
            CatchUp::Skip => {
                skipped = Some(((count > 1).then_some(*scheduled), latest, count));
                None
            }
        }
    };

    // §4.2 overlap: the previous run may still be live (running, waiting,
    // or held). Skip records the firing; Queue coalesces it into the one
    // pending slot that starts when the live run ends.
    let mut run_at = None;
    let mut queue_at = None;
    if let Some(instant) = candidate {
        if store.has_live_run(job_id).await? {
            match job.policies.overlap {
                Overlap::Skip => {
                    skipped = Some(match skipped {
                        Some((from, _, n)) => (from, instant, n + 1),
                        None => (None, instant, 1),
                    });
                }
                Overlap::Queue => queue_at = Some(instant),
            }
        } else {
            run_at = Some(instant);
        }
    }

    // Re-arm target. `count` is already inside the budget, so this is just
    // "did that spend the last of it". (An instant parked in the queue slot
    // isn't a row yet, so `fired_count` can lag by one — the cap may
    // overshoot by a firing in that window; it still terminates.)
    let next = match remaining {
        Some(remaining) if count >= remaining => None,
        _ => schedule::next_fire(&job.schedule, now)?,
    };

    let created = store
        .record_firing_checked(
            Firing {
                job: job_id,
                entry_step: &entry,
                // §4.2: the instant this arm is claiming. The store re-arms only
                // if it still expects exactly this one.
                claiming: scheduled,
                // Every due instant this firing covers is a firing, however it
                // ends up recorded — run, skip row, or coalesced into the queue.
                consumed: count,
                run_at: run_at.as_ref(),
                skipped: skipped
                    .as_ref()
                    .map(|(from, to, n)| (from.as_ref(), to, *n)),
                queue_at: queue_at.as_ref(),
                next_fire_at: next.as_ref(),
                now,
            },
            job.approval.as_ref().map(|a| a.definition_hash),
        )
        .await?;

    // The claim failed: this job was cancelled or paused underneath us, or
    // another heap entry for this same instant got here first. Emitting
    // arms anyway is exactly how a duplicate compounds (§4.2).
    let Fired::Recorded {
        run: created,
        drained,
    } = created
    else {
        return Ok(Vec::new());
    };

    let mut arms = Vec::new();
    if let (Some(run), Some(at)) = (created, run_at) {
        arms.push(Arm::Step(DueStep {
            job: job_id,
            run,
            step: entry.clone(),
            at,
        }));
    }
    // The live run may have ended between the overlap check and the commit;
    // the drain rides inside `record_firing`'s transaction, so that race is
    // settled there rather than in a second commit this function could fail
    // between.
    if let Some((run, at)) = drained {
        arms.push(Arm::Step(DueStep {
            job: job_id,
            run,
            step: entry,
            at,
        }));
    }
    if let Some(at) = next {
        arms.push(Arm::Fire { job: job_id, at });
    }
    Ok(arms)
}

// ---------------------------------------------------------------------------
// Startup reconciliation (§3.4)
// ---------------------------------------------------------------------------

/// Resolve every non-terminal run the store holds. "Machine died
/// mid-workflow" is two problems with different safety profiles:
///
/// - **Case 2 — cursor `Running`**: the step's process was killed with the
///   daemon; its side effects are unknown. Governed by `on_interrupt`:
///   `Hold` (default) parks it and enqueues the on_hold notification, `Fail`
///   closes the killed attempt as a failure and lets the step's own
///   transitions route it, `Retry` re-arms it — but only when the step
///   opted in via `restart_safe`, else it degrades to Hold.
/// - **Case 1 — cursor `Waiting`**: between steps, nothing half-done; always
///   re-armed here. The `missed_wait` policy is applied at fire time (see
///   `try_run_step`), so suspend-without-restart gets the same handling.
///
/// Returns the heap entries to arm. Public so the §11 crash-point tests can
/// drive it directly against a store left mid-write.
pub async fn reconcile(store: &Store, now: &Timestamp) -> Result<Vec<Arm>> {
    store.expire_pending(now).await?;
    store.finish_exhausted_jobs().await?;
    // Case 1 snapshot FIRST: Case-2 handling below moves cursors to Waiting
    // (Fail-with-recovery-edge, Retry), and taking the waiting set afterward
    // would arm those runs twice — a double-spawn.
    let mut due: Vec<Arm> = store
        .waiting_runs()
        .await?
        .into_iter()
        .map(Arm::Step)
        .collect();

    // The recurring half of the heap (§5.3): every active job's re-arm
    // target. Overdue targets get the catch_up treatment at fire time.
    for (job, at) in store.recurring_arms().await? {
        due.push(Arm::Fire { job, at });
    }

    for (job_id, run, step_id) in store.interrupted_runs().await? {
        let job = store.load_job(job_id).await?;
        if !job.approval_valid() {
            continue;
        }
        let Some(step) = job.graph.steps.get(&step_id) else {
            // Submit validated the graph, so a cursor pointing nowhere is
            // corruption — park it for a human rather than guess.
            let notify = on_hold_message(&job, &step_id, HeldReason::Interrupted);
            store
                .hold_run(
                    job_id,
                    run,
                    &step_id,
                    HeldReason::Interrupted,
                    &notify,
                    now,
                    None,
                )
                .await?;
            continue;
        };

        // §3.4: Retry is only sound for idempotent steps, so it is gated
        // per step; on a step that never opted in, it means Hold.
        let policy = match job.policies.on_interrupt {
            OnInterrupt::Retry if !step.restart_safe => OnInterrupt::Hold,
            policy => policy,
        };
        match policy {
            OnInterrupt::Hold => {
                let notify =
                    job.hooks.on_hold.clone().unwrap_or_else(|| {
                        on_hold_message(&job, &step_id, HeldReason::Interrupted)
                    });
                store
                    .hold_run(
                        job_id,
                        run,
                        &step_id,
                        HeldReason::Interrupted,
                        &notify,
                        now,
                        None,
                    )
                    .await?;
            }
            OnInterrupt::Fail => {
                // Close the killed attempt as a step failure (no exit code,
                // like death by signal) and run its failure handling —
                // fail-fast unless the graph wrote a recovery edge.
                let attempt = store.latest_attempt(job_id, run, &step_id).await?;
                let outcome = StepOutcome {
                    exit_code: None,
                    timed_out: false,
                    success: false,
                    stdout: String::new(),
                    stderr: String::new(),
                };
                let closed = close_step(
                    store, &job, run, &step_id, step, attempt, &outcome, None, now,
                )
                .await?;
                due.extend(
                    [closed.next, closed.drained]
                        .into_iter()
                        .flatten()
                        .map(Arm::Step),
                );
            }
            OnInterrupt::Retry => {
                // No epoch bump: automatic retries keep counting against
                // max_visits, so a crash loop still hits a bound (§3.4).
                store.rearm_interrupted(job_id, run, &step_id, now).await?;
                due.push(Arm::Step(DueStep {
                    job: job_id,
                    run,
                    step: step_id,
                    at: *now,
                }));
            }
        }
    }

    // Last, because the loop above can itself end a run: an `on_interrupt =
    // Fail` with no recovery edge closes it terminally, freeing a firing
    // that was queued behind it (§4.2). Draining before that ran — as this
    // used to — left the slot filled until some *later* run happened to end.
    // It also covers the plain case of a run that ended while the daemon was
    // down, with nothing left to notice.
    for job in store.queued_jobs().await? {
        let entry = store.load_job(job).await?.graph.entry;
        if let Some((run, at)) = store.drain_queued(job, &entry).await? {
            due.push(Arm::Step(DueStep {
                job,
                run,
                step: entry,
                at,
            }));
        }
    }

    Ok(due)
}

/// The §3.5 default on_hold message — overridable per job via hooks. The
/// reason is spelled out in the body: "interrupted" and "the daemon kept
/// failing" want different things from the reader.
fn on_hold_message(job: &Job, step: &str, reason: HeldReason) -> NotifySpec {
    let id = job.id;
    let label = match &job.name {
        Some(name) => format!("{id} \"{name}\""),
        None => id.to_string(),
    };
    NotifySpec {
        title: format!("cued: job {label} paused"),
        body: format!(
            "step '{step}' was interrupted ({}) and may be half-done.\n\
             \x20 cued logs {id}      # inspect\n\
             \x20 cued continue {id}  # resume from here\n\
             \x20 cued retry {id}     # re-run the step",
            reason.describe()
        ),
    }
}

// ---------------------------------------------------------------------------
// The control socket (§5.1, §7.3)
// ---------------------------------------------------------------------------

async fn accept_loop(ctx: Arc<Ctx>, listener: UnixListener) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                // §7.3: ask the kernel who's connected — same uid or hang
                // up. The client sends no identity claim.
                let authorized = stream
                    .peer_cred()
                    .map(|cred| cred.uid() == unsafe { libc::getuid() })
                    .unwrap_or(false);
                if !authorized {
                    continue;
                }
                tokio::spawn(handle_connection(Arc::clone(&ctx), stream));
            }
            Err(error) => eprintln!("cued: accept failed: {error}"),
        }
    }
}

async fn handle_connection(ctx: Arc<Ctx>, stream: UnixStream) {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let response = dispatch(&ctx, &line).await;
        if matches!(response, Response::Subscribed) {
            stream_changes(&ctx, lines, write).await;
            return;
        }
        let Ok(mut payload) = serde_json::to_string(&response) else {
            break;
        };
        payload.push('\n');
        let written = write.write_all(payload.as_bytes()).await;
        if matches!(response, Response::Upgrading { .. }) {
            ctx.reply_written.notify_one();
        }
        if written.is_err() {
            break;
        }
    }
}

/// §5.1 Subscribe: `Subscribed`, then a `Changed` per store change (several
/// close together arrive as one), until the client hangs up or writes again.
/// Holds no `WorkToken`: a subscriber stays connected indefinitely, and an
/// upgrade waits for every token to drop. Its exec closes this socket, and
/// the subscriber reconnects to the new image.
async fn stream_changes(
    ctx: &Ctx,
    mut lines: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    mut write: tokio::net::unix::OwnedWriteHalf,
) {
    // Before the reply: whatever commits once the client has it is told.
    let mut changes = ctx.store.subscribe();
    let mut notice = Response::Subscribed;
    loop {
        let Ok(mut payload) = serde_json::to_string(&notice) else {
            return;
        };
        payload.push('\n');
        if write.write_all(payload.as_bytes()).await.is_err() {
            return;
        }
        tokio::select! {
            changed = changes.changed() => {
                if changed.is_err() {
                    return; // the store is gone: the daemon is stopping
                }
                changes.borrow_and_update();
                notice = Response::Changed;
            }
            // EOF, an error, or a line: the stream takes no requests.
            _ = lines.next_line() => return,
        }
    }
}

async fn dispatch(ctx: &Ctx, line: &str) -> Response {
    let request: Request = match serde_json::from_str(line) {
        Ok(request) => request,
        Err(error) => {
            return Response::Error {
                message: format!(
                    "bad request: {error} — client and daemon may be different builds; stop the running `cued daemon` and rerun; the next command respawns it"
                ),
            };
        }
    };
    // Ahead of the proto check, by design: see `RequestBody::Upgrade`.
    if let RequestBody::Upgrade { wait_secs, force } = request.body {
        // Any same-uid client can send this, so bound it here rather than
        // trust the CLI's check: an unbounded wait overflows the deadline.
        let wait = Duration::from_secs(wait_secs.min(crate::proto::MAX_UPGRADE_WAIT_SECS));
        return request_upgrade(ctx, wait, force).await;
    }
    if request.proto != PROTO_VERSION {
        return Response::ProtoMismatch {
            daemon_proto: PROTO_VERSION,
        };
    }
    // Ahead of the work token: the stream that follows is long-lived.
    if let RequestBody::Subscribe = request.body {
        return Response::Subscribed;
    }
    // An upgrade waits for this to finish before it execs.
    let _work = WorkToken::new(ctx);
    match request.body {
        RequestBody::Upgrade { .. } | RequestBody::Subscribe => unreachable!("answered above"),
        RequestBody::Ping => Response::Pong {
            proto: PROTO_VERSION,
        },
        RequestBody::Submit { spec } => {
            match handle_submit(ctx, *spec, crate::model::JobSource::Cli, false).await {
                Ok((job, run)) => Response::Submitted {
                    job,
                    run,
                    pending_approval: false,
                },
                Err(error) => Response::Error {
                    message: format!("{error:#}"),
                },
            }
        }
        RequestBody::SubmitDefinition {
            spec,
            source,
            require_approval,
        } => match handle_submit(ctx, *spec, source, require_approval).await {
            Ok((job, run)) => Response::Submitted {
                job,
                run,
                pending_approval: require_approval,
            },
            Err(error) => Response::Error {
                message: format!("{error:#}"),
            },
        },
        RequestBody::Approve {
            job,
            definition_hash,
        } => match handle_approve(ctx, &job, definition_hash).await {
            Ok(job) => Response::Approved { job },
            Err(error) => Response::Error {
                message: format!("{error:#}"),
            },
        },
        RequestBody::List { all } => match handle_list(ctx, all).await {
            Ok(jobs) => Response::JobList { jobs },
            Err(error) => Response::Error {
                message: format!("{error:#}"),
            },
        },
        RequestBody::Continue { job } => match handle_continue(ctx, &job).await {
            Ok((job, run, step)) => Response::Rearmed { job, run, step },
            Err(error) => Response::Error {
                message: format!("{error:#}"),
            },
        },
        RequestBody::Retry { job, from } => match handle_retry(ctx, &job, from).await {
            Ok((job, run, step)) => Response::Rearmed { job, run, step },
            Err(error) => Response::Error {
                message: format!("{error:#}"),
            },
        },
        RequestBody::Pause { job } => match handle_pause(ctx, &job).await {
            Ok(job) => Response::Paused { job },
            Err(error) => Response::Error {
                message: format!("{error:#}"),
            },
        },
        RequestBody::Resume { job } => match handle_resume(ctx, &job).await {
            Ok((job, next_at)) => Response::Resumed { job, next_at },
            Err(error) => Response::Error {
                message: format!("{error:#}"),
            },
        },
        RequestBody::Cancel { job } => match handle_cancel(ctx, &job).await {
            Ok((job, runs)) => Response::JobCancelled { job, runs },
            Err(error) => Response::Error {
                message: format!("{error:#}"),
            },
        },
        RequestBody::Gc => match collect_garbage(
            &ctx.store,
            &ctx.paths,
            &ctx.config.retention,
            &ctx.clock.now(),
        )
        .await
        {
            Ok(outcome) => Response::Collected {
                runs: outcome.runs.len() as u32,
                jobs: outcome.jobs.len() as u32,
            },
            Err(error) => Response::Error {
                message: format!("{error:#}"),
            },
        },
        RequestBody::Show { job } => match handle_show(ctx, &job).await {
            Ok((job, next_fire_at, queued_at)) => Response::JobDetail {
                job: Box::new(job),
                next_fire_at,
                queued_at,
            },
            Err(error) => Response::Error {
                message: format!("{error:#}"),
            },
        },
        RequestBody::Logs {
            job,
            run,
            step,
            attempt,
        } => match handle_logs(ctx, &job, run, step.as_deref(), attempt).await {
            Ok((job, run, attempts)) => Response::LogManifest { job, run, attempts },
            Err(error) => Response::Error {
                message: format!("{error:#}"),
            },
        },
        RequestBody::WaitAllowed => match crate::mcp::wait_policy(&ctx.paths) {
            Ok(_) => Response::WaitAllowed,
            Err(error) => Response::Error {
                message: format!("{error:#}"),
            },
        },
        RequestBody::Runs { job, run } => match handle_runs(ctx, &job, run).await {
            Ok(reply) => Response::JobRun(Box::new(reply)),
            Err(error) => Response::Error {
                message: format!("{error:#}"),
            },
        },
    }
}

/// Hand the main loop an upgrade and wait for its outcome — which, if it
/// goes ahead, is the last reply this image ever sends.
async fn request_upgrade(ctx: &Ctx, wait: Duration, force: bool) -> Response {
    let Some(upgrades) = &ctx.upgrades else {
        return Response::Error {
            message: "this daemon runs in-process and has no binary to re-execute".into(),
        };
    };
    if ctx.upgrading.swap(true, SeqCst) {
        return Response::Error {
            message: "an upgrade is already in progress".into(),
        };
    }
    let (reply, outcome) = tokio::sync::oneshot::channel();
    if upgrades.send(UpgradeOrder { wait, force, reply }).is_err() {
        ctx.upgrading.store(false, SeqCst);
        return Response::Error {
            message: "the daemon is shutting down".into(),
        };
    }
    outcome.await.unwrap_or_else(|_| Response::Error {
        message: "the daemon stopped before the upgrade finished".into(),
    })
}

/// §6: the canonical definition, exactly as stored. Rendering — human,
/// TOML, or JSON — is entirely the client's business.
async fn handle_show(
    ctx: &Ctx,
    reference: &str,
) -> Result<(Job, Option<Timestamp>, Option<Timestamp>)> {
    let job = ctx.store.resolve_job(reference).await?;
    ctx.store.expire_pending(&ctx.clock.now()).await?;
    ctx.store.job_detail(job).await
}

/// §2.1/§5.1: hand back *where* the output is and how each attempt went; the
/// client reads the files itself. The daemon stays out of the byte path
/// entirely, which is what keeps `-f` a plain tail.
async fn handle_logs(
    ctx: &Ctx,
    reference: &str,
    run: Option<i64>,
    step: Option<&str>,
    attempt: Option<u32>,
) -> Result<(JobId, RunId, Vec<crate::proto::LogAttempt>)> {
    let job = ctx.store.resolve_job(reference).await?;
    let (run, attempts) = ctx.store.log_manifest(job, run, step, attempt).await?;
    Ok((job, run, attempts))
}

/// `cued wait`'s poll, gated here rather than in the client: the daemon's
/// environment was fixed when it started, while the waiter's is whatever
/// shell asked. Reads only — never the writer. A lapsed approval is the
/// scheduler's to expire (it sweeps at least every `MAX_TICK`).
async fn handle_runs(
    ctx: &Ctx,
    reference: &str,
    query: crate::proto::RunQuery,
) -> Result<crate::proto::JobRun> {
    let policy = crate::mcp::wait_policy(&ctx.paths)?;
    let job = ctx.store.resolve_job(reference).await?;
    let (mut reply, pending) = ctx.store.job_run(job, query).await?;
    // Step results go only with a run the waiter is about to report, and
    // only while `logs` allows them.
    let wants_steps = |reply: &crate::proto::JobRun| {
        policy.steps
            && reply
                .run
                .as_ref()
                .is_some_and(|run| run.status.is_settled())
    };
    if !pending && !wants_steps(&reply) {
        return Ok(reply);
    }
    let definition = ctx.store.load_job(job).await?;
    if pending {
        // Nothing happens on its own before the approval deadline: let the
        // waiter nap toward it, rather than poll a job that may sit pending
        // for days. Approving is a command, noticed within a nap.
        reply.quiet_until = Some(definition.approval_deadline()?.0);
    }
    if wants_steps(&reply)
        && let Some(run) = &reply.run
    {
        let (_, attempts) = ctx
            .store
            .log_manifest(job, Some(run.id.0), None, None)
            .await?;
        reply.steps = Some(RunSteps {
            attempts,
            notify: definition.notify_steps(),
        });
    }
    Ok(reply)
}

/// §4.2 cancel: stop re-arming, mark every live run `Cancelled`, and reach
/// any that is actually executing through the §2.3 registry.
///
/// Store first, kill second. The store write is what `cued list` and a
/// restart both believe, so it has to land even if the process teardown
/// doesn't — and a heap entry armed for a run this just cancelled is
/// harmless, because `begin_step`'s claim won't find a `waiting` cursor to
/// take.
async fn handle_cancel(ctx: &Ctx, reference: &str) -> Result<(JobId, Vec<RunId>)> {
    let job = ctx.store.resolve_job(reference).await?;
    let runs = ctx
        .store
        .cancel_job_with_clock(job, || ctx.clock.now())
        .await?;
    let signalled = cancel_live(&ctx.live, job, &runs);
    if signalled > 0 {
        eprintln!("cued: cancel {job} — signalled {signalled} running step(s)");
    }
    Ok((job, runs))
}

/// §3.4: resume the latest run from its Held cursor, runnable now.
async fn handle_continue(ctx: &Ctx, reference: &str) -> Result<(JobId, RunId, StepId)> {
    let job = ctx.store.resolve_job(reference).await?;
    let (run, _, _) = ctx.store.latest_run_cursor(job).await?;
    let now = ctx.clock.now();
    let step = ctx.store.resume_held(job, run, &now).await?;
    let _ = ctx.arm.send(Arm::Step(DueStep {
        job,
        run,
        step: step.clone(),
        at: now,
    }));
    Ok((job, run, step))
}

/// §3.4: rewind the latest run in place. Default restart point: the held
/// step if there is one, else the graph's entry (a full re-run).
async fn handle_retry(
    ctx: &Ctx,
    reference: &str,
    from: Option<String>,
) -> Result<(JobId, RunId, StepId)> {
    let job_id = ctx.store.resolve_job(reference).await?;
    let job = ctx.store.load_job(job_id).await?;
    let (run, _, held_step) = ctx.store.latest_run_cursor(job_id).await?;
    let step = from
        .or(held_step)
        .unwrap_or_else(|| job.graph.entry.clone());
    ensure!(
        job.graph.steps.contains_key(&step),
        "job {job_id} has no step {step:?}"
    );
    let now = ctx.clock.now();
    ctx.store.rewind_run(job_id, run, &step, &now).await?;
    let _ = ctx.arm.send(Arm::Step(DueStep {
        job: job_id,
        run,
        step: step.clone(),
        at: now,
    }));
    Ok((job_id, run, step))
}

async fn handle_submit(
    ctx: &Ctx,
    spec: JobSpec,
    source: crate::model::JobSource,
    require_approval: bool,
) -> Result<(JobId, Option<RunId>)> {
    submit::validate(&spec)?; // the §6.3 gate, whatever the front-end
    let now = ctx.clock.now();
    let entry = spec.graph.entry.clone();
    let (job, run, at) = ctx
        .store
        .submit_definition(&spec, &now, source, require_approval)
        .await?;
    if !require_approval {
        let _ = ctx.arm.send(match run {
            // One-off: its single run exists already, parked on the first
            // sleep-edge (§3.2).
            Some(run) => Arm::Step(DueStep {
                job,
                run,
                step: entry,
                at,
            }),
            // Recurring: runs are created firing by firing (§4.2).
            None => Arm::Fire { job, at },
        });
    }
    let _ = ctx.nudge.send(());
    Ok((job, run))
}

async fn handle_approve(ctx: &Ctx, reference: &str, hash: [u8; 32]) -> Result<JobId> {
    let id = ctx.store.resolve_job(reference).await?;
    let (run, next) = ctx
        .store
        .approve_with_clock(id, hash, || ctx.clock.now())
        .await?;
    if next.is_none() {
        ctx.store.finish_exhausted_jobs().await?;
    }
    let job = ctx.store.load_job(id).await?;
    if job.can_start()
        && let Some(at) = next
    {
        let arm = match run {
            Some(run) => Arm::Step(DueStep {
                job: id,
                run,
                step: job.graph.entry,
                at,
            }),
            None => Arm::Fire { job: id, at },
        };
        let _ = ctx.arm.send(arm);
    }
    Ok(id)
}

/// §4.2 pause: nothing new starts — no firing, no next step. A step that is
/// already running is not touched; it finishes and is closed as usual, and
/// `try_run_step`'s status check keeps the run's next step from being claimed.
async fn handle_pause(ctx: &Ctx, reference: &str) -> Result<JobId> {
    let job = ctx.store.resolve_job(reference).await?;
    ctx.store.set_paused(job).await?;
    Ok(job)
}

/// §4.2 resume: re-arm to the next FUTURE instant only — never back-fills.
async fn handle_resume(ctx: &Ctx, reference: &str) -> Result<(JobId, Option<Timestamp>)> {
    let job_id = ctx.store.resolve_job(reference).await?;
    let job = ctx.store.load_job(job_id).await?;
    let now = ctx.clock.now();
    // A one-off's schedule lives on its run's waiting cursor, not on
    // next_fire_at — computing one would fire a duplicate run.
    let next = if !job.schedule.is_recurring() {
        None
    } else {
        match job.schedule.count() {
            Some(cap) if ctx.store.fired_count(job_id).await? >= cap => None,
            _ => schedule::next_fire(&job.schedule, &now)?,
        }
    };
    ctx.store.set_resumed(job_id, next.as_ref(), &now).await?;
    if let Some(at) = &next {
        let _ = ctx.arm.send(Arm::Fire {
            job: job_id,
            at: *at,
        });
    }
    // Re-arm any wait the pause left stranded (a paused one-off's pending
    // run, or a mid-workflow sleep-edge whose pop was swallowed while
    // paused). Stale duplicates are harmless — begin_step's cursor check.
    for due in ctx.store.waiting_runs().await? {
        if due.job == job_id {
            let _ = ctx.arm.send(Arm::Step(due));
        }
    }
    Ok((job_id, next))
}

async fn handle_list(ctx: &Ctx, all: bool) -> Result<Vec<JobEntry>> {
    let now = ctx.clock.now();
    ctx.store.expire_pending(&now).await?;
    let overviews = ctx.store.list_overview(all, &now).await?;
    Ok(overviews
        .into_iter()
        .map(|overview| JobEntry {
            id: overview.id,
            name: overview.name,
            status: overview.status,
            approval: overview.approval,
            source: overview.source,
            expired_at: overview.expired_at,
            expiry_reason: overview.expiry_reason,
            action: action_summary(&overview.graph),
            next_at: overview
                .last_run
                .as_ref()
                .and_then(|run| run.cursor_at)
                .or(overview.next_fire_at),
            last_run: overview.last_run.map(|run| RunEntry {
                id: run.id,
                status: run.status,
                ended_at: run.ended_at,
                fail_reason: run.fail_reason,
            }),
        })
        .collect())
}

/// One line of "what does this job do" for `cued list`: the entry step's
/// action, un-desugared where we can (§2.2's sh -c form reads back as the
/// original string).
fn action_summary(graph: &Graph) -> String {
    let action = match graph.steps.get(&graph.entry).map(|step| &step.action) {
        Some(Action::Shell { argv }) => match argv.as_slice() {
            [shell, flag, script] if shell == "/bin/sh" && flag == "-c" => script.clone(),
            argv => argv.join(" "),
        },
        Some(Action::Notify { title, .. }) => format!("notify: {title}"),
        None => "?".into(),
    };
    if graph.steps.len() > 1 {
        format!("{action} (+{} steps)", graph.steps.len() - 1)
    } else {
        action
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Transition;

    /// Cancellation after registration but before the spawner is polled is
    /// deterministic: it must be latched, so the real spawner declines to
    /// create the child rather than depending on a scheduling race.
    #[tokio::test]
    async fn cancel_after_registration_before_spawn_prevents_child() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let marker = dir.path().join("child-started");
        let log_file = dir.path().join("step.log");
        let (job, run, attempt) = (JobId(9), RunId(4), 2);
        let live = Registry::default();
        let cancel = Cancel::new();
        let request = ExecRequest {
            argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                format!("touch {}", marker.display()),
            ],
            cwd: dir.path().display().to_string(),
            env: std::collections::BTreeMap::new(),
            timeout: None,
            kill_grace: jiff::SignedDuration::from_millis(300),
            log_file,
            cancel: cancel.signal_handle(),
        };
        register_live(
            &live,
            job,
            run,
            attempt,
            cancel,
            None,
            Duration::from_millis(300),
        );

        assert_eq!(cancel_live(&live, job, &[run]), 1);
        let outcome = SystemSpawner.run(request).await?;

        assert!(
            outcome.cancelled,
            "the registered cancellation must be observed"
        );
        assert!(
            !marker.exists(),
            "the child must not start after cancellation"
        );
        unregister_live(&live, job, run, attempt);
        assert!(live.lock().expect("registry").is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn abort_before_spawn_removes_unowned_registry_entry() {
        let live = Registry::default();
        let (job, run, attempt) = (JobId(12), RunId(7), 1);
        let cancel = Cancel::new();
        let signal = cancel.signal_handle();
        register_live(
            &live,
            job,
            run,
            attempt,
            cancel.clone(),
            None,
            Duration::ZERO,
        );
        let ownership = AttemptOwnership::new(Arc::clone(&live), job, run, attempt, cancel);

        let task = tokio::spawn(async move {
            let _ownership = ownership;
            std::future::pending::<()>().await;
        });
        task.abort();
        assert!(
            task.await
                .expect_err("the waiter was aborted")
                .is_cancelled()
        );

        assert!(live.lock().expect("registry").is_empty());
        assert!(signal.is_cancelled(), "drop must latch cancellation");
    }

    fn register_long_running_attempt(
        dir: &std::path::Path,
        job: JobId,
        run: RunId,
        attempt: u32,
        live: &Registry,
    ) -> (Cancel, ExecRequest) {
        let cancel = Cancel::new();
        let marker = dir.join("child-pid");
        let request = ExecRequest {
            argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                format!(
                    "trap '' TERM; echo $$ > '{}'; exec sleep 300",
                    marker.display()
                ),
            ],
            cwd: dir.display().to_string(),
            env: std::collections::BTreeMap::new(),
            timeout: None,
            kill_grace: jiff::SignedDuration::from_millis(300),
            log_file: dir.join("step.log"),
            cancel: cancel.signal_handle(),
        };
        register_live(
            live,
            job,
            run,
            attempt,
            cancel.clone(),
            None,
            Duration::from_millis(300),
        );
        (cancel, request)
    }

    async fn wait_for_child_pid(marker: &std::path::Path) -> Result<i32> {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Ok(pid) = std::fs::read_to_string(marker)
                    && let Ok(pid) = pid.trim().parse()
                {
                    return pid;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .map_err(Into::into)
    }

    async fn wait_for_attempt_cleanup(live: &Registry, pid: i32) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let still_registered = !live.lock().expect("registry").is_empty();
                // SAFETY: kill(pid, 0) probes process existence and sends no signal.
                let process_exists = unsafe { libc::kill(pid, 0) } == 0;
                if !still_registered && !process_exists {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .map_err(Into::into)
    }

    async fn test_context(root: &std::path::Path) -> Result<Ctx> {
        let paths = Paths {
            data_dir: root.to_path_buf(),
            db_file: root.join("cued.db"),
            lock_file: root.join("cued.lock"),
            logs_dir: root.join("logs"),
            daemon_log: root.join("daemon.log"),
            socket_file: root.join("cued.sock"),
            config_file: root.join("config.toml"),
        };
        let store = Store::open(&paths.db_file).await?;
        let (arm, _arm_rx) = mpsc::unbounded_channel();
        let (nudge, _nudge_rx) = mpsc::unbounded_channel();
        Ok(Ctx {
            store,
            paths,
            config: Config::default(),
            clock: SystemClock,
            spawner: SystemSpawner,
            arm,
            nudge,
            live: Registry::default(),
            stopping: Arc::default(),
            work: Arc::new(()),
            draining: Arc::default(),
            wake: Arc::default(),
            upgrades: None,
            upgrading: Arc::default(),
            reply_written: Arc::default(),
        })
    }

    #[tokio::test]
    async fn aborting_attempt_waiter_cancels_and_reaps_execution() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let ctx = test_context(dir.path()).await?;
        let live = Arc::clone(&ctx.live);
        let (job, run, attempt) = (JobId(10), RunId(5), 1);
        let (cancel, request) = register_long_running_attempt(dir.path(), job, run, attempt, &live);
        let ownership = AttemptOwnership::new(Arc::clone(&live), job, run, attempt, cancel);
        let task = tokio::spawn(run_registered_attempt(SystemSpawner, ownership, request));

        let pid = wait_for_child_pid(&dir.path().join("child-pid")).await?;
        task.abort();
        assert!(
            task.await
                .expect_err("the waiter was aborted")
                .is_cancelled()
        );
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            0,
            "test child exited before shutdown"
        );
        assert!(!live.lock().expect("registry").is_empty());
        tokio::time::timeout(Duration::from_secs(2), terminate_live_steps(&ctx))
            .await
            .expect("shutdown must wait for the supervised process cleanup");
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            -1,
            "shutdown left the child alive"
        );
        assert!(live.lock().expect("registry").is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn panicking_attempt_waiter_cancels_and_reaps_execution() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let live = Registry::default();
        let (job, run, attempt) = (JobId(11), RunId(6), 1);
        let (cancel, request) = register_long_running_attempt(dir.path(), job, run, attempt, &live);
        let (panic_tx, panic_rx) = tokio::sync::oneshot::channel::<()>();
        let task_live = Arc::clone(&live);
        let task = tokio::spawn(async move {
            let ownership = AttemptOwnership::new(task_live, job, run, attempt, cancel);
            let running = run_registered_attempt(SystemSpawner, ownership, request);
            tokio::pin!(running);
            tokio::select! {
                result = &mut running => result,
                _ = panic_rx => panic!("injected panic in attempt waiter"),
            }
        });

        let pid = wait_for_child_pid(&dir.path().join("child-pid")).await?;
        panic_tx.send(()).expect("the waiter is still running");
        assert!(task.await.expect_err("the waiter panicked").is_panic());
        wait_for_attempt_cleanup(&live, pid).await?;
        Ok(())
    }

    /// §2.3 + §3.4: `cued cancel` puts attempt 1 into its teardown and a
    /// `cued retry` behind it spawns attempt 2 while attempt 1 is still
    /// dying. Attempt 1's cleanup must take its *own* handle out of the
    /// registry — with a per-run key it removed attempt 2's, and the next
    /// cancel then found nothing to signal and killed nothing while
    /// reporting success.
    #[test]
    fn a_finished_attempt_unregisters_only_itself() {
        let live = Registry::default();
        let (job, run) = (JobId(1), RunId(1));
        let dying = Cancel::new();
        let replacement = Cancel::new();
        // Handles first, as the fire path does: a signal only latches while
        // someone is listening, and the spawn's handle is taken before the
        // attempt is registered.
        let (dying_handle, replacement_handle) =
            (dying.signal_handle(), replacement.signal_handle());
        register_live(&live, job, run, 1, dying, None, Duration::ZERO);
        register_live(&live, job, run, 2, replacement, None, Duration::ZERO);

        // Attempt 1's task ends and cleans up after itself.
        unregister_live(&live, job, run, 1);

        assert_eq!(
            live.lock().expect("registry").len(),
            1,
            "only attempt 2 should remain"
        );
        assert_eq!(
            cancel_live(&live, job, &[run]),
            1,
            "the live attempt must be reachable"
        );
        assert!(
            replacement_handle.is_cancelled(),
            "cancel must reach the replacement's process"
        );
        assert!(
            !dying_handle.is_cancelled(),
            "the departed attempt needs no second signal"
        );
    }

    /// And while both are alive, a cancel reaches both: the one still inside
    /// its own teardown is holding a process group too.
    #[test]
    fn cancel_reaches_every_live_attempt_of_a_run() {
        let live = Registry::default();
        let (job, run) = (JobId(1), RunId(1));
        let first = Cancel::new();
        let second = Cancel::new();
        let (first_handle, second_handle) = (first.signal_handle(), second.signal_handle());
        register_live(&live, job, run, 1, first, None, Duration::ZERO);
        register_live(&live, job, run, 2, second, None, Duration::ZERO);

        assert_eq!(cancel_live(&live, job, &[run]), 2);
        assert!(first_handle.is_cancelled() && second_handle.is_cancelled());

        // A run nobody asked about is left alone.
        let other = Cancel::new();
        let other_handle = other.signal_handle();
        register_live(&live, job, RunId(2), 1, other, None, Duration::ZERO);
        cancel_live(&live, job, &[run]);
        assert!(
            !other_handle.is_cancelled(),
            "an unrelated run must not be signalled"
        );
    }

    fn outcome(exit_code: Option<i32>, timed_out: bool) -> StepOutcome {
        StepOutcome {
            exit_code,
            timed_out,
            success: exit_code == Some(0) && !timed_out,
            stdout: "starting up\n".into(),
            stderr: "warn: disk\n".into(),
        }
    }

    #[test]
    fn first_match_wins_in_order() {
        let mut step = crate::submit::single_shell_graph(vec!["/bin/true".into()])
            .steps
            .remove("run")
            .unwrap();
        step.transitions = vec![
            Transition {
                when: Condition::ExitEq(3),
                then: Effect::End {
                    outcome: Outcome::Failure,
                },
            },
            Transition {
                when: Condition::Always,
                then: Effect::Goto {
                    step: "next".into(),
                    after: None,
                },
            },
        ];

        let (edge, effect) = choose_edge(&step, &outcome(Some(3), false));
        assert_eq!(edge, Some(0));
        assert!(matches!(
            effect,
            Effect::End {
                outcome: Outcome::Failure
            }
        ));

        let (edge, effect) = choose_edge(&step, &outcome(Some(0), false));
        assert_eq!(edge, Some(1), "ExitEq(3) skipped, Always caught");
        assert!(matches!(effect, Effect::Goto { .. }));
    }

    #[test]
    fn no_match_derives_end_from_the_step() {
        let step = crate::submit::single_shell_graph(vec!["/bin/true".into()])
            .steps
            .remove("run")
            .unwrap();
        let (edge, effect) = choose_edge(&step, &outcome(Some(0), false));
        assert_eq!(edge, None);
        assert!(matches!(
            effect,
            Effect::End {
                outcome: Outcome::Success
            }
        ));

        // §3.2: nonzero exit, timeout, and death-by-signal all derive Failure.
        for failed in [
            outcome(Some(2), false),
            outcome(Some(0), true),
            outcome(None, false),
        ] {
            let (_, effect) = choose_edge(&step, &failed);
            assert!(matches!(
                effect,
                Effect::End {
                    outcome: Outcome::Failure
                }
            ));
        }
    }

    #[test]
    fn conditions_over_streams_and_signals() {
        let ok = outcome(Some(0), false);
        assert!(condition_matches(
            &Condition::Stdout(OutputMatch::Contains("starting".into())),
            &ok
        ));
        // Output ends with a newline, so line-anchored patterns want (?m) —
        // vanilla regex semantics, no silent multiline magic.
        assert!(condition_matches(
            &Condition::Stderr(OutputMatch::Regex(r"(?m)disk$".into())),
            &ok
        ));
        assert!(!condition_matches(
            &Condition::Stderr(OutputMatch::Regex("disk$".into())),
            &ok
        ));
        assert!(!condition_matches(
            &Condition::Stdout(OutputMatch::Regex("[invalid".into())),
            &ok
        ));
        assert!(condition_matches(
            &Condition::All(vec![Condition::Succeeded, Condition::ExitEq(0)]),
            &ok
        ));

        // Signal-killed: no exit code → exit-shaped conditions never match.
        let killed = outcome(None, false);
        assert!(!condition_matches(&Condition::ExitEq(0), &killed));
        assert!(!condition_matches(&Condition::ExitNe(0), &killed));
        assert!(!condition_matches(&Condition::ExitIn(vec![0, 1]), &killed));
        assert!(condition_matches(&Condition::Failed, &killed));
    }
}
