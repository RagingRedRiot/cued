//! The thin client side (DESIGN.md §5): serialize the command into a §5.1
//! request, send it over the socket, render the reply. The one piece of
//! cleverness lives here: auto-spawn (§5.2) — if the daemon isn't running,
//! start it, warn that it's ad-hoc, and retry.
//! `cued logs` is the exception: it reads log files directly, no socket.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde::Serialize;

use anyhow::{Context, Result, bail, ensure};
use jiff::{SignedDuration, Timestamp, Zoned};

use crate::cli::{Command, SubmitCommon};
use crate::config::Config;
use crate::model::{
    Action, Condition, Effect, Hooks, Job, JobId, JobSpec, OutputMatch, Policies, RunId, RunStatus,
    Schedule, Step, Wait,
};
use crate::paths::Paths;
use crate::persist::{
    self, Availability, BackendStatus, CronReboot, PersistenceBackend, SystemdUser,
};
use crate::proto::{
    JobEntry, LogAttempt, MAX_UPGRADE_WAIT_SECS, PROTO_VERSION, Request, RequestBody, Response,
};
use crate::{submit, timeparse};

pub fn run(command: Command) -> Result<()> {
    let paths = Paths::resolve()?;
    match command {
        Command::Mcp | Command::Daemon(_) => unreachable!("routed to daemon::run in main"),
        Command::Approve { job } => approve(&paths, job),
        Command::At {
            time,
            command,
            common,
        } => at(&paths, &time, command, common),
        Command::Remind {
            args,
            every,
            common,
        } => remind(&paths, args, every.as_deref(), common),
        Command::Every {
            spec,
            at,
            command,
            common,
        } => every(&paths, &spec, at.as_deref(), command, common),
        Command::List { all, json } => list(&paths, all, json),
        Command::Continue { job } => rearm(&paths, RequestBody::Continue { job }, "resuming from"),
        Command::Retry { job, from } => {
            rearm(&paths, RequestBody::Retry { job, from }, "rewound to")
        }
        Command::Pause { job } => pause(&paths, job),
        Command::Resume { job } => resume(&paths, job),
        Command::Cancel { job } => cancel(&paths, job),
        Command::Show { job, toml, json } => show(&paths, job, toml, json),
        Command::Gc => gc(&paths),
        Command::Chain {
            first,
            on_fail,
            common,
            ..
        } => chain(&paths, &first, &on_fail, common),
        Command::Submit {
            file,
            at,
            every,
            zone,
        } => submit_file(
            &paths,
            &file,
            at.as_deref(),
            every.as_deref(),
            zone.as_deref(),
        ),
        Command::Logs {
            job,
            run,
            step,
            attempt,
            follow,
            json,
        } => logs(&paths, job, run, step, attempt, follow, json),
        // §8 is a local operation — it installs a unit or a crontab line and
        // never talks to the daemon, so it deliberately doesn't auto-spawn
        // one on the way.
        Command::Setup {
            backend,
            status,
            uninstall,
        } => setup(backend, status, uninstall),
        Command::Upgrade { wait, force } => upgrade(&paths, &wait, force),
        Command::Uninstall { purge, yes } => uninstall(&paths, purge, yes),
    }
}

fn approve(paths: &Paths, reference: String) -> Result<()> {
    let (job, next, queued) = match request(paths, RequestBody::Show { job: reference })? {
        Response::JobDetail {
            job,
            next_fire_at,
            queued_at,
        } => (job, next_fire_at, queued_at),
        other => return fail_on(other),
    };
    ensure!(
        job.status.is_live()
            && job
                .approval
                .as_ref()
                .is_some_and(|a| a.state == crate::model::ApprovalState::Pending),
        "job is not awaiting approval — expired jobs must be rescheduled"
    );
    let hash = job.definition().definition_hash()?;
    print_job(&job, next, queued);
    // Display all bound fields with values redacted, including step env/cwd
    // omitted by the compact human summary above.
    println!(
        "{}",
        serde_json::to_string_pretty(&crate::mcp::redacted_job(&job)?)?
    );
    println!(
        "Approval binds this stored definition and captured environment; referenced scripts and binaries can still change."
    );
    print!("Approve {}? [y/N] ", job.id);
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    ensure!(
        matches!(answer.trim(), "y" | "Y" | "yes"),
        "approval cancelled"
    );
    match request(
        paths,
        RequestBody::Approve {
            job: job.id.to_string(),
            definition_hash: hash,
        },
    )? {
        Response::Approved { job } => {
            println!("{job} approved");
            Ok(())
        }
        other => fail_on(other),
    }
}

// ---------------------------------------------------------------------------
// cued at "9am tomorrow" -- ./backup.sh (§6.1)
// ---------------------------------------------------------------------------

fn at(paths: &Paths, time: &str, command: Vec<String>, common: SubmitCommon) -> Result<()> {
    let config = Config::load(&paths.config_file)?;
    // §9: read the user's words in the zone they named, or their own.
    let zone = timeparse::resolve_zone(common.zone.as_deref())?;
    let now = timeparse::now_in(&zone);
    let at = timeparse::parse_instant(time, &now)?;

    // §2.2: a literal `--` on the command line means "argv, exec directly";
    // its absence means the single-string sh -c form. clap swallows the
    // separator, so consult the raw args for it.
    let had_separator = std::env::args().any(|arg| arg == "--");
    let argv = submit::desugar_shell(command, had_separator);
    let graph = submit::single_shell_graph(argv);

    let cwd = std::env::current_dir()
        .context("resolving cwd")?
        .to_string_lossy()
        .into_owned();
    let env = submit::capture_env(&config.env.deny, &common.keep_env);

    let spec = JobSpec {
        name: common.name,
        schedule: Schedule::Once { at },
        graph,
        cwd,
        env,
        policies: job_policies(&config),
        hooks: Hooks::default(),
    };

    match request(
        paths,
        RequestBody::Submit {
            spec: Box::new(spec),
        },
    )? {
        Response::Submitted { job, .. } => {
            // §9.1 echo rule: the resolved interpretation, always.
            println!(
                "{job} scheduled for {}",
                timeparse::describe_instant(&at, &now.timestamp(), &timeparse::local_zone())
            );
            Ok(())
        }
        other => fail_on(other),
    }
}

// ---------------------------------------------------------------------------
// cued remind "1h" "stretch" (§6.1) — a job whose action is a notification
// ---------------------------------------------------------------------------

fn remind(
    paths: &Paths,
    mut args: Vec<String>,
    every: Option<&str>,
    common: SubmitCommon,
) -> Result<()> {
    let config = Config::load(&paths.config_file)?;
    let zone = timeparse::resolve_zone(common.zone.as_deref())?;
    let now = timeparse::now_in(&zone);
    // Positional shapes (§6.1): `remind WHEN MESSAGE`, or with --every just
    // `remind MESSAGE` (an optional WHEN anchors the cadence, §4.1).
    let message = args.pop().expect("clap enforces at least one positional");
    let when = args.pop();
    let schedule = match (every, when.as_deref()) {
        (Some(spec), anchor) => submit::every_schedule(spec, anchor, &now)?,
        (None, Some(time)) => Schedule::Once {
            at: timeparse::parse_instant(time, &now)?,
        },
        (None, None) => bail!(
            "when should I remind you? — cued remind \"1h\" {message:?} \
             (or make it recurring with --every)"
        ),
    };
    let first = crate::schedule::next_fire(&schedule, &now.timestamp())?
        .context("schedule has no future firing")?;

    let spec = JobSpec {
        name: common.name,
        schedule,
        graph: submit::single_notify_graph(message.clone(), String::new()),
        cwd: std::env::current_dir()
            .context("resolving cwd")?
            .to_string_lossy()
            .into_owned(),
        // Notify steps never exec, but capture anyway — uniform model, and
        // a future edit could add a Shell step to this job.
        env: submit::capture_env(&config.env.deny, &common.keep_env),
        policies: job_policies(&config),
        hooks: Hooks::default(),
    };
    match request(
        paths,
        RequestBody::Submit {
            spec: Box::new(spec),
        },
    )? {
        Response::Submitted { job, .. } => {
            let cadence = every
                .map(|spec| format!(" every {spec} — first"))
                .unwrap_or_else(|| " —".into());
            println!(
                "{job} reminder {message:?}{cadence} {}",
                timeparse::describe_instant(&first, &now.timestamp(), &timeparse::local_zone())
            );
            Ok(())
        }
        other => fail_on(other),
    }
}

