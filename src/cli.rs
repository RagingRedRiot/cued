//! The clap surface (DESIGN.md §6.1). CLI = linear happy-path; branching
//! lives in TOML files (`cued submit`). Every command that takes a job
//! accepts an id (`j7`) or a live job's name interchangeably (§2).

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "cued", version, about = "Durable one-off & recurring scheduling", long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Serve exactly schedule/list/show/cancel/logs via MCP over stdio.
    Mcp,
    /// Review the stored definition and confirm approval (no noninteractive bypass).
    Approve { job: String },
    /// Schedule a one-off command: cued at "9am tomorrow" -- ./backup.sh
    At {
        /// When to run (§9 grammar: "9am tomorrow", "in 90m", "2026-06-25 09:00")
        time: String,
        /// The command. After `--`: argv, exec'd directly. As a single
        /// quoted string (no `--`): run via `sh -c` (§2.2). Everything from
        /// here on is the command, so cued's own flags go before TIME.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
        #[command(flatten)]
        common: SubmitCommon,
        /// Block until the run ends and exit with its outcome, as plain
        /// `cued wait` would (`cued wait --timeout`/`--json` for scripts).
        #[arg(long)]
        wait: bool,
    },

    /// Schedule a notification: cued remind "1h" "stretch"
    Remind {
        /// [WHEN] MESSAGE — when (§9 grammar; a bare duration means "that
        /// far from now") and what. With --every, WHEN is optional and
        /// anchors the cadence ("9am" --every "6h", §4.1).
        #[arg(required = true, num_args = 1..=2, value_names = ["WHEN", "MESSAGE"])]
        args: Vec<String>,
        /// Make it recurring: --every "day 9am" or --every "30m" (§4)
        #[arg(long)]
        every: Option<String>,
        #[command(flatten)]
        common: SubmitCommon,
    },

    /// Schedule a recurring command: cued every "30m" -- ./sync.sh
    Every {
        /// Cadence (§9 grammar: "30m", "day 09:00", "month on 1 at 9am")
        spec: String,
        /// Anchor / first firing; with an interval spec this makes an
        /// anchored Every ("every 6h starting 9am", §4.1).
        #[arg(long)]
        at: Option<String>,
        /// The command, as for `at`. Everything from here on is the
        /// command, so cued's own flags go before SPEC.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
        #[command(flatten)]
        common: SubmitCommon,
    },

    /// Schedule a linear chain: cued chain "./build.sh" --then "./test.sh"
    Chain {
        first: String,
        /// On success, go to the next link (repeatable).
        #[arg(long)]
        then: Vec<String>,
        /// Insert a durable wait before the next link: --then-after 1h CMD
        #[arg(long, value_names = ["DURATION", "CMD"], num_args = 2)]
        then_after: Vec<String>,
        /// Uniform failure policy for the whole chain.
        #[arg(long, value_parser = ["stop", "continue"], default_value = "stop")]
        on_fail: String,
        #[command(flatten)]
        common: SubmitCommon,
        /// Block until the chain ends and exit with its outcome, as plain
        /// `cued wait` would (`cued wait --timeout`/`--json` for scripts).
        #[arg(long)]
        wait: bool,
    },

    /// Submit a TOML workflow file (§6.2)
    Submit {
        file: std::path::PathBuf,
        /// Override / supply the schedule from the command line.
        #[arg(long)]
        at: Option<String>,
        #[arg(long)]
        every: Option<String>,
        /// Read the file's times as belonging to this IANA zone instead of
        /// the machine's; a `zone` key in the file does the same (§9).
        #[arg(long, value_name = "IANA")]
        zone: Option<String>,
        /// Block until the first run ends and exit with its outcome, as plain
        /// `cued wait` would (`cued wait --timeout`/`--json` for scripts).
        #[arg(long)]
        wait: bool,
    },

    /// Block until a job's run ends; the exit status is its outcome
    #[command(after_help = "\
Waits for the run in progress, else the next one to fire. A Held run is
reported at once: it needs `cued continue` or `cued retry`. Skipped firings
are records, not runs, and are passed over; --run N means exactly that run,
skipped or not yet created. On a paused job, an executing step is waited
out; anything needing the job to move is refused. Never starts a daemon.