// ---------------------------------------------------------------------------
// cued every "30m" -- ./sync.sh (§4, §6.1)
// ---------------------------------------------------------------------------

fn every(
    paths: &Paths,
    spec_text: &str,
    at_flag: Option<&str>,
    command: Vec<String>,
    common: SubmitCommon,
) -> Result<()> {
    let config = Config::load(&paths.config_file)?;
    let zone = timeparse::resolve_zone(common.zone.as_deref())?;
    let now = timeparse::now_in(&zone);
    let schedule = submit::every_schedule(spec_text, at_flag, &now)?;

    let had_separator = std::env::args().any(|arg| arg == "--");
    let argv = submit::desugar_shell(command, had_separator);
    let graph = submit::single_shell_graph(argv);
    let cwd = std::env::current_dir()
        .context("resolving cwd")?
        .to_string_lossy()
        .into_owned();
    let env = submit::capture_env(&config.env.deny, &common.keep_env);

    // Echo material (§9.1): show the first firing the daemon will compute.
    let first = crate::schedule::next_fire(&schedule, &now.timestamp())?
        .context("schedule has no future firing")?;

    let spec = JobSpec {
        name: common.name,
        schedule,
        graph,
        cwd,
        env,
        policies: job_policies(&config),
        hooks: Hooks::default(),
    };
    match request(
        paths,
        RequestBody::Submit {
            spec: Box::new(spec),
        },
    )? {
        Response::Submitted { job, .. } => {
            println!(
                "{job} every {spec_text} — first run {}",
                timeparse::describe_instant(&first, &now.timestamp(), &timeparse::local_zone())
            );
            Ok(())
        }
        other => fail_on(other),
    }
}

// ---------------------------------------------------------------------------
// cued pause / cued resume (§4.2)
// ---------------------------------------------------------------------------

fn pause(paths: &Paths, job: String) -> Result<()> {
    match request(paths, RequestBody::Pause { job })? {
        Response::Paused { job } => {
            println!(
                "{job} paused — nothing new starts; a step already running finishes and is \
                 recorded (use `cued cancel {job}` to stop it). `cued resume {job}` picks up."
            );
            Ok(())
        }
        other => fail_on(other),
    }
}

fn resume(paths: &Paths, job: String) -> Result<()> {
    match request(paths, RequestBody::Resume { job })? {
        Response::Resumed { job, next_at } => {
            match next_at {
                Some(next) => println!(
                    "{job} resumed — next run {}",
                    timeparse::describe_instant(
                        &next,
                        &jiff::Timestamp::now(),
                        &timeparse::local_zone()
                    )
                ),
                None => println!("{job} resumed"),
            }
            Ok(())
        }
        other => fail_on(other),
    }
}

/// §4.2 cancel: the job stops for good, and anything live is terminated.
/// Naming the runs matters — "cancelled" reads very differently depending on
/// whether something was actually killed mid-flight.
fn cancel(paths: &Paths, job: String) -> Result<()> {
    match request(paths, RequestBody::Cancel { job })? {
        Response::JobCancelled { job, runs } => {
            match runs.as_slice() {
                [] => println!("{job} cancelled — nothing was running"),
                [run] => println!("{job} cancelled — terminated {job}.{run}"),
                runs => {
                    let list: Vec<String> = runs.iter().map(|run| format!("{job}.{run}")).collect();
                    println!("{job} cancelled — terminated {}", list.join(", "));
                }
            }
            Ok(())
        }
        other => fail_on(other),
    }
}

/// Precedence built-in < config file < job < step (§10.1): the job's
/// policies start from the user's configured defaults.
fn job_policies(config: &Config) -> Policies {
    Policies {
        missed_wait: config.policy.missed_wait,
        on_interrupt: config.policy.on_interrupt,
        catch_up: config.policy.catch_up,
        overlap: config.policy.overlap,
        deadline: None,
    }
}

// ---------------------------------------------------------------------------
// cued list (§10.3)
// ---------------------------------------------------------------------------

fn list(paths: &Paths, all: bool, json: bool) -> Result<()> {
    match request(paths, RequestBody::List { all })? {
        Response::JobList { jobs } => {
            if json {
                let mut value = serde_json::to_value(&jobs)?;
                hide_approval_hashes(&mut value);
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else if jobs.is_empty() {
                println!("nothing scheduled — try: cued at \"9am tomorrow\" -- ./backup.sh");
            } else {
                print!("{}", render_table(&jobs, &Timestamp::now()));
            }
            Ok(())
        }
        other => fail_on(other),
    }
}

fn render_table(jobs: &[JobEntry], now: &Timestamp) -> String {
    // §9: every instant the table shows is rendered in the reader's own
    // zone. A job submitted against another zone still reads in yours —
    // that is the point of it, "9am Eastern" shown as the 7am it will be.
    let reader = timeparse::local_zone();
    let mut rows = vec![[
        "ID".to_string(),
        "NAME".to_string(),
        "STATUS".to_string(),
        "WHEN".to_string(),
        "LAST".to_string(),
        "COMMAND".to_string(),
    ]];
    for job in jobs {
        let when = match (&job.next_at, &job.last_run) {
            (Some(next), _) => timeparse::describe_instant(next, now, &reader),
            (None, Some(run)) => match &run.ended_at {
                Some(ended) => timeparse::describe_instant(ended, now, &reader),
                None => "-".into(),
            },
            (None, None) => "-".into(),
        };
        let last = job
            .last_run
            .as_ref()
            .map(|run| {
                let mut text = format!("{} {:?}", run.id, run.status).to_lowercase();
                // §3.2 records *why* a run failed when the command itself
                // didn't; showing "failed" alone hides the difference
                // between a broken script and a blown deadline.
                if let Some(reason) = &run.fail_reason {
                    text.push_str(&format!(" ({reason})"));
                }
                text
            })
            .unwrap_or_else(|| "-".into());
        rows.push([
            job.id.to_string(),
            job.name.clone().unwrap_or_else(|| "-".into()),
            format!(
                "{} [{:?}]",
                crate::model::display_status(job.status, job.approval.as_ref()),
                job.source
            ),
            when,
            last,
            job.action.clone(),
        ]);
    }

    let mut widths = [0usize; 6];
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut out = String::new();
    for row in &rows {
        for (index, (cell, width)) in row.iter().zip(widths).enumerate() {
            if index == row.len() - 1 {
                // Last column unpadded — no trailing spaces.
                out.push_str(cell);
            } else {
                out.push_str(&format!("{cell:<width$}  "));
            }
        }
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------------------
// cued continue / cued retry (§3.4)
// ---------------------------------------------------------------------------

fn rearm(paths: &Paths, body: RequestBody, verb: &str) -> Result<()> {
    match request(paths, body)? {
        Response::Rearmed { job, run, step } => {
            println!("{job}.{run} {verb} step '{step}' — running now");
            Ok(())
        }
        other => fail_on(other),
    }
}

// ---------------------------------------------------------------------------
// Socket plumbing: one request, one reply (§5.1) + auto-spawn (§5.2)
// ---------------------------------------------------------------------------

pub(crate) fn request(paths: &Paths, body: RequestBody) -> Result<Response> {
    exchange(&connect_or_spawn(paths)?, body)
}

/// One request, one reply, on a connection the caller chose.
fn exchange(stream: &UnixStream, body: RequestBody) -> Result<Response> {
    let response = exchange_raw(stream, body)?;
    if let Response::Error { message } = &response
        && message.starts_with("bad request:")
    {
        bail!(
            "{message} — run `cued upgrade`, or stop the running `cued daemon` and rerun if it predates that command"
        );
    }
    Ok(response)
}

/// `exchange` without translating a rejected request into advice, for the
/// callers that know better what a rejection means.
fn exchange_raw(stream: &UnixStream, body: RequestBody) -> Result<Response> {
    let mut payload = serde_json::to_string(&Request {
        proto: PROTO_VERSION,
        body,
    })?;
    payload.push('\n');
    (&*stream)
        .write_all(payload.as_bytes())
        .context("writing to daemon")?;

    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .context("reading daemon reply")?;
    ensure!(
        !line.is_empty(),
        "daemon closed the connection without replying"
    );
    // §5.1's version handshake covers a *declared* change. Pre-release the
    // wire shape moves without the number moving (deliberately — there is no
    // compatibility to retain yet), so the realistic cause of an undecodable
    // reply is a daemon still running older code. Say so, rather than leave
    // a serde error as the only clue.
    let response: Response = serde_json::from_str(&line).context(
        "couldn't decode the daemon's reply — it is probably running an older          build than this CLI; stop the running `cued daemon` and rerun, and the          next command respawns it",
    )?;
    Ok(response)
}

fn connect_or_spawn(paths: &Paths) -> Result<UnixStream> {
    if let Ok(stream) = UnixStream::connect(&paths.socket_file) {
        return Ok(stream);
    }

    // §5.2 auto-spawn, the gpg-agent model: start it detached and retry
    // briefly. The warning is about being *unsupervised*, so it's only worth
    // printing when §8 says nothing is: with a backend installed, the daemon
    // being down is a transient the backend will handle, and nagging about
    // it every time would train the user to ignore the line that matters.
    let supervised = persist::any_installed();
    let exe = std::env::current_exe().context("locating the cued binary")?;
    let mut daemon = std::process::Command::new(exe);
    daemon
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0); // detach from our ctrl-c fate
    daemon.spawn().context("spawning cued daemon")?;
    if !supervised {
        eprintln!(
            "cued: daemon started ad-hoc — jobs fire only while it runs; \
             run `cued setup` to survive logout/reboot"
        );
    }

    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(50));
        if let Ok(stream) = UnixStream::connect(&paths.socket_file) {
            return Ok(stream);
        }
    }
    // §10.3: this is the "exit 3" case once stable exit codes land. The
    // daemon's own stderr goes to its log, so point there rather than
    // leaving the user with nothing to read.
    bail!(
        "daemon unreachable at {} (auto-spawn didn't come up) — see {}",
        paths.socket_file.display(),
        paths.daemon_log.display()
    )
}

/// Terminal handling for every reply that isn't the one the caller wanted.
fn fail_on<T>(response: Response) -> Result<T> {
    match response {
        Response::Error { message } => bail!("{message}"),
        // §5.1: upgrading the binary under a live old daemon is the
        // expected failure mode — designed message, not a serde error.
        Response::ProtoMismatch { daemon_proto } => bail!(
            "daemon (proto {daemon_proto}) doesn't match this CLI (proto {PROTO_VERSION}) — \
             run `cued upgrade`, or stop the running `cued daemon` and rerun if it predates that command"
        ),
        other => bail!("unexpected daemon reply: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// cued setup (§8) — probe, offer, install, tear down
// ---------------------------------------------------------------------------

/// The §8.1 three-way, in the order `cued setup` offers it: best first.
fn backends() -> Vec<Box<dyn PersistenceBackend>> {
    vec![
        Box::new(SystemdUser { linger: true }),
        Box::new(CronReboot),
        Box::new(SystemdUser { linger: false }),
    ]
}

fn backend_by_key(key: &str) -> Option<Box<dyn PersistenceBackend>> {
    match key {
        "systemd-linger" => Some(Box::new(SystemdUser { linger: true })),
        "cron" => Some(Box::new(CronReboot)),
        "systemd" => Some(Box::new(SystemdUser { linger: false })),
        _ => None,
    }
}

const BACKEND_KEYS: [&str; 3] = ["systemd-linger", "cron", "systemd"];

fn setup(backend: Option<String>, status_only: bool, uninstall: bool) -> Result<()> {
    if uninstall {
        return setup_uninstall();
    }
    if let Some(key) = backend {
        let chosen = backend_by_key(&key).with_context(|| format!("unknown backend {key:?}"))?;
        return setup_install(chosen.as_ref());
    }

    print_probe()?;
    if status_only {
        return Ok(());
    }

    // §8.2: offer only what this host can actually do. A non-interactive
    // shell gets the same information and the flag to act on it, rather
    // than a prompt nobody can answer.
    if !stdin_is_a_tty() {
        println!(
            "\nNot a terminal — choose with: cued setup --backend <{}>",
            BACKEND_KEYS.join("|")
        );
        return Ok(());
    }
    let Some(chosen) = prompt_for_backend()? else {
        println!("Nothing installed.");
        return Ok(());
    };
    setup_install(chosen.as_ref())
}

/// §8.2's "probe before offering": what is installed now, and what this host
/// supports — reported before any choice is asked for.
fn print_probe() -> Result<()> {
    println!("cued persistence (§8)\n");
    println!("  binary:  {}", persist::daemon_exe()?.display());
    println!(
        "  linger:  {}",
        if persist::linger_enabled() {
            "enabled"
        } else {
            "not enabled"
        }
    );
    println!();

    for (key, backend) in BACKEND_KEYS.iter().zip(backends()) {
        let installed = matches!(backend.status(), Ok(BackendStatus::Installed));
        let mark = if installed { "*" } else { " " };
        let availability = match backend.available() {
            Availability::Available => "available".to_string(),
            Availability::Caveat(why) => format!("available, with a caveat — {why}"),
            Availability::Unavailable(why) => format!("unavailable — {why}"),
        };
        println!("{mark} {key:<16} {}", backend.name());
        println!("    {availability}");
        println!("    {}", backend.tradeoff());
    }
    println!("\n  (* = installed)");
    Ok(())
}

fn prompt_for_backend() -> Result<Option<Box<dyn PersistenceBackend>>> {
    let offered: Vec<(usize, &str, Box<dyn PersistenceBackend>)> = BACKEND_KEYS
        .iter()
        .zip(backends())
        .filter(|(_, backend)| backend.available().is_available())
        .enumerate()
        .map(|(index, (key, backend))| (index + 1, *key, backend))
        .collect();

    if offered.is_empty() {
        bail!("no persistence backend is usable on this host — see the probe above");
    }

    println!("\nInstall which?");
    for (number, key, backend) in &offered {
        println!("  {number}) {key} — {}", backend.name());
    }
    print!("Choice [1-{}, or q to quit]: ", offered.len());
    std::io::Write::flush(&mut std::io::stdout())?;

    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("reading your choice")?;
    let answer = line.trim();
    if answer.is_empty() || answer.eq_ignore_ascii_case("q") {
        return Ok(None);
    }
    let number: usize = answer
        .parse()
        .with_context(|| format!("{answer:?} isn't one of 1-{}", offered.len()))?;
    offered
        .into_iter()
        .find(|(candidate, _, _)| *candidate == number)
        .map(|(_, _, backend)| Some(backend))
        .with_context(|| format!("{number} isn't one of the offered choices"))
}

fn setup_install(backend: &dyn PersistenceBackend) -> Result<()> {
    match backend.available() {
        Availability::Unavailable(why) => {
            bail!("{} isn't usable here — {why}", backend.name())
        }
        // Installable, but don't let the caveat get lost between the probe
        // and the confirmation — this is the line that stops someone
        // believing a promise the host may not keep.
        Availability::Caveat(why) => println!("Note: {why}\n"),
        Availability::Available => {}
    }
    // §8.2 symmetric teardown, applied to *switching*: leaving a cron
    // @reboot line behind while installing a systemd unit would start two
    // daemons, and the second would lose the §5.2 flock and die quietly.
    for other in backends() {
        if other.name() != backend.name() && matches!(other.status(), Ok(BackendStatus::Installed))
        {
            for note in other.uninstall()? {
                println!("  {note}");
            }
        }
    }

    let exe = persist::daemon_exe()?;
    println!("Installing {} ({})", backend.name(), exe.display());
    for note in backend.install(&exe)? {
        println!("  {note}");
    }
    println!("\n{} is installed.", backend.name());
    Ok(())
}

/// What is installed, once each. `systemd-linger` and `systemd` are two
/// offers of the same unit file — linger is an account setting, not part of
/// the install — so both report it, and removing it is the same either way.
fn installed_backends() -> Vec<Box<dyn PersistenceBackend>> {
    BACKEND_KEYS
        .iter()
        .zip(backends())
        .filter(|(key, backend)| {
            **key != "systemd-linger" && matches!(backend.status(), Ok(BackendStatus::Installed))
        })
        .map(|(_, backend)| backend)
        .collect()
}

fn setup_uninstall() -> Result<()> {
    let mut removed = false;
    for backend in installed_backends() {
        println!("Removing {}", backend.name());
        for note in backend.uninstall()? {
            println!("  {note}");
        }
        removed = true;
    }
    if removed {
        println!("\nNo persistence backend is installed; the daemon is ad-hoc again (§5.2).");
    } else {
        println!("Nothing to remove — no persistence backend is installed.");
    }
    Ok(())
}

fn stdin_is_a_tty() -> bool {
    // SAFETY: isatty on a borrowed fd; no ownership, no side effects.
    unsafe { libc::isatty(libc::STDIN_FILENO) == 1 }
}

// ---------------------------------------------------------------------------
// cued logs (§2.1, §5.1) — the daemon says where; the client reads
// ---------------------------------------------------------------------------

/// How often `-f` looks for new bytes, and how many of those polls pass
/// before we re-ask the daemon whether the attempt has ended.
const FOLLOW_POLL: Duration = Duration::from_millis(200);
const FOLLOW_RECHECK_EVERY: u32 = 5;

#[derive(Serialize)]
struct JsonAttempt<'a> {
    step: &'a str,
    attempt: u32,
    started_at: String,
    ended_at: Option<String>,
    exit_code: Option<i32>,
    timed_out: bool,
    path: String,
    content: String,
}

fn logs(
    paths: &Paths,
    job: String,
    run: Option<i64>,
    step: Option<String>,
    attempt: Option<u32>,
    follow: bool,
    json: bool,
) -> Result<()> {
    let (job_id, run_id, attempts) = log_manifest(paths, &job, run, step, attempt)?;

    if json {
        return print_logs_json(paths, job_id, run_id, &attempts);
    }
    if follow {
        return follow_logs(paths, job_id, run_id, attempts);
    }

    if attempts.is_empty() {
        // Not an error: a Skipped firing, a Missed run, or a reminder that
        // never spawned a process all legitimately have nothing to show.
        println!("{job_id}.{run_id}: no attempts recorded");
        return Ok(());
    }
    let notify_steps = notify_steps(paths, job_id);
    // One attempt is the common case (a single-step job), and raw bytes are
    // what a pipe wants — headers only once there's something to tell apart.
    let label = attempts.len() > 1;
    for entry in &attempts {
        print_attempt(paths, job_id, run_id, entry, label, &notify_steps)?;
    }
    Ok(())
}

/// Which of a job's steps are `Notify` — the one thing the §2.1 manifest
/// can't say, since `step_runs` records what happened and not what the step
/// *is*. Read from the definition via `Show`, which the protocol already
/// carries, rather than widening the manifest for a label. A job that can't
/// be read back just means no step is known to be a notify, and the header
/// falls back to the §2.2 reading.
fn notify_steps(paths: &Paths, job: JobId) -> BTreeSet<String> {
    match request(
        paths,
        RequestBody::Show {
            job: job.to_string(),
        },
    ) {
        Ok(Response::JobDetail { job, .. }) => job
            .graph
            .steps
            .iter()
            .filter(|(_, step)| matches!(step.action, Action::Notify { .. }))
            .map(|(id, _)| id.clone())
            .collect(),
        _ => BTreeSet::new(),
    }
}

fn log_manifest(
    paths: &Paths,
    job: &str,
    run: Option<i64>,
    step: Option<String>,
    attempt: Option<u32>,
) -> Result<(JobId, RunId, Vec<LogAttempt>)> {
    match request(
        paths,
        RequestBody::Logs {
            job: job.to_string(),
            run,
            step,
            attempt,
        },
    )? {
        Response::LogManifest { job, run, attempts } => Ok((job, run, attempts)),
        other => {
            fail_on(other)?;
            unreachable!("fail_on returns Err for every non-manifest reply")
        }
    }
}

fn print_attempt(
    paths: &Paths,
    job: JobId,
    run: RunId,
    entry: &LogAttempt,
    label: bool,
    notify_steps: &BTreeSet<String>,
) -> Result<()> {
    let path = paths.step_log(job, run, &entry.step, entry.attempt);
    let notify = notify_steps.contains(&entry.step);
    if label {
        println!("==> {} <==", attempt_header(entry, notify));
    }
    match std::fs::read(&path) {
        Ok(bytes) => {
            // Log files hold whatever the step emitted, which need not be
            // UTF-8; write the bytes through rather than mangling them.
            std::io::Write::write_all(&mut std::io::stdout(), &bytes)
                .context("writing log output")?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // A Notify step never spawns a process, so it never opens a log
            // file (§2.1). Say so — silence would read as a missing file.
            if label {
                println!("(no output captured)");
            } else {
                println!(
                    "{job}.{run} step {:?} attempt {} captured no output \
                     (a notify step runs no process)",
                    entry.step, entry.attempt
                );
            }
        }
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", path.display()));
        }
    }
    Ok(())
}

/// "build.2 — exit 1 (2026-09-16 12:04:03)" — enough to tell attempts apart
/// and to see which one failed, without a second command.
///
/// `notify` says the step never spawned a process. Without it a notification
/// that worked perfectly reports as "killed", because §2.2's "no exit code
/// means death by signal" is true of a Shell step and meaningless for a
/// Notify one, which has no process to kill.
fn attempt_header(entry: &LogAttempt, notify: bool) -> String {
    let outcome = if entry.running {
        "running".to_string()
    } else if entry.ended_at.is_none() {
        // §3.4: a killed attempt's row is left open on purpose. Calling that
        // "running" — as reading liveness off `ended_at` did — tells someone
        // looking at a parked job that work is still going on.
        "interrupted".to_string()
    } else if entry.timed_out {
        "timed out".to_string()
    } else {
        match entry.exit_code {
            Some(code) => format!("exit {code}"),
            None if notify => "notified".to_string(),
            None => "killed".to_string(),
        }
    };
    format!(
        "{}.{} — {outcome} ({})",
        entry.step,
        entry.attempt,
        entry
            .started_at
            .to_zoned(timeparse::local_zone())
            .strftime("%Y-%m-%d %H:%M:%S")
    )
}

/// §5.1: `-f` is a tail, not a stream over the socket. We follow the one
/// attempt the run's cursor says is executing — execution is sequential
/// (§3), so there is never more than one — and stop when it stops.
///
/// Liveness is `running`, not an absent `ended_at`. §3.4 leaves a killed
/// attempt's row open deliberately, so following on the older reading meant
/// latching onto an interrupted attempt and waiting for an end time that
/// would never be written: `cued logs -f` on a parked job hung until killed.
fn follow_logs(paths: &Paths, job: JobId, run: RunId, attempts: Vec<LogAttempt>) -> Result<()> {
    let Some(live) = attempts.iter().find(|entry| entry.running) else {
        // Nothing to follow: show what's there rather than hanging on a
        // stream that will never produce a byte.
        let label = attempts.len() > 1;
        let notify = notify_steps(paths, job);
        for entry in &attempts {
            print_attempt(paths, job, run, entry, label, &notify)?;
        }
        if !attempts.is_empty() {
            eprintln!("cued: nothing is running in {job}.{run} — showed what it captured");
        }
        return Ok(());
    };

    let path = paths.step_log(job, run, &live.step, live.attempt);
    let (step, attempt) = (live.step.clone(), live.attempt);
    eprintln!("cued: following {job}.{run} {step}.{attempt} …");

    let mut offset = 0u64;
    let mut polls = 0u32;
    loop {
        offset += drain_from(&path, offset)?;
        polls += 1;
        if polls.is_multiple_of(FOLLOW_RECHECK_EVERY) {
            let (_, _, current) = log_manifest(
                paths,
                &job.to_string(),
                Some(run.0),
                Some(step.clone()),
                Some(attempt),
            )?;
            let stopped = current.first().is_none_or(|entry| !entry.running);
            if stopped {
                // One last read: bytes can land between the final write and
                // the row being closed.
                drain_from(&path, offset)?;
                // An attempt that stopped without an end time was killed
                // rather than finished (§3.4) — say so, or the tail just
                // ends and looks like the step completed.
                if current
                    .first()
                    .is_some_and(|entry| entry.ended_at.is_none())
                {
                    eprintln!(
                        "cued: {job}.{run} {step}.{attempt} was interrupted —                          `cued list` shows where it parked"
                    );
                }
                return Ok(());
            }
        }
        std::thread::sleep(FOLLOW_POLL);
    }
}

/// Copy whatever has appeared past `offset` to stdout; returns how much.
/// A file that doesn't exist yet is normal — the attempt row is written
/// before the process spawns (§3.3), so we can arrive first.
fn drain_from(path: &std::path::Path, offset: u64) -> Result<u64> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(error).with_context(|| format!("opening {}", path.display()));
        }
    };
    std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(offset))
        .with_context(|| format!("seeking {}", path.display()))?;
    let mut buffer = Vec::new();
    let read = std::io::Read::read_to_end(&mut file, &mut buffer)
        .with_context(|| format!("reading {}", path.display()))?;
    if read > 0 {
        let mut out = std::io::stdout();
        std::io::Write::write_all(&mut out, &buffer).context("writing log output")?;
        std::io::Write::flush(&mut out).context("flushing log output")?;
    }
    Ok(read as u64)
}