Reports what MCP would: the outcome while MCP `read` is on (else the daemon
refuses), each step's exit code only while `logs` is on, never captured
output or environment. The daemon applies its own mcp.toml and environment;
nothing set in this shell turns a switch back on.

Exit status: 0 done, 3 failed, 4 held, 5 ended (cancelled / missed /
expired / out of runs), 124 timed out, 1 cued error (including no daemon
running), 2 usage error.
With --wait on at/chain/submit, 6: submitted, but the wait failed.")]
    Wait {
        job: String,
        /// Wait for this run instead.
        #[arg(long)]
        run: Option<i64>,
        /// Give up after this long (§9 duration: "30m", "2h").
        #[arg(long, value_parser = parse_timeout)]
        timeout: Option<std::time::Duration>,
        #[arg(long)]
        json: bool,
    },

    /// What's pending (live jobs + recently ended runs; Held always shown)
    List {
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },

    /// Inspect a job; --toml round-trips the canonical graph (§6)
    Show {
        job: String,
        #[arg(long)]
        toml: bool,
        #[arg(long)]
        json: bool,
    },

    /// Cancel a job: stop re-arming and terminate any live run (§4.2)
    Cancel { job: String },

    /// Captured output of a job's runs (read straight from log files, §5.1)
    Logs {
        job: String,
        /// A specific run (default: latest).
        #[arg(long)]
        run: Option<i64>,
        #[arg(long)]
        step: Option<String>,
        #[arg(long)]
        attempt: Option<u32>,
        /// Follow the currently running attempt.
        #[arg(short, long)]
        follow: bool,
        #[arg(long)]
        json: bool,
    },

    /// Resume a Held run from where it parked (§3.4)
    Continue { job: String },

    /// Re-run a terminal or Held run, rewinding it in place (§3.4)
    Retry {
        job: String,
        /// Restart from this step instead of the interrupted one.
        #[arg(long)]
        from: Option<String>,
    },

    /// Start nothing new for a job; a step already running finishes and is recorded (§4.2)
    Pause { job: String },

    /// Re-arm a paused job to its next future instant (never back-fills, §4.2)
    Resume { job: String },

    /// Prune terminal runs and their logs per the retention policy (§10.2)
    Gc,

    /// Install/inspect a persistence backend so the daemon survives
    /// logout/reboot (§8). With no flags: probe this host and offer what it
    /// actually supports.
    Setup {
        /// Install without prompting (§8.1's three-way).
        #[arg(long, value_parser = ["systemd-linger", "systemd", "cron"])]
        backend: Option<String>,
        /// Report what's installed and what this host supports, then stop.
        #[arg(long)]
        status: bool,
        /// Remove the installed backend (§8.2 symmetric teardown).
        #[arg(long, conflicts_with_all = ["backend", "status"])]
        uninstall: bool,
    },

    /// Remove cued from this account: the persistence backend, the running
    /// daemon, and the store with all job history and logs (§8.2). Your
    /// config files and the binary itself are kept
    Uninstall {
        /// Also remove the config directory (config.toml, mcp.toml).
        #[arg(long)]
        purge: bool,
        /// Don't ask for confirmation (required when not on a terminal).
        #[arg(long)]
        yes: bool,
    },

    /// Switch the running daemon to the installed binary without touching
    /// its persistence or store: running steps finish first, then the
    /// daemon re-executes in place (§5.2)
    Upgrade {
        /// How long to let running steps finish (§9 duration).
        #[arg(long, default_value = "10m")]
        wait: String,
        /// If steps are still running after --wait, interrupt them (they
        /// reconcile per `on_interrupt`, as after any restart) rather than
        /// abandon the upgrade.
        #[arg(long)]
        force: bool,
    },

    /// Run the daemon (what persistence backends invoke; auto-spawned by
    /// the client when absent, §5.2)
    Daemon(DaemonArgs),
}

/// Flags shared by the submitting front-ends.
#[derive(Debug, Args)]
pub struct SubmitCommon {
    /// Human name for the job (unique among live jobs).
    #[arg(long)]
    pub name: Option<String>,
    /// Retain an env var the secrets denylist would strip (§7.5).
    #[arg(long = "keep-env", value_name = "VAR")]
    pub keep_env: Vec<String>,
    /// Read the times in this command as belonging to this IANA zone
    /// instead of the machine's — "9am Eastern" while you live in Mountain
    /// (§9). Output is still shown in your own zone.
    #[arg(long, value_name = "IANA")]
    pub zone: Option<String>,
}