/// §10.3: `--json` for scripting. Content is included so a script gets the
/// output and its provenance in one shot, and `path` so it can go read more.
fn print_logs_json(paths: &Paths, job: JobId, run: RunId, attempts: &[LogAttempt]) -> Result<()> {
    let rendered: Vec<JsonAttempt<'_>> = attempts
        .iter()
        .map(|entry| {
            let path = paths.step_log(job, run, &entry.step, entry.attempt);
            JsonAttempt {
                step: &entry.step,
                attempt: entry.attempt,
                started_at: entry.started_at.to_string(),
                ended_at: entry.ended_at.as_ref().map(|at| at.to_string()),
                exit_code: entry.exit_code,
                timed_out: entry.timed_out,
                content: std::fs::read_to_string(&path).unwrap_or_default(),
                path: path.display().to_string(),
            }
        })
        .collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "job": job.to_string(),
            "run": run.to_string(),
            "attempts": rendered,
        }))?
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// cued show (§6, §10.3)
// ---------------------------------------------------------------------------

fn show(paths: &Paths, job: String, as_toml: bool, as_json: bool) -> Result<()> {
    let (job, next_fire_at, queued_at) = match request(paths, RequestBody::Show { job })? {
        Response::JobDetail {
            job,
            next_fire_at,
            queued_at,
        } => (*job, next_fire_at, queued_at),
        other => return fail_on(other),
    };

    if as_toml {
        // §6: the export form is the file format — dump, edit, resubmit.
        print!(
            "{}",
            crate::export::job_to_toml(&job, &timeparse::local_zone())
        );
        return Ok(());
    }
    if as_json {
        // §10.3 scripting: the serde form, in full — including the captured
        // env, which the human view summarises and the TOML omits. Approval
        // hashes are internal concurrency fences, not useful user output.
        let mut value = serde_json::to_value(&job)?;
        hide_approval_hashes(&mut value);
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }
    print_job(&job, next_fire_at, queued_at);
    Ok(())
}

fn hide_approval_hashes(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            if let Some(serde_json::Value::Object(approval)) = object.get_mut("approval") {
                approval.remove("definition_hash");
            }
            for child in object.values_mut() {
                hide_approval_hashes(child);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                hide_approval_hashes(item);
            }
        }
        _ => {}
    }
}

fn print_job(job: &Job, next_fire_at: Option<Timestamp>, queued_at: Option<Timestamp>) {
    // §9: reading is always in the reader's own zone, whatever zone the job
    // was submitted against.
    let reader = timeparse::local_zone();
    let now = Timestamp::now();
    println!("{}  {}", job.id, job.name.as_deref().unwrap_or("(unnamed)"));
    println!("  status    {}", job.display_status());
    println!("  source    {:?}", job.source);
    if let Some(at) = job.expired_at {
        println!(
            "  expired   {at} ({:?}) — reschedule to run",
            job.expiry_reason
        );
    }
    println!("  schedule  {}", describe_schedule(&job.schedule));
    if let Some(next) = &next_fire_at {
        println!(
            "  next      {}",
            timeparse::describe_instant(next, &now, &reader)
        );
    }
    if let Some(queued) = &queued_at {
        // §4.2: one coalesced firing waiting on the live run to end.
        println!(
            "  queued    {}",
            timeparse::describe_instant(queued, &now, &reader)
        );
    }
    println!(
        "  created   {}",
        job.created_at
            .to_zoned(reader.clone())
            .strftime("%Y-%m-%d %H:%M:%S %Z")
    );
    println!("  cwd       {}", job.cwd);

    // Names let a user check whether a job captured inputs it relies on;
    // values are available only through the explicit `show --json` output.
    println!("  env       {} vars captured", job.env.vars.len());
    let captured_names: Vec<&str> = job.env.vars.keys().map(String::as_str).collect();
    print_name_list("            ", &captured_names);
    if !job.env.stripped.is_empty() {
        println!(
            "  env       {} stripped by the secrets denylist",
            job.env.stripped.len()
        );
        let stripped_names: Vec<&str> = job.env.stripped.iter().map(String::as_str).collect();
        print_name_list("            ", &stripped_names);
    }

    println!("  policies  {}", describe_policies(&job.policies));
    let hooks = describe_hooks(&job.hooks);
    if !hooks.is_empty() {
        println!("  hooks     {hooks}");
    }

    println!(
        "\n  steps ({}, entry = {:?})",
        job.graph.steps.len(),
        job.graph.entry
    );
    let entry = &job.graph.entry;
    if let Some(step) = job.graph.steps.get(entry) {
        print_step(entry, step, true);
    }
    for (id, step) in &job.graph.steps {
        if id != entry {
            print_step(id, step, false);
        }
    }
}

fn print_name_list(indent: &str, names: &[&str]) {
    let mut line = indent.to_string();
    for name in names {
        if line.len() > indent.len() && line.len() + 2 + name.len() > 88 {
            println!("{line}");
            line = indent.to_string();
        }
        if line.len() > indent.len() {
            line.push_str(", ");
        }
        line.push_str(name);
    }
    if line.len() > indent.len() {
        println!("{line}");
    }
}

fn print_step(id: &str, step: &Step, is_entry: bool) {
    let marker = if is_entry { "→" } else { " " };
    println!("  {marker} {id}");
    match &step.action {
        Action::Shell { argv } => match argv.as_slice() {
            [shell, flag, script] if shell == "/bin/sh" && flag == "-c" => {
                println!("      run     {script}");
            }
            argv => println!("      run     {}", argv.join(" ")),
        },
        Action::Notify { title, body } => {
            println!("      notify  {title}");
            if !body.is_empty() {
                println!("              {body}");
            }
        }
    }
    let mut limits = Vec::new();
    if let Some(timeout) = step.timeout {
        limits.push(format!("timeout {timeout:#}"));
    }
    if let Some(max) = step.max_visits {
        limits.push(format!("max_visits {max}"));
    }
    if step.restart_safe {
        limits.push("restart_safe".to_string());
    }
    if !limits.is_empty() {
        println!("      limits  {}", limits.join(", "));
    }
    // §3.2: ordered, first match wins — so print them in order, numbered.
    for (index, transition) in step.transitions.iter().enumerate() {
        println!(
            "      {index}. when {} → {}",
            describe_condition(&transition.when),
            describe_effect(&transition.then)
        );
    }
    if step.transitions.is_empty() {
        // Not an omission: §3.2 derives End from the step's own outcome.
        println!("      (no transitions — success ends the run, failure fails it)");
    }
}