/// The long flags `subcommand` takes (`--wait`, `--name`, …), read from
/// its definition so a check against them can't drift from it.
pub fn long_flags(subcommand: &str) -> Vec<String> {
    use clap::CommandFactory;
    Cli::command()
        .find_subcommand(subcommand)
        .map(|command| {
            command
                .get_arguments()
                .filter_map(|arg| arg.get_long())
                .filter(|long| !matches!(*long, "help" | "version"))
                .map(|long| format!("--{long}"))
                .collect()
        })
        .unwrap_or_default()
}

/// `cued wait --timeout`: checked here, so a bad value is a usage error
/// (exit 2) like any other, never a cued failure (1) a wrapper might retry.
fn parse_timeout(text: &str) -> Result<std::time::Duration, String> {
    let timeout = crate::timeparse::parse_duration(text).map_err(|error| format!("{error:#}"))?;
    if !timeout.is_positive() {
        return Err("must be positive".into());
    }
    let timeout = timeout.unsigned_abs();
    // A deadline is an Instant; one that can't be represented can't be met.
    std::time::Instant::now()
        .checked_add(timeout)
        .ok_or("too long")?;
    Ok(timeout)
}

#[derive(Debug, Args)]
pub struct DaemonArgs {
    /// Log to stderr instead of quietly to the journal/log file.
    #[arg(long)]
    pub foreground: bool,
    /// Internal: the instance lock, socket lock and listener fds an
    /// upgrading daemon hands to its re-executed self (§5.2).
    #[arg(long, hide = true, value_name = "LOCK,SOCKET_LOCK,LISTENER")]
    pub handoff: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_at(args: &[&str]) -> Vec<String> {
        match Cli::try_parse_from(args).expect("parse").command {
            Command::At { command, .. } => command,
            other => panic!("expected At, got {other:?}"),
        }
    }

    #[test]
    fn at_takes_argv_after_separator() {
        // §2.2: after `--`, argv — including hyphenated flags.
        assert_eq!(
            parse_at(&["cued", "at", "9am", "--", "./x", "--full"]),
            ["./x", "--full"]
        );
    }

    #[test]
    fn at_takes_a_single_string_without_separator() {
        // §2.2: single quoted string, no `--` → later desugared to sh -c.
        assert_eq!(
            parse_at(&["cued", "at", "9am", "make build && make deploy"]),
            ["make build && make deploy"]
        );
    }

    #[test]
    fn remind_takes_when_message_or_just_message_with_every() {
        let parse = |args: &[&str]| match Cli::try_parse_from(args).expect("parse").command {
            Command::Remind { args, every, .. } => (args, every),
            other => panic!("expected Remind, got {other:?}"),
        };
        // cued remind "1h" "stretch"
        let (args, every) = parse(&["cued", "remind", "1h", "stretch"]);
        assert_eq!(args, ["1h", "stretch"]);
        assert_eq!(every, None);
        // cued remind --every "day 9am" "standup" — one positional (§6.1)
        let (args, every) = parse(&["cued", "remind", "--every", "day 9am", "standup"]);
        assert_eq!(args, ["standup"]);
        assert_eq!(every.as_deref(), Some("day 9am"));
        // cued remind "9am" "standup" --every "6h" — anchored cadence (§4.1)
        let (args, every) = parse(&["cued", "remind", "9am", "standup", "--every", "6h"]);
        assert_eq!(args, ["9am", "standup"]);
        assert_eq!(every.as_deref(), Some("6h"));
        // No positionals at all is a parse error.
        assert!(Cli::try_parse_from(["cued", "remind", "--every", "6h"]).is_err());
    }

    #[test]
    fn at_flags_precede_the_command() {
        let (name, command) = match Cli::try_parse_from([
            "cued",
            "at",
            "--name",
            "backup",
            "9am",
            "--",
            "./backup.sh",
        ])
        .expect("parse")
        .command
        {
            Command::At {
                command, common, ..
            } => (common.name, command),
            other => panic!("expected At, got {other:?}"),
        };
        assert_eq!(name.as_deref(), Some("backup"));
        assert_eq!(command, ["./backup.sh"]);
    }
}