fn describe_schedule(schedule: &Schedule) -> String {
    let mut text = match schedule {
        Schedule::Once { at } => format!(
            "once at {}",
            at.to_zoned(timeparse::local_zone())
                .strftime("%Y-%m-%d %H:%M:%S %Z")
        ),
        Schedule::Every {
            interval, anchor, ..
        } => format!(
            "every {interval:#}, anchored {}",
            anchor
                .to_zoned(timeparse::local_zone())
                .strftime("%Y-%m-%d %H:%M:%S %Z")
        ),
        Schedule::Calendar { spec, zone, .. } => {
            format!("{} ({zone})", crate::export::describe_calendar(spec))
        }
    };
    match schedule {
        Schedule::Once { .. } => {}
        Schedule::Every { until, count, .. } | Schedule::Calendar { until, count, .. } => {
            if let Some(until) = until {
                text.push_str(&format!(
                    ", until {}",
                    until
                        .to_zoned(timeparse::local_zone())
                        .strftime("%Y-%m-%d %H:%M")
                ));
            }
            if let Some(count) = count {
                text.push_str(&format!(", {count} firings max"));
            }
        }
    }
    text
}

fn describe_policies(policies: &Policies) -> String {
    let mut parts = vec![
        format!("missed_wait={:?}", policies.missed_wait),
        format!("on_interrupt={:?}", policies.on_interrupt),
        format!("catch_up={:?}", policies.catch_up),
        format!("overlap={:?}", policies.overlap),
    ];
    if let Some(deadline) = policies.deadline {
        parts.push(format!("deadline={deadline:#}"));
    }
    parts.join(", ")
}

fn describe_hooks(hooks: &Hooks) -> String {
    [
        ("on_hold", hooks.on_hold.is_some()),
        ("on_failure", hooks.on_failure.is_some()),
        ("on_success", hooks.on_success.is_some()),
        ("on_missed", hooks.on_missed.is_some()),
    ]
    .iter()
    .filter(|(_, set)| *set)
    .map(|(name, _)| *name)
    .collect::<Vec<_>>()
    .join(", ")
}

fn describe_condition(when: &Condition) -> String {
    match when {
        Condition::Always => "always".into(),
        Condition::Succeeded => "succeeded".into(),
        Condition::Failed => "failed".into(),
        Condition::TimedOut => "timed out".into(),
        Condition::ExitEq(code) => format!("exit == {code}"),
        Condition::ExitNe(code) => format!("exit != {code}"),
        Condition::ExitIn(codes) => format!("exit in {codes:?}"),
        Condition::Stdout(matcher) => format!("stdout {}", describe_match(matcher)),
        Condition::Stderr(matcher) => format!("stderr {}", describe_match(matcher)),
        Condition::All(inner) => inner
            .iter()
            .map(describe_condition)
            .collect::<Vec<_>>()
            .join(" and "),
    }
}

fn describe_match(matcher: &OutputMatch) -> String {
    match matcher {
        OutputMatch::Contains(needle) => format!("contains {needle:?}"),
        OutputMatch::Regex(pattern) => format!("matches /{pattern}/"),
    }
}

fn describe_effect(then: &Effect) -> String {
    match then {
        Effect::End { outcome } => format!("end ({outcome:?})"),
        Effect::Goto { step, after: None } => format!("goto {step}"),
        Effect::Goto {
            step,
            after: Some(Wait::In(d)),
        } => format!("wait {d:#} then goto {step}"),
        Effect::Goto {
            step,
            after: Some(Wait::Until(at)),
        } => {
            format!(
                "wait until {} then goto {step}",
                at.to_zoned(timeparse::local_zone())
                    .strftime("%Y-%m-%d %H:%M")
            )
        }
        Effect::Goto {
            step,
            after: Some(Wait::Backoff { start, factor, max }),
        } => {
            format!("backoff {start:#}×{factor} (max {max:#}) then goto {step}")
        }
    }
}

// ---------------------------------------------------------------------------
// cued submit flow.toml (§6.2)
// ---------------------------------------------------------------------------

fn submit_file(
    paths: &Paths,
    file: &std::path::Path,
    at: Option<&str>,
    every: Option<&str>,
    zone: Option<&str>,
) -> Result<()> {
    let config = Config::load(&paths.config_file)?;
    let text =
        std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let now = Zoned::now();
    let cwd = std::env::current_dir()
        .context("resolving cwd")?
        .to_string_lossy()
        .into_owned();

    let spec = submit::from_toml(
        &text,
        submit::SubmitContext {
            now: &now,
            zone,
            at,
            every,
            // §2.1: the file is a definition; the environment is still
            // captured here, from the shell doing the submitting.
            env: submit::capture_env(&config.env.deny, &[]),
            // §10.1: the same configured defaults every other front-end
            // starts from, for the file's `[defaults]` to override.
            policies: job_policies(&config),
            cwd: &cwd,
        },
    )
    .with_context(|| format!("in {}", file.display()))?;

    // §9.1's echo rule applies to a file exactly as it does to `cued at`:
    // print what the schedule resolved to, before it's the daemon's problem.
    let first = crate::schedule::next_fire(&spec.schedule, &now.timestamp())?
        .context("schedule has no future firing")?;
    let steps = spec.graph.steps.len();
    let name = spec.name.clone();

    match request(
        paths,
        RequestBody::Submit {
            spec: Box::new(spec),
        },
    )? {
        Response::Submitted { job, .. } => {
            let label = name.map(|name| format!(" {name:?}")).unwrap_or_default();
            println!(
                "{job}{label} — {steps} step{}, first run {}",
                if steps == 1 { "" } else { "s" },
                timeparse::describe_instant(&first, &now.timestamp(), &timeparse::local_zone())
            );
            Ok(())
        }
        other => fail_on(other),
    }
}

// ---------------------------------------------------------------------------
// cued gc (§10.2)
// ---------------------------------------------------------------------------

/// The daemon sweeps at startup and daily anyway; this is for when you want
/// the space back now, or want to see that retention is doing anything.
fn gc(paths: &Paths) -> Result<()> {
    match request(paths, RequestBody::Gc)? {
        Response::Collected { runs, jobs } => {
            if runs == 0 && jobs == 0 {
                println!("nothing to prune — everything is within the retention policy");
            } else {
                println!(
                    "pruned {runs} run{} and {jobs} job{}, with their logs",
                    if runs == 1 { "" } else { "s" },
                    if jobs == 1 { "" } else { "s" }
                );
            }
            Ok(())
        }
        other => fail_on(other),
    }
}

// ---------------------------------------------------------------------------
// cued upgrade (§5.2)
// ---------------------------------------------------------------------------

/// Move the running daemon onto the binary now installed at its path: the
/// daemon finishes its running steps, then re-executes in place. The store,
/// the persistence backend and the daemon's PID are all left as they were.
fn upgrade(paths: &Paths, wait: &str, force: bool) -> Result<()> {
    let wait = timeparse::parse_duration(wait)?;
    ensure!(wait.is_positive(), "--wait must be positive");
    let wait_secs = wait.as_secs().max(1) as u64;
    ensure!(
        wait_secs <= MAX_UPGRADE_WAIT_SECS,
        "--wait can be at most {}",
        timeparse::describe_duration(SignedDuration::from_secs(MAX_UPGRADE_WAIT_SECS as i64))
    );

    // Deliberately no auto-spawn: a fresh daemon would already be the
    // installed binary, so there is nothing to do.
    let Ok(stream) = UnixStream::connect(&paths.socket_file) else {
        println!("no daemon is running — the next cued command starts the installed binary");
        return Ok(());
    };
    eprintln!(
        "cued: waiting up to {} for running steps to finish…",
        timeparse::describe_duration(wait)
    );
    match exchange_raw(&stream, RequestBody::Upgrade { wait_secs, force })? {
        // Only a daemon built before `upgrade` existed fails to decode it.
        Response::Error { message } if message.starts_with("bad request:") => bail!(
            "the running daemon predates `cued upgrade`, so it can't be upgraded in place. \
             Restart it once — `systemctl --user restart cued` if you use the systemd \
             backend, otherwise stop the `cued daemon` process and run any cued command — \
             and later upgrades will work"
        ),
        Response::UpgradeCurrent { exe } => {
            println!(
                "daemon is already running the binary installed at {}",
                exe.display()
            );
            Ok(())
        }
        Response::UpgradeAbandoned { reason } => bail!("upgrade abandoned: {reason}"),
        Response::Upgrading { exe } => {
            let pid = await_new_image(paths)?;
            // Answering is not enough: a daemon that could not exec the new
            // build falls back to its old image, which answers too.
            if !runs_binary(pid, &exe) {
                bail!(
                    "the daemon could not start {} and is still running the previous build \
                     — see {}",
                    exe.display(),
                    paths.daemon_log.display()
                );
            }
            println!("daemon upgraded in place to {}", exe.display());
            // The daemon execs its own path, which is what the persistence
            // backend starts too. If this CLI lives elsewhere, the two are
            // now different builds — worth saying.
            let ours = std::env::current_exe().and_then(|exe| exe.canonicalize());
            if ours.as_ref().is_ok_and(|ours| *ours != exe) {
                eprintln!(
                    "cued: note: this CLI is {}, not the daemon's binary",
                    ours.expect("checked").display()
                );
            }
            Ok(())
        }
        other => fail_on(other),
    }
}

/// The socket never closed across the exec, so this connects at once and
/// the reply arrives when the new image has reconciled and is serving.
/// Returns the pid that answered.
fn await_new_image(paths: &Paths) -> Result<i32> {
    let stream = UnixStream::connect(&paths.socket_file).with_context(|| {
        format!(
            "the daemon did not come back — see {}",
            paths.daemon_log.display()
        )
    })?;
    // Startup runs migrations, which on a large store can take a while.
    stream.set_read_timeout(Some(Duration::from_secs(120)))?;
    match exchange(&stream, RequestBody::Ping) {
        Ok(Response::Pong { .. }) => {
            peer_pid(&stream).context("couldn't identify the process serving the daemon socket")
        }
        Ok(other) => fail_on(other),
        Err(error) => Err(error).with_context(|| {
            format!(
                "the upgraded daemon is not answering — see {}",
                paths.daemon_log.display()
            )
        }),
    }
}

// ---------------------------------------------------------------------------
// cued uninstall (§8.2)
// ---------------------------------------------------------------------------

/// How long the daemon locks may stay held with no daemon process to wait
/// for (one that exited between our look and its last unlock, or a holder
/// we cannot identify) before uninstall stops waiting.
const UNIDENTIFIED_LOCK_WAIT: Duration = Duration::from_secs(10);

/// How often uninstall says it is still waiting for the daemon to stop.
const STOP_PROGRESS_EVERY: Duration = Duration::from_secs(15);

/// `setup --uninstall`, then everything else cued put on this account:
/// stop the daemon, delete the store, logs and socket. Config is the
/// user's own writing, so it goes only with `--purge`; the binary belongs
/// to whatever installed it (cargo), so it is pointed at, never deleted.
fn uninstall(paths: &Paths, purge: bool, yes: bool) -> Result<()> {
    let installed = installed_backends();
    // Deliberately no auto-spawn: starting a daemon to delete it would
    // create the very store we are about to remove.
    let daemon = UnixStream::connect(&paths.socket_file).ok();
    let daemon_pid = daemon.as_ref().and_then(peer_pid);
    let summary = daemon.as_ref().and_then(|stream| {
        match exchange(stream, RequestBody::List { all: true }) {
            Ok(Response::JobList { jobs }) => Some(describe_store(&jobs)),
            _ => None,
        }
    });
    drop(daemon);
    let config_dir = paths.config_file.parent().map(Path::to_path_buf);

    println!("This removes cued from this account:");
    for backend in &installed {
        println!("  - persistence: {}", backend.name());
    }
    match (daemon_pid, &summary) {
        (Some(pid), Some(summary)) if summary.running > 0 => println!(
            "  - the daemon (pid {pid}), terminating {} running step{}",
            summary.running,
            if summary.running == 1 { "" } else { "s" }
        ),
        (Some(pid), _) => println!("  - the daemon (pid {pid})"),
        (None, _) => {}
    }
    let size = dir_size(&paths.data_dir);
    match &summary {
        Some(summary) => println!(
            "  - {} ({}): {}",
            paths.data_dir.display(),
            human_size(size),
            summary.describe()
        ),
        None => println!(
            "  - {} ({}): the job store, all history and logs",
            paths.data_dir.display(),
            human_size(size)
        ),
    }
    if purge && let Some(dir) = config_dir.as_ref().filter(|dir| dir.exists()) {
        println!("  - {} (your config)", dir.display());
    }
    println!("Kept:");
    if !purge && let Some(dir) = config_dir.as_ref().filter(|dir| dir.exists()) {
        println!("  - {} (your config; --purge removes it)", dir.display());
    }
    if let Ok(exe) = persist::daemon_exe() {
        println!(
            "  - the binary, {} (remove with `cargo uninstall cued`)",
            exe.display()
        );
    }
    if summary.as_ref().is_some_and(|summary| summary.live > 0) {
        println!(
            "
To keep a job, export it first: cued show ID --toml > job.toml"
        );
    }

    if !yes {
        ensure!(
            stdin_is_a_tty(),
            "not a terminal — rerun with --yes to confirm deleting the above"
        );
        print!(
            "
Delete all of this? It cannot be undone. [y/N] "
        );
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .context("reading your answer")?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            println!("Nothing removed.");
            return Ok(());
        }
    }

    // Backend first: systemd stops a supervised daemon itself, and nothing
    // can start a new one at boot once both backends are gone.
    for backend in &installed {
        println!("Removing {}", backend.name());
        for note in backend.uninstall()? {
            println!("  {note}");
        }
    }

    // Identified afresh rather than from the pid shown above: that daemon
    // may have exited since (systemd stops a supervised one itself, and the
    // prompt can sit for a long time), and its pid been reused.
    let daemon = ServingDaemon::find(paths);
    if let Some(daemon) = &daemon {
        daemon.terminate();
    }
    // Holding both locks proves no daemon is left, and keeps one a stray
    // client auto-spawns meanwhile from opening the store under us.
    let locks = hold_daemon_locks(paths, daemon.as_ref())?;

    let mut removed = Vec::new();
    for path in [paths.socket_file.clone(), paths.socket_lock()] {
        if remove_file_if_present(&path)? {
            removed.push(path);
        }
    }
    if remove_owned_dir(&paths.data_dir)? {
        removed.push(paths.data_dir.clone());
    }
    drop(locks);
    if purge
        && let Some(dir) = &config_dir
        && remove_owned_dir(dir)?
    {
        removed.push(dir.clone());
    }
    for path in &removed {
        println!("removed {}", path.display());
    }

    println!(
        "
cued is uninstalled."
    );
    if let Ok(exe) = persist::daemon_exe() {
        println!(
            "The binary remains at {}; `cargo uninstall cued` removes it.",
            exe.display()
        );
    }
    println!(
        "Running any cued command — including an MCP client launching `cued mcp` — \
         starts afresh with an empty store."
    );
    Ok(())
}

/// What the store holds, for the confirmation.
struct StoreSummary {
    jobs: usize,
    live: usize,
    held: usize,
    running: usize,
}

impl StoreSummary {
    fn describe(&self) -> String {
        let plural = |n: usize| if n == 1 { "" } else { "s" };
        let mut text = format!("{} job{}", self.jobs, plural(self.jobs));
        if self.live > 0 {
            text.push_str(&format!(", {} still scheduled", self.live));
        }
        if self.held > 0 {
            text.push_str(&format!(
                ", {} held run{} awaiting review",
                self.held,
                plural(self.held)
            ));
        }
        text + ", all history and logs"
    }
}

fn describe_store(jobs: &[JobEntry]) -> StoreSummary {
    let last = |status: RunStatus| {
        jobs.iter()
            .filter(|job| {
                job.last_run
                    .as_ref()
                    .is_some_and(|run| run.status == status)
            })
            .count()
    };
    StoreSummary {
        jobs: jobs.len(),
        live: jobs.iter().filter(|job| job.status.is_live()).count(),
        held: last(RunStatus::Held),
        running: last(RunStatus::Running),
    }
}

/// Whether `pid` is executing the file now at `exe` — compared by inode,
/// since a replaced binary keeps running under its old path.
fn runs_binary(pid: i32, exe: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (
        std::fs::metadata(format!("/proc/{pid}/exe")),
        std::fs::metadata(exe),
    ) {
        (Ok(running), Ok(installed)) => {
            (running.dev(), running.ino()) == (installed.dev(), installed.ino())
        }
        _ => false,
    }
}

/// Who is on the other end of the daemon socket, from the kernel rather
/// than anything the daemon says (the same peer credentials §7.3 uses).
fn peer_pid(stream: &UnixStream) -> Option<i32> {
    use std::os::fd::AsRawFd;
    // SAFETY: ucred is plain data; getsockopt writes at most `len` bytes.
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let status = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &mut len,
        )
    };
    // SAFETY: getuid is a read-only query of the current process identity.
    let ours = cred.uid == unsafe { libc::getuid() };
    (status == 0 && ours && cred.pid > 0).then_some(cred.pid)
}

/// Wait for the daemon to let go of both its locks, then hold them. For as
/// long as the daemon is alive this waits: its shutdown takes as long as
/// its slowest step's `kill_grace`, which is the user's to set, and giving
/// up would leave the account half-uninstalled.
fn hold_daemon_locks(
    paths: &Paths,
    daemon: Option<&ServingDaemon>,
) -> Result<(std::fs::File, std::fs::File)> {
    let started = std::time::Instant::now();
    let mut gone_since = None;
    let mut next_progress = started + STOP_PROGRESS_EVERY;
    loop {
        if let Some(lock) = crate::paths::try_lock(&paths.lock_file)?
            && let Some(socket_lock) = crate::paths::try_lock(&paths.socket_lock())?
        {
            return Ok((lock, socket_lock));
        }
        let now = std::time::Instant::now();
        if daemon.is_some_and(ServingDaemon::is_alive) {
            if now >= next_progress {
                eprintln!(
                    "cued: waiting for the daemon to finish terminating its running steps \
                     ({}s so far; Ctrl-C to stop waiting and rerun `cued uninstall` later)",
                    (now - started).as_secs()
                );
                next_progress = now + STOP_PROGRESS_EVERY;
            }
        } else {
            let since = *gone_since.get_or_insert(now);
            ensure!(
                now - since < UNIDENTIFIED_LOCK_WAIT,
                "something still holds the daemon's locks ({} or {}) — stop any running \
                 `cued daemon` and rerun `cued uninstall`; the persistence backend is \
                 already removed",
                paths.lock_file.display(),
                paths.socket_lock().display()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The process serving the daemon socket right now, held by pidfd where
/// the kernel offers one (Linux 6.5+), so that signalling it can never reach
/// a different process that has since been given the same pid.
enum ServingDaemon {
    Pidfd(std::os::fd::OwnedFd),
    /// Older kernels: a pid read just now from a live connection.
    Pid(i32),
}

impl ServingDaemon {
    fn find(paths: &Paths) -> Option<Self> {
        let stream = UnixStream::connect(&paths.socket_file).ok()?;
        match peer_pidfd(&stream) {
            Some(pidfd) => Some(Self::Pidfd(pidfd)),
            None => peer_pid(&stream).map(Self::Pid),
        }
    }

    fn terminate(&self) {
        use std::os::fd::AsRawFd;
        // SAFETY: signalling the process the daemon socket's peer
        // credentials named; one already exiting makes either a no-op.
        unsafe {
            match self {
                Self::Pidfd(pidfd) => {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        pidfd.as_raw_fd(),
                        libc::SIGTERM,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    );
                }
                Self::Pid(pid) => {
                    libc::kill(*pid, libc::SIGTERM);
                }
            }
        }
    }

    fn is_alive(&self) -> bool {
        use std::os::fd::AsRawFd;
        match self {
            // A pidfd turns readable once its process has exited.
            Self::Pidfd(pidfd) => {
                let mut poll = libc::pollfd {
                    fd: pidfd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: one valid pollfd, no wait.
                unsafe { libc::poll(&mut poll, 1, 0) == 0 }
            }
            // SAFETY: signal 0 only checks existence.
            Self::Pid(pid) => unsafe { libc::kill(*pid, 0) == 0 },
        }
    }
}

/// `SO_PEERPIDFD`: a pidfd for the socket's peer, straight from the kernel.
/// `None` on kernels without it, or for a peer that is not ours.
fn peer_pidfd(stream: &UnixStream) -> Option<std::os::fd::OwnedFd> {
    use std::os::fd::{AsRawFd, FromRawFd};
    peer_pid(stream)?; // same-uid check
    let mut fd: libc::c_int = -1;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: getsockopt writes at most `len` bytes into `fd`.
    let status = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERPIDFD,
            (&raw mut fd).cast(),
            &mut len,
        )
    };
    // SAFETY: on success the kernel handed us a new descriptor to own.
    (status == 0 && fd >= 0).then(|| unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
}

fn remove_file_if_present(path: &Path) -> Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("removing {}", path.display())),
    }
}

/// Recursive delete, but only of a directory that is plainly ours: named
/// `cued`, and a real directory. An XDG variable pointing somewhere odd can
/// at worst make this refuse, never widen what it removes. A symlink is
/// removed as a link; whatever it points at is left alone.
fn remove_owned_dir(dir: &Path) -> Result<bool> {
    ensure!(
        dir.file_name().is_some_and(|name| name == "cued"),
        "refusing to delete {}: not a cued directory",
        dir.display()
    );
    let metadata = match std::fs::symlink_metadata(dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).with_context(|| format!("checking {}", dir.display())),
    };
    if metadata.file_type().is_symlink() {
        std::fs::remove_file(dir)
    } else {
        std::fs::remove_dir_all(dir)
    }
    .with_context(|| format!("removing {}", dir.display()))?;
    Ok(true)
}

fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.file_type() {
            Ok(kind) if kind.is_dir() => dir_size(&entry.path()),
            Ok(kind) if kind.is_file() => entry.metadata().map(|m| m.len()).unwrap_or(0),
            _ => 0,
        })
        .sum()
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

// ---------------------------------------------------------------------------
// cued chain "./build.sh" --then "./test.sh" (§6.1)
// ---------------------------------------------------------------------------

/// The linear rung of §6's ladder: more than one step, still no branching.
/// The `then` / `then_after` vectors clap parsed are deliberately ignored —
/// they've lost the order between them, which for a pipeline is most of the
/// meaning — and the links are recovered from the raw argv instead.
fn chain(paths: &Paths, first: &str, on_fail: &str, common: SubmitCommon) -> Result<()> {
    let config = Config::load(&paths.config_file)?;
    let now = Zoned::now();

    let args: Vec<String> = std::env::args().collect();
    let links = submit::chain_links(&args)?;
    let failure = match on_fail {
        "continue" => submit::ChainFailure::Continue,
        // clap's value_parser has already refused anything else.
        _ => submit::ChainFailure::Stop,
    };
    let graph = submit::chain_graph(first, &links, failure)?;
    let steps = graph.steps.len();

    let spec = JobSpec {
        name: common.name,
        // §6.1 gives `chain` no scheduling flags, so a chain runs now — the
        // durability on offer is surviving a reboot *mid-pipeline*, not
        // waiting for a clock.
        schedule: Schedule::Once {
            at: now.timestamp(),
        },
        graph,
        cwd: std::env::current_dir()
            .context("resolving cwd")?
            .to_string_lossy()
            .into_owned(),
        env: submit::capture_env(&config.env.deny, &common.keep_env),
        policies: job_policies(&config),
        hooks: Hooks::default(),
    };

    match request(
        paths,
        RequestBody::Submit {
            spec: Box::new(spec),
        },
    )? {
        Response::Submitted { job, .. } => {
            println!(
                "{job} — {steps} step{} chained, starting now ({})",
                if steps == 1 { "" } else { "s" },
                match failure {
                    submit::ChainFailure::Stop => "stops on failure",
                    submit::ChainFailure::Continue => "continues past failures",
                }
            );
            Ok(())
        }
        other => fail_on(other),
    }
}
