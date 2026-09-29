//! Authoring front-ends → the canonical graph (DESIGN.md §6).
//!
//! Every surface — `cued at`/`remind`/`every` one-liners, `cued chain`,
//! and TOML workflow files — desugars into `model::Graph` + `Schedule`
//! here, then passes one shared validation gate (§6.3) before anything
//! reaches the store. TOML is also the export form: `cued show --toml`
//! reserializes the same structs (the §6 round-trip promise).

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail, ensure};
use jiff::{SignedDuration, Timestamp, Zoned};
use serde::Deserialize;

use crate::model::{
    Action, CalendarSpec, CapturedEnv, CatchUp, Condition, Effect, Graph, Hooks, JobSpec,
    MissedWait, MonthDay, NotifySpec, OnInterrupt, Outcome, OutputMatch, Overlap, Policies,
    Schedule, Step, StepId, Transition, Wait,
};
use crate::timeparse;

/// §2.2: argv is canonical. A single word given *without* the `--` separator
/// is the "single quoted string" form and desugars to `sh -c`; everything
/// after `--` is argv, exec'd directly, no shell.
pub fn desugar_shell(mut command: Vec<String>, had_separator: bool) -> Vec<String> {
    if command.len() == 1 && !had_separator {
        vec!["/bin/sh".into(), "-c".into(), command.remove(0)]
    } else {
        command
    }
}

/// The degenerate §2 case: one Shell step, no transitions — fail-fast and
/// derived End cover it (§3.2).
pub fn single_shell_graph(argv: Vec<String>) -> Graph {
    let step = Step {
        action: Action::Shell { argv },
        cwd: None,
        env: None,
        timeout: None,
        kill_grace: None,
        transitions: Vec::new(),
        max_visits: None,
        restart_safe: false,
        missed_wait: None,
    };
    Graph {
        entry: "run".into(),
        steps: BTreeMap::from([("run".to_string(), step)]),
    }
}

/// The reminder twin of [`single_shell_graph`]: one Notify step, no
/// transitions — "succeeds" on durable enqueue (§3.2, §3.5).
pub fn single_notify_graph(title: String, body: String) -> Graph {
    let step = Step {
        action: Action::Notify { title, body },
        cwd: None,
        env: None,
        timeout: None,
        kill_grace: None,
        transitions: Vec::new(),
        max_visits: None,
        restart_safe: false,
        missed_wait: None,
    };
    Graph {
        entry: "remind".into(),
        steps: BTreeMap::from([("remind".to_string(), step)]),
    }
}

/// §7.5 (decided): capture the FULL environment — the whole point is that
/// the 3am run sees what the interactive shell saw — then strip the
/// configured secret patterns, recording the stripped *names* so `cued show`
/// can answer "why did the run miss $DEPLOY_TOKEN?". `--keep-env` retains a
/// var the denylist would strip, as an explicit, visible decision.
pub fn capture_env(deny: &[String], keep: &[String]) -> CapturedEnv {
    let mut captured = CapturedEnv::default();
    for (name, value) in std::env::vars() {
        let stripped =
            deny.iter().any(|pattern| glob_match(pattern, &name)) && !keep.contains(&name);
        if stripped {
            captured.stripped.push(name);
        } else {
            captured.vars.insert(name, value);
        }
    }
    captured.stripped.sort();
    captured
}

/// Just `*` wildcards — the only shapes the §10.1 denylist uses
/// (`*_TOKEN`, `*PASSWORD*`). Case-sensitive, like env names.
fn glob_match(pattern: &str, name: &str) -> bool {
    let pieces: Vec<&str> = pattern.split('*').collect();
    if pieces.len() == 1 {
        return pattern == name;
    }
    let mut rest = name;
    for (index, piece) in pieces.iter().enumerate() {
        if piece.is_empty() {
            continue;
        }
        if index == 0 {
            let Some(after) = rest.strip_prefix(piece) else {
                return false;
            };
            rest = after;
        } else if index == pieces.len() - 1 {
            return rest.ends_with(piece);
        } else if let Some(found) = rest.find(piece) {
            rest = &rest[found + piece.len()..];
        } else {
            return false;
        }
    }
    true
}

/// The §6.3 gate, run on every submission regardless of front-end.
///
/// Takes the whole spec rather than the graph alone because §6.3 asks for
/// "a valid schedule" too, and the daemon — not the CLI — is the trust
/// boundary here: a client speaking the §5.1 protocol directly can put any
/// `Schedule` on the wire, while `timeparse` only constrains what the CLI
/// and §6.2 files can express. One entry point, so a front-end can't pass
/// half the gate.
pub fn validate(spec: &JobSpec) -> Result<()> {
    check_schedule(&spec.schedule)?;
    check_graph(&spec.graph, &spec.policies)
}

/// §6.3: "A valid schedule: `at` alone (Once), `every` alone (anchored now),
/// or both (anchored Every — §4.1); `until`/`count` only on recurring."
///
/// The last clause needs no check: `Schedule::Once` has no `until`/`count`
/// fields, so the model already makes that unrepresentable. What's left is
/// the values inside a recurring one, each of which otherwise fails much
/// later and much less clearly — a zero interval divides by zero in §4.2's
/// catch-up arithmetic, and an empty weekday set walks a thousand days
/// looking for a firing that can't exist.
fn check_schedule(schedule: &Schedule) -> Result<()> {
    match schedule {
        Schedule::Once { .. } => Ok(()),
        Schedule::Every {
            interval,
            until,
            count,
            ..
        } => {
            ensure!(
                interval.is_positive(),
                "an `every` interval must be positive (got {interval:#})"
            );
            check_limits(until, count)
        }
        Schedule::Calendar {
            spec,
            zone,
            until,
            count,
        } => {
            jiff::tz::TimeZone::get(zone).with_context(|| format!("unknown time zone {zone:?}"))?;
            check_calendar(spec)?;
            check_limits(until, count)
        }
    }
}

fn check_limits(until: &Option<Timestamp>, count: &Option<u32>) -> Result<()> {
    // A past `until` is caught downstream with a good message ("schedule has
    // no future firing"); a zero `count` is not — it quietly produces a job
    // that finishes without ever running, which reads as cued losing it.
    ensure!(
        !matches!(count, Some(0)),
        "`count` is how many firings to allow — 0 would mean never (drop the job instead)"
    );
    let _ = until;
    Ok(())
}

/// §9.2's v1 tier. The grammar can't produce these, but the wire can.
fn check_calendar(spec: &CalendarSpec) -> Result<()> {
    match spec {
        CalendarSpec::Daily { .. } => Ok(()),
        CalendarSpec::Weekly { days, .. } => {
            ensure!(!days.is_empty(), "a weekly rule needs at least one weekday");
            Ok(())
        }
        CalendarSpec::Monthly { days, .. } => {
            ensure!(!days.is_empty(), "a monthly rule needs at least one day");
            for day in days {
                if let MonthDay::Day(number) = day {
                    // §9.2 clamps 29–31 onto short months; 0 or 32+ is not a
                    // day of any month and would simply never match.
                    ensure!(
                        (1..=31).contains(number),
                        "month day {number} doesn't exist — 1-31, or \"last\""
                    );
                }
            }
            Ok(())
        }
    }
}

fn check_graph(graph: &Graph, policies: &Policies) -> Result<()> {
    ensure!(!graph.steps.is_empty(), "a job needs at least one step");
    ensure!(
        graph.steps.contains_key(&graph.entry),
        "entry step {:?} doesn't exist",
        graph.entry
    );

    for (id, step) in &graph.steps {
        check_step_id(id)?;
        if let Action::Shell { argv } = &step.action {
            ensure!(!argv.is_empty(), "step {id:?}: argv is empty");
            ensure!(!argv[0].is_empty(), "step {id:?}: argv[0] is empty");
        }

        for (index, transition) in step.transitions.iter().enumerate() {
            if let Effect::Goto { step: target, .. } = &transition.then {
                ensure!(
                    graph.steps.contains_key(target),
                    "step {id:?}, transition {index}: goto target {target:?} doesn't exist"
                );
            }
            if matches!(step.action, Action::Notify { .. }) {
                check_notify_condition(id, &transition.when)?;
            }
        }
    }

    // After the goto targets are known to exist, so the walk below can
    // trust every edge it follows.
    check_loops_are_bounded(graph, policies)
}

/// §6.3: "Every back-edge sits under a bound (`max_visits` and/or job
/// `deadline`) so a loop can't run forever."
///
/// The two bounds are genuinely alternatives: `max_visits` caps how often
/// one step may be re-entered (§3.2), while a `deadline` caps the whole run
/// in wall-clock time and kills it mid-step when it expires. Either makes
/// every loop in the graph finite, so a job with a deadline needs no
/// per-step cap at all.
fn check_loops_are_bounded(graph: &Graph, policies: &Policies) -> Result<()> {
    if policies.deadline.is_some() {
        return Ok(());
    }
    for (from, to) in back_edges(graph) {
        ensure!(
            graph.steps[&to].max_visits.is_some(),
            "step {from:?} loops back to {to:?} with nothing to stop it — \
             give {to:?} a `max_visits`, or the job a `deadline` (§6.3)"
        );
    }
    Ok(())
}

/// Edges that close a cycle: an edge into a step already on the current
/// path.
///
/// Walked from every step rather than only from `entry`, because a step
/// unreachable from the entry is not necessarily unreachable — `cued retry
/// --from <step>` starts a run anywhere in the graph (§3.4). A loop that can
/// only be entered that way still has to be bounded.
///
/// Iterative rather than recursive: this runs daemon-side on a graph that
/// arrived over the socket, and a deep enough one shouldn't be able to take
/// the scheduler down with a blown stack.
fn back_edges(graph: &Graph) -> Vec<(StepId, StepId)> {
    /// Unvisited / on the current path / finished — the standard three-
    /// colour marking, which is what makes a *back* edge distinguishable
    /// from a mere revisit.
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        White,
        Gray,
        Black,
    }

    let mut marks: BTreeMap<&str, Mark> = graph
        .steps
        .keys()
        .map(|id| (id.as_str(), Mark::White))
        .collect();
    let mut found = Vec::new();

    for root in graph.steps.keys() {
        if marks[root.as_str()] != Mark::White {
            continue;
        }
        marks.insert(root.as_str(), Mark::Gray);
        // (step, how many of its edges we've already followed)
        let mut stack: Vec<(&str, usize)> = vec![(root.as_str(), 0)];

        while let Some(&mut (node, ref mut edge)) = stack.last_mut() {
            let targets = goto_targets(graph, node);
            let Some(next) = targets.get(*edge) else {
                marks.insert(node, Mark::Black);
                stack.pop();
                continue;
            };
            *edge += 1;
            match marks.get(next) {
                Some(Mark::White) => {
                    marks.insert(next, Mark::Gray);
                    stack.push((next, 0));
                }
                // Already on the path we're standing on: this edge closes
                // a cycle.
                Some(Mark::Gray) => found.push((node.to_string(), (*next).to_string())),
                _ => {}
            }
        }
    }
    found
}

fn goto_targets<'a>(graph: &'a Graph, step: &str) -> Vec<&'a str> {
    graph.steps[step]
        .transitions
        .iter()
        .filter_map(|transition| match &transition.then {
            Effect::Goto { step, .. } => Some(step.as_str()),
            Effect::End { .. } => None,
        })
        // Targets are checked to exist before this runs; filtering keeps
        // the walk total rather than relying on that ordering.
        .filter(|target| graph.steps.contains_key(*target))
        .collect()
}

/// A step id is a user-given label (§3.1) that becomes part of a filename:
/// §2.1's per-attempt log is `<step>.<attempt>.log`. An id carrying `/` or
/// `..` would put that file outside the run's directory — somewhere
/// `cued logs` can't read it and §10.2's GC can't remove it — so the gate
/// constrains ids to what is safely a single path component.
///
/// Front-ends that generate their own ids (`run`, `remind`, `step1`…) are
/// well within this; it exists for the ids a §6.2 file can carry, which are
/// whatever the file's author typed.
fn check_step_id(id: &StepId) -> Result<()> {
    ensure!(!id.is_empty(), "a step id can't be empty");
    ensure!(
        id.len() <= 64,
        "step id {id:?} is too long — 64 characters at most, since it becomes a filename"
    );
    ensure!(
        id.bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'),
        "step id {id:?} — letters, digits, '-' and '_' only; ids become \
         filenames (§2.1), so they can't carry path separators"
    );
    Ok(())
}

/// §3.1: exit / stdout / stderr conditions are meaningless on a Notify step
/// — only Succeeded / Failed / TimedOut (and the Always fallthrough) apply.
fn check_notify_condition(step: &StepId, condition: &Condition) -> Result<()> {
    match condition {
        Condition::Always | Condition::Succeeded | Condition::Failed | Condition::TimedOut => {
            Ok(())
        }
        Condition::All(inner) => {
            for condition in inner {
                check_notify_condition(step, condition)?;
            }
            Ok(())
        }
        Condition::ExitEq(_)
        | Condition::ExitNe(_)
        | Condition::ExitIn(_)
        | Condition::Stdout(_)
        | Condition::Stderr(_) => bail!(
            "step {step:?} is a notify step — exit/stdout/stderr conditions \
             don't apply (use succeeded/failed/timed_out)"
        ),
    }
}

// ---------------------------------------------------------------------------
// cued chain (§6.1) — the linear middle of the authoring ladder
// ---------------------------------------------------------------------------

/// One link of a chain, in the order it was typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// `--then CMD`
    Then(String),
    /// `--then-after DURATION CMD` — a durable sleep-edge before this link.
    ThenAfter(String, String),
}

/// §6.1's uniform failure policy. Stop is the default and is just §3.2's
/// fail-fast: no edge handles the failure, so the run ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainFailure {
    Stop,
    Continue,
}

/// Build the canonical graph for `cued chain` (§6.1): a straight line of
/// `sh -c` steps, each reaching the next on success — or unconditionally,
/// under `--on-fail continue`. No branching is expressible here by design;
/// that's what §6.2's file is for.
pub fn chain_graph(first: &str, links: &[Link], on_fail: ChainFailure) -> Result<Graph> {
    // (command, the wait that precedes it). The duration on `--then-after`
    // describes the *edge into* that link, not the link itself.
    let mut commands: Vec<(&str, Option<SignedDuration>)> = vec![(first, None)];
    for link in links {
        match link {
            Link::Then(command) => commands.push((command, None)),
            Link::ThenAfter(duration, command) => {
                let wait = timeparse::parse_duration(duration)
                    .with_context(|| format!("--then-after {duration:?}"))?;
                ensure!(
                    wait.is_positive(),
                    "--then-after {duration:?} isn't a forward wait"
                );
                commands.push((command, Some(wait)));
            }
        }
    }

    let ids: Vec<String> = (1..=commands.len()).map(|n| format!("step{n}")).collect();
    let mut steps = BTreeMap::new();
    for (index, (command, _)) in commands.iter().enumerate() {
        let mut step = Step {
            action: Action::Shell {
                // §2.2: each link is one quoted string, so it's the shell
                // form — argv per link would need a flag §6.1 doesn't have.
                argv: desugar_shell(vec![(*command).to_string()], false),
            },
            cwd: None,
            env: None,
            timeout: None,
            kill_grace: None,
            transitions: Vec::new(),
            max_visits: None,
            restart_safe: false,
            missed_wait: None,
        };
        if let Some(next) = ids.get(index + 1) {
            step.transitions = vec![Transition {
                when: match on_fail {
                    // §3.2 fail-fast does the rest: with no edge matching a
                    // failure, the run ends there.
                    ChainFailure::Stop => Condition::Succeeded,
                    ChainFailure::Continue => Condition::Always,
                },
                then: Effect::Goto {
                    step: next.clone(),
                    after: commands[index + 1].1.map(Wait::In),
                },
            }];
        }
        steps.insert(ids[index].clone(), step);
    }

    Ok(Graph {
        entry: ids[0].clone(),
        steps,
    })
}

/// Recover the order the links were typed in.
///
/// clap collects `--then` and `--then-after` into two separate vectors,
/// which loses the interleaving between them: `--then-after 1h B --then C`
/// and `--then C --then-after 1h B` produce identical vectors and two very
/// different pipelines. Since a chain is defined by its order, that has to
/// come from the raw argv.
///
/// Value-taking flags are skipped whole, so a `--name --then` can't have its
/// value mistaken for the start of a link.
pub fn chain_links(args: &[String]) -> Result<Vec<Link>> {
    const VALUE_FLAGS: [&str; 3] = ["--name", "--keep-env", "--on-fail"];

    let mut links = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();

        if let Some(duration) = arg.strip_prefix("--then-after=") {
            let command = args
                .get(index + 1)
                .context("--then-after needs a DURATION and a CMD")?;
            links.push(Link::ThenAfter(duration.to_string(), command.clone()));
            index += 2;
        } else if arg == "--then-after" {
            let duration = args
                .get(index + 1)
                .context("--then-after needs a DURATION and a CMD")?;
            let command = args
                .get(index + 2)
                .context("--then-after needs a CMD after its DURATION")?;
            links.push(Link::ThenAfter(duration.clone(), command.clone()));
            index += 3;
        } else if let Some(command) = arg.strip_prefix("--then=") {
            links.push(Link::Then(command.to_string()));
            index += 1;
        } else if arg == "--then" {
            let command = args.get(index + 1).context("--then needs a CMD")?;
            links.push(Link::Then(command.clone()));
            index += 2;
        } else if VALUE_FLAGS.contains(&arg) {
            index += 2;
        } else {
            index += 1;
        }
    }
    Ok(links)
}

// ---------------------------------------------------------------------------
// The §6.2 TOML workflow file → the canonical graph
// ---------------------------------------------------------------------------
//
// The reading half of §6's round-trip promise; `export::job_to_toml` is the
// writing half. Every field below is `deny_unknown_fields` on purpose: a
// scheduler that silently ignores a misspelled `on_faliure` would run a job
// that isn't the one you wrote, and you'd find out at 3am.

/// Everything the file itself can say. CLI `--at` / `--every` override the
/// scheduling keys (§6.2).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileJob {
    name: Option<String>,
    /// Optional when the file has exactly one step — there's nothing to be
    /// ambiguous about — and required past that.
    entry: Option<String>,
    at: Option<String>,
    every: Option<String>,
    /// The zone the file's wall-clock times are to be read in (§9) — what
    /// `cued show --toml` writes so an export resubmits to the same instant
    /// anywhere, and what you write by hand to say "this schedule is in
    /// Eastern" from a machine that isn't.
    zone: Option<String>,
    until: Option<String>,
    count: Option<u32>,
    catch_up: Option<String>,
    overlap: Option<String>,
    defaults: Option<FileDefaults>,
    on_hold: Option<FileNotify>,
    on_failure: Option<FileNotify>,
    on_success: Option<FileNotify>,
    on_missed: Option<FileNotify>,
    #[serde(default)]
    step: Vec<FileStep>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileDefaults {
    cwd: Option<String>,
    /// Overlaid on top of the captured environment (§2.1) rather than
    /// replacing it — capture is what stops a job working interactively and
    /// failing at 3am, and a file shouldn't be able to switch that off by
    /// pinning three variables.
    env: Option<BTreeMap<String, String>>,
    deadline: Option<String>,
    on_interrupt: Option<String>,
    missed_wait: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileNotify {
    title: String,
    body: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileStep {
    id: String,
    run: Option<FileRun>,
    notify: Option<FileNotify>,
    cwd: Option<String>,
    env: Option<BTreeMap<String, String>>,
    timeout: Option<String>,
    kill_grace: Option<String>,
    max_visits: Option<u32>,
    #[serde(default)]
    restart_safe: bool,
    missed_wait: Option<String>,
    /// The sugar tier: `on.success`, `on.fail`, `on.timeout`, `on.always`.
    on: Option<FileOn>,
    /// The escape-hatch tier: `[[step.transition]]`.
    #[serde(default)]
    transition: Vec<FileTransition>,
}

/// §2.2: a string is the `sh -c` form; an array is argv.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum FileRun {
    Argv(Vec<String>),
    Script(String),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileOn {
    success: Option<FileEffect>,
    fail: Option<FileEffect>,
    timeout: Option<FileEffect>,
    always: Option<FileEffect>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileTransition {
    when: FileCondition,
    then: FileEffect,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum FileCondition {
    Word(String),
    Table(Box<FileConditionTable>),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConditionTable {
    exit: Option<i32>,
    exit_ne: Option<i32>,
    exit_in: Option<Vec<i32>>,
    stdout_contains: Option<String>,
    stdout_matches: Option<String>,
    stderr_contains: Option<String>,
    stderr_matches: Option<String>,
    all: Option<Vec<FileCondition>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileEffect {
    goto: Option<String>,
    end: Option<String>,
    after: Option<FileAfter>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum FileAfter {
    Duration(String),
    Table(FileAfterTable),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileAfterTable {
    until: Option<String>,
    start: Option<String>,
    factor: Option<f64>,
    max: Option<String>,
}

/// What the submitting side knows that the file doesn't.
pub struct SubmitContext<'a> {
    pub now: &'a Zoned,
    /// CLI `--at` / `--every`, which override the file's own keys.
    pub at: Option<&'a str>,
    pub every: Option<&'a str>,
    /// `--zone` from the command line. More specific than the file's own
    /// `zone` key, so it wins; both are more specific than the machine's.
    pub zone: Option<&'a str>,
    /// Captured per §2.1/§7.5 before we get here; `[defaults] env` overlays.
    pub env: CapturedEnv,
    /// §10.1's precedence is built-in < config file < job < step, so the
    /// file's `[defaults]` overlay *these* — the user's configured defaults
    /// — rather than the built-in enum values. Passing them in is what makes
    /// `cued submit` obey a config the CLI front-ends already obeyed.
    pub policies: Policies,
    /// Where the client was run, unless `[defaults] cwd` says otherwise.
    pub cwd: &'a str,
}

/// Parse + desugar a TOML workflow file (§6.2) into the one canonical
/// representation every front-end produces. Validation (§6.3) is the
/// caller's next step and the daemon's regardless.
pub fn from_toml(text: &str, context: SubmitContext<'_>) -> Result<JobSpec> {
    let file: FileJob = toml::from_str(text).context("parsing the workflow file")?;
    ensure!(
        !file.step.is_empty(),
        "a workflow file needs at least one [[step]]"
    );

    // §9's inbound translation, resolved once for the whole file: the
    // command line wins, then the file's own key, then the machine's zone.
    // Everything below reads its wall clocks against this.
    let zone = match (context.zone, file.zone.as_deref()) {
        (Some(named), _) | (None, Some(named)) => timeparse::resolve_zone(Some(named))?,
        (None, None) => context.now.time_zone().clone(),
    };
    // The same instant, re-expressed in the zone we are reading against.
    let now = context.now.timestamp().to_zoned(zone);
    let context = SubmitContext {
        now: &now,
        ..context
    };

    let entry = match &file.entry {
        Some(entry) => entry.clone(),
        None => {
            ensure!(
                file.step.len() == 1,
                "{} steps but no `entry` — say which one a run starts at",
                file.step.len()
            );
            file.step[0].id.clone()
        }
    };

    let mut steps = BTreeMap::new();
    for step in &file.step {
        let built = build_step(step, context.now)?;
        ensure!(
            steps.insert(step.id.clone(), built).is_none(),
            "two steps share the id {:?} — ids are unique within a job (§3.1)",
            step.id
        );
    }

    // Built before the context is consumed for its captured environment.
    let schedule = build_schedule(&file, &context)?;
    let base_policies = context.policies.clone();

    let defaults = file.defaults.as_ref();
    let mut env = context.env;
    if let Some(overrides) = defaults.and_then(|d| d.env.as_ref()) {
        for (name, value) in overrides {
            // An explicit pin wins over both the capture and the denylist:
            // writing it in the file is the visible, deliberate decision
            // §7.5 asks for.
            env.stripped.retain(|stripped| stripped != name);
            env.vars.insert(name.clone(), value.clone());
        }
    }

    Ok(JobSpec {
        name: file.name.clone(),
        schedule,
        graph: Graph { entry, steps },
        cwd: defaults
            .and_then(|d| d.cwd.clone())
            .unwrap_or_else(|| context.cwd.to_string()),
        env,
        policies: build_policies(&file, defaults, &base_policies)?,
        hooks: Hooks {
            on_hold: file.on_hold.as_ref().map(notify_spec),
            on_failure: file.on_failure.as_ref().map(notify_spec),
            on_success: file.on_success.as_ref().map(notify_spec),
            on_missed: file.on_missed.as_ref().map(notify_spec),
        },
    })
}

fn notify_spec(notify: &FileNotify) -> NotifySpec {
    NotifySpec {
        title: notify.title.clone(),
        body: notify.body.clone().unwrap_or_default(),
    }
}

// ---------------------------------------------------------------------------
// Schedule (§4.1, §6.3)
// ---------------------------------------------------------------------------

fn build_schedule(file: &FileJob, context: &SubmitContext<'_>) -> Result<Schedule> {
    let at = context.at.or(file.at.as_deref());
    let every = context.every.or(file.every.as_deref());

    let mut schedule = match (at, every) {
        (Some(at), None) => Schedule::Once {
            at: timeparse::parse_instant(at, context.now)?,
        },
        (at, Some(every)) => every_schedule(every, at, context.now)?,
        (None, None) => bail!(
            "no schedule — give the file an `at` or `every` key, \
             or pass --at / --every"
        ),
    };

    // §6.3: until/count are recurrence limits; on a one-off they'd be
    // meaningless, and silently dropping them would hide a real mistake.
    if let Schedule::Once { .. } = schedule {
        ensure!(
            file.until.is_none() && file.count.is_none(),
            "`until` / `count` only apply to a recurring schedule (§4.1)"
        );
        return Ok(schedule);
    }

    let until = file
        .until
        .as_deref()
        .map(|text| timeparse::parse_instant(text, context.now))
        .transpose()?;
    let count = file.count;
    match &mut schedule {
        Schedule::Once { .. } => unreachable!("handled above"),
        Schedule::Every {
            until: slot,
            count: cap,
            ..
        } => {
            *slot = until;
            *cap = count;
        }
        Schedule::Calendar {
            until: slot,
            count: cap,
            ..
        } => {
            // The rule's zone is already the reading zone — `every_schedule`
            // takes it from `now`, and `now` was re-zoned above.
            *slot = until;
            *cap = count;
        }
    }
    Ok(schedule)
}

/// §4.1's three spellings: a duration alone anchors at submit time; a
/// duration plus an `at` is an anchored Every ("every 6h starting 9am"); a
/// calendar rule ("day 9am", "month on 1 at 9:00") carries its own time.
///
/// Shared by `cued every`, `cued remind --every` and the file's `every` key:
/// one desugaring, so a cadence means the same thing however it was written
/// (§6's single canonical representation).
/// `now` is zoned because this is the *inbound* side of §9's translation
/// layer: the zone is what an anchor like "9am" is read against, and what a
/// `Calendar` rule goes on meaning. What comes back holds UTC instants, plus
/// — for `Calendar` alone — the zone as part of the rule.
pub fn every_schedule(spec: &str, at: Option<&str>, now: &Zoned) -> Result<Schedule> {
    if let Ok(interval) = timeparse::parse_duration(spec) {
        ensure!(interval.is_positive(), "an interval must be positive");
        let anchor = match at {
            Some(time) => timeparse::parse_instant(time, now)?,
            None => now.timestamp(),
        };
        return Ok(Schedule::Every {
            interval,
            anchor,
            until: None,
            count: None,
        });
    }
    let calendar = timeparse::parse_calendar(spec)?;
    ensure!(
        at.is_none(),
        "calendar rules carry their own time (\"{spec}\") — drop the separate time"
    );
    // §9: a calendar rule is not an instant, so its zone is meaning rather
    // than presentation and has to be carried, not converted away.
    let zone = now.time_zone().iana_name().unwrap_or("UTC").to_string();
    Ok(Schedule::Calendar {
        spec: calendar,
        zone,
        until: None,
        count: None,
    })
}

/// §10.1: built-in < config file < job < step. `base` is everything to the
/// left of the file — the built-ins already folded into the user's config —
/// so a key the file omits falls back to what they configured, not to the
/// enum default. Skipping that layer meant `cued submit` silently ignored a
/// config every other front-end honoured.
fn build_policies(
    file: &FileJob,
    defaults: Option<&FileDefaults>,
    base: &Policies,
) -> Result<Policies> {
    Ok(Policies {
        missed_wait: match defaults.and_then(|d| d.missed_wait.as_deref()) {
            Some(word) => parse_missed_wait(word)?,
            None => base.missed_wait,
        },
        on_interrupt: match defaults.and_then(|d| d.on_interrupt.as_deref()) {
            Some("hold") => OnInterrupt::Hold,
            Some("fail") => OnInterrupt::Fail,
            Some("retry") => OnInterrupt::Retry,
            Some(other) => bail!("unknown on_interrupt {other:?} — hold | fail | retry"),
            None => base.on_interrupt,
        },
        catch_up: match file.catch_up.as_deref() {
            Some("run_once") => CatchUp::RunOnce,
            Some("skip") => CatchUp::Skip,
            Some(other) => bail!("unknown catch_up {other:?} — run_once | skip"),
            None => base.catch_up,
        },
        overlap: match file.overlap.as_deref() {
            Some("skip") => Overlap::Skip,
            Some("queue") => Overlap::Queue,
            Some(other) => bail!("unknown overlap {other:?} — skip | queue"),
            None => base.overlap,
        },
        deadline: match defaults.and_then(|d| d.deadline.as_deref()) {
            Some(text) => Some(timeparse::parse_duration(text)?),
            None => base.deadline,
        },
    })
}

fn parse_missed_wait(word: &str) -> Result<MissedWait> {
    match word {
        "run_asap" => Ok(MissedWait::RunAsap),
        "abandon" => Ok(MissedWait::Abandon),
        other => bail!("unknown missed_wait {other:?} — run_asap | abandon"),
    }
}

// ---------------------------------------------------------------------------
// Steps & transitions (§3.1, §6.2)
// ---------------------------------------------------------------------------

fn build_step(step: &FileStep, now: &Zoned) -> Result<Step> {
    let action = match (&step.run, &step.notify) {
        (Some(run), None) => Action::Shell {
            argv: match run {
                // §2.2: a bare string is the shell form; an array is argv.
                FileRun::Script(script) => {
                    vec!["/bin/sh".into(), "-c".into(), script.clone()]
                }
                FileRun::Argv(argv) => argv.clone(),
            },
        },
        (None, Some(notify)) => Action::Notify {
            title: notify.title.clone(),
            body: notify.body.clone().unwrap_or_default(),
        },
        (Some(_), Some(_)) => bail!(
            "step {:?} has both `run` and `notify` — a step is one action (§2)",
            step.id
        ),
        (None, None) => bail!("step {:?} needs a `run` or a `notify`", step.id),
    };

    Ok(Step {
        action,
        cwd: step.cwd.clone(),
        env: step.env.clone(),
        timeout: step
            .timeout
            .as_deref()
            .map(timeparse::parse_duration)
            .transpose()?,
        kill_grace: step
            .kill_grace
            .as_deref()
            .map(timeparse::parse_duration)
            .transpose()?,
        transitions: build_transitions(step, now)?,
        max_visits: step.max_visits,
        restart_safe: step.restart_safe,
        missed_wait: step
            .missed_wait
            .as_deref()
            .map(parse_missed_wait)
            .transpose()?,
    })
}

/// The two tiers of §6.2, which are deliberately not mixed. Sugar desugars
/// into an ordered list; explicit transitions *are* an ordered list. Letting
/// a file use both would mean silently interleaving them, and since §3.2 is
/// first-match-wins, an `on.always` landing ahead of a hand-written edge
/// would shadow it. Better to say so than to guess.
fn build_transitions(step: &FileStep, now: &Zoned) -> Result<Vec<Transition>> {
    let sugar = step.on.as_ref();
    ensure!(
        sugar.is_none() || step.transition.is_empty(),
        "step {:?} mixes `on.*` sugar with [[step.transition]] — \
         use one tier or the other, since both are ordered and first match wins (§3.2)",
        step.id
    );

    if let Some(on) = sugar {
        // §6.2's fixed precedence: timeout, then fail / success, then always.
        let mut out = Vec::new();
        for (condition, effect) in [
            (Condition::TimedOut, &on.timeout),
            (Condition::Failed, &on.fail),
            (Condition::Succeeded, &on.success),
            (Condition::Always, &on.always),
        ] {
            if let Some(effect) = effect {
                out.push(Transition {
                    when: condition,
                    then: build_effect(effect, &step.id, now)?,
                });
            }
        }
        return Ok(out);
    }

    step.transition
        .iter()
        .map(|transition| {
            Ok(Transition {
                when: build_condition(&transition.when, &step.id)?,
                then: build_effect(&transition.then, &step.id, now)?,
            })
        })
        .collect()
}

fn build_condition(when: &FileCondition, step: &str) -> Result<Condition> {
    match when {
        FileCondition::Word(word) => match word.as_str() {
            "always" => Ok(Condition::Always),
            "succeeded" => Ok(Condition::Succeeded),
            "failed" => Ok(Condition::Failed),
            "timed_out" => Ok(Condition::TimedOut),
            other => bail!(
                "step {step:?}: unknown condition {other:?} — \
                 always | succeeded | failed | timed_out, or a table like {{ exit = 0 }}"
            ),
        },
        FileCondition::Table(table) => {
            let mut found: Vec<Condition> = Vec::new();
            if let Some(code) = table.exit {
                found.push(Condition::ExitEq(code));
            }
            if let Some(code) = table.exit_ne {
                found.push(Condition::ExitNe(code));
            }
            if let Some(codes) = &table.exit_in {
                found.push(Condition::ExitIn(codes.clone()));
            }
            if let Some(needle) = &table.stdout_contains {
                found.push(Condition::Stdout(OutputMatch::Contains(needle.clone())));
            }
            if let Some(pattern) = &table.stdout_matches {
                found.push(Condition::Stdout(OutputMatch::Regex(pattern.clone())));
            }
            if let Some(needle) = &table.stderr_contains {
                found.push(Condition::Stderr(OutputMatch::Contains(needle.clone())));
            }
            if let Some(pattern) = &table.stderr_matches {
                found.push(Condition::Stderr(OutputMatch::Regex(pattern.clone())));
            }
            if let Some(inner) = &table.all {
                let inner: Result<Vec<Condition>> = inner
                    .iter()
                    .map(|condition| build_condition(condition, step))
                    .collect();
                found.push(Condition::All(inner?));
            }
            match found.len() {
                1 => Ok(found.remove(0)),
                0 => bail!("step {step:?}: a `when` table needs a condition key"),
                // Two keys in one table reads like AND but isn't spelled
                // like it; §3.2 has exactly one way to say AND.
                _ => bail!(
                    "step {step:?}: several conditions in one `when` table — \
                     combine them explicitly with {{ all = [...] }} (§3.2)"
                ),
            }
        }
    }
}

fn build_effect(effect: &FileEffect, step: &str, now: &Zoned) -> Result<Effect> {
    match (&effect.goto, &effect.end) {
        (Some(target), None) => Ok(Effect::Goto {
            step: target.clone(),
            after: effect
                .after
                .as_ref()
                .map(|a| build_wait(a, now))
                .transpose()?,
        }),
        (None, Some(outcome)) => {
            ensure!(
                effect.after.is_none(),
                "step {step:?}: `after` delays a goto; an `end` has nothing to wait for"
            );
            match outcome.as_str() {
                "success" => Ok(Effect::End {
                    outcome: Outcome::Success,
                }),
                "failure" => Ok(Effect::End {
                    outcome: Outcome::Failure,
                }),
                other => bail!("step {step:?}: unknown end {other:?} — success | failure"),
            }
        }
        (Some(_), Some(_)) => {
            bail!("step {step:?}: a transition is either a `goto` or an `end`, not both")
        }
        (None, None) => bail!("step {step:?}: a transition needs a `goto` or an `end`"),
    }
}

fn build_wait(after: &FileAfter, now: &Zoned) -> Result<Wait> {
    match after {
        FileAfter::Duration(text) => Ok(Wait::In(timeparse::parse_duration(text)?)),
        FileAfter::Table(table) => {
            if let Some(until) = &table.until {
                ensure!(
                    table.start.is_none() && table.factor.is_none() && table.max.is_none(),
                    "`after` is either {{ until = … }} or a backoff table, not both"
                );
                // A civil instant needs a reference for "9am" to mean
                // anything; an `until` in a file is expected to be absolute.
                return Ok(Wait::Until(timeparse::parse_instant(until, now)?));
            }
            let (start, factor, max) = (
                table.start.as_deref().context("backoff needs `start`")?,
                table.factor.context("backoff needs `factor`")?,
                table.max.as_deref().context("backoff needs `max`")?,
            );
            ensure!(
                factor >= 1.0,
                "a backoff factor below 1 shrinks the delay (got {factor})"
            );
            Ok(Wait::Backoff {
                start: timeparse::parse_duration(start)?,
                factor,
                max: timeparse::parse_duration(max)?,
            })
        }
    }
}

/// Wrap a graph in the smallest spec that will carry it to the §6.3 gate.
/// A `Once` schedule is the least interesting one there is, which is what
/// the graph-shaped tests want.
#[cfg(test)]
fn spec_for(graph: Graph, policies: Policies) -> JobSpec {
    JobSpec {
        name: None,
        schedule: Schedule::Once {
            at: "2026-07-16T09:00:00-06:00[America/Denver]"
                .parse()
                .expect("instant"),
        },
        graph,
        cwd: "/tmp".into(),
        env: CapturedEnv::default(),
        policies,
        hooks: Hooks::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{NotifySpec, Transition};

    #[test]
    fn desugar_single_string_vs_argv() {
        // Single string, no `--` → sh -c (§2.2).
        let sugared = desugar_shell(vec!["make a && make b".into()], false);
        assert_eq!(sugared, ["/bin/sh", "-c", "make a && make b"]);
        // After `--`: argv as-is, even a single word.
        let direct = desugar_shell(vec!["./backup.sh".into()], true);
        assert_eq!(direct, ["./backup.sh"]);
        // Multiple words are argv regardless.
        let multi = desugar_shell(vec!["./x".into(), "--full".into()], false);
        assert_eq!(multi, ["./x", "--full"]);
    }

    #[test]
    fn glob_matches_the_denylist_shapes() {
        assert!(glob_match("*_TOKEN", "DEPLOY_TOKEN"));
        assert!(!glob_match("*_TOKEN", "TOKEN_PATH"));
        assert!(glob_match("*PASSWORD*", "MY_PASSWORD_FILE"));
        assert!(glob_match("*PASSWORD*", "PASSWORD"));
        assert!(glob_match("AWS_*", "AWS_REGION"));
        assert!(!glob_match("AWS_*", "NOT_AWS"));
        assert!(glob_match("EXACT", "EXACT"));
        assert!(!glob_match("EXACT", "EXACTLY"));
    }

    #[test]
    fn capture_strips_and_records_with_keep_override() {
        // SAFETY: test-local vars, no concurrent env readers we care about.
        unsafe {
            std::env::set_var("CUED_TEST_PLAIN", "1");
            std::env::set_var("CUED_TEST_API_TOKEN", "hunter2");
            std::env::set_var("CUED_TEST_KEPT_TOKEN", "hunter3");
        }
        let deny = vec!["*_TOKEN".to_string()];
        let keep = vec!["CUED_TEST_KEPT_TOKEN".to_string()];
        let captured = capture_env(&deny, &keep);

        assert_eq!(
            captured.vars.get("CUED_TEST_PLAIN").map(String::as_str),
            Some("1")
        );
        assert!(!captured.vars.contains_key("CUED_TEST_API_TOKEN"));
        assert!(
            captured
                .stripped
                .contains(&"CUED_TEST_API_TOKEN".to_string())
        );
        // --keep-env: retained AND not recorded as stripped.
        assert_eq!(
            captured
                .vars
                .get("CUED_TEST_KEPT_TOKEN")
                .map(String::as_str),
            Some("hunter3")
        );
        assert!(
            !captured
                .stripped
                .contains(&"CUED_TEST_KEPT_TOKEN".to_string())
        );
    }

    #[test]
    fn validate_catches_the_cheap_lies() {
        let good = single_shell_graph(vec!["/bin/true".into()]);
        assert!(validate(&spec_for(good.clone(), Policies::default())).is_ok());

        let mut bad_entry = good.clone();
        bad_entry.entry = "nope".into();
        assert!(
            validate(&spec_for(bad_entry, Policies::default()))
                .unwrap_err()
                .to_string()
                .contains("entry")
        );

        let empty_argv = single_shell_graph(vec![]);
        assert!(
            validate(&spec_for(empty_argv, Policies::default()))
                .unwrap_err()
                .to_string()
                .contains("argv")
        );

        let mut bad_goto = good.clone();
        bad_goto
            .steps
            .get_mut("run")
            .unwrap()
            .transitions
            .push(Transition {
                when: Condition::Always,
                then: Effect::Goto {
                    step: "ghost".into(),
                    after: None,
                },
            });
        assert!(
            validate(&spec_for(bad_goto, Policies::default()))
                .unwrap_err()
                .to_string()
                .contains("ghost")
        );
    }

    #[test]
    fn notify_steps_reject_exit_conditions() {
        let mut graph = single_shell_graph(vec!["/bin/true".into()]);
        let step = graph.steps.get_mut("run").unwrap();
        step.action = Action::Notify {
            title: NotifySpec {
                title: "t".into(),
                body: "b".into(),
            }
            .title,
            body: "b".into(),
        };
        step.transitions.push(Transition {
            when: Condition::ExitEq(0),
            then: Effect::End {
                outcome: crate::model::Outcome::Success,
            },
        });
        assert!(
            validate(&spec_for(graph, Policies::default()))
                .unwrap_err()
                .to_string()
                .contains("notify")
        );
    }
}

// ---------------------------------------------------------------------------
// §6.2 file tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod file_tests {
    use super::*;
    use crate::export::job_to_toml;
    use crate::model::{Job, JobId, JobStatus};

    fn now() -> Zoned {
        "2026-07-16T09:00:00-06:00[America/Denver]"
            .parse()
            .expect("now")
    }

    fn parse(text: &str) -> Result<JobSpec> {
        let reference = now();
        from_toml(
            text,
            SubmitContext {
                now: &reference,
                at: None,
                every: None,
                zone: None,
                env: CapturedEnv::default(),
                policies: Policies::default(),
                cwd: "/tmp",
            },
        )
    }

    /// Only so the spec can be handed back to the exporter; the identity and
    /// lifecycle fields are the daemon's to assign and play no part here.
    fn as_job(spec: JobSpec) -> Job {
        Job {
            id: JobId(1),
            name: spec.name,
            schedule: spec.schedule,
            status: JobStatus::Active,
            approval: None,
            source: crate::model::JobSource::Cli,
            expired_at: None,
            expiry_reason: None,
            graph: spec.graph,
            cwd: spec.cwd,
            env: spec.env,
            policies: spec.policies,
            hooks: spec.hooks,
            created_at: now().timestamp(),
        }
    }

    /// The §6.2 example, verbatim from the design document. If the format
    /// drifts from what the doc shows, this is what says so.
    const DESIGN_EXAMPLE: &str = r#"
name  = "nightly-deploy"
entry = "build"
at    = "02:00 tomorrow"

[defaults]
cwd      = "/srv/app"
deadline = "2h"

[on_hold]
title = "deploy paused"
body  = "nightly deploy needs a look"

[[step]]
id  = "build"
run = "./build.sh"
timeout = "10m"
on.success = { goto = "window" }
on.fail    = { end  = "failure" }

[[step]]
id     = "window"
notify = { title = "build ok", body = "deploying after quiet window" }
on.always = { goto = "deploy", after = "1h" }

[[step]]
id  = "deploy"
run = ["./deploy.sh", "--target", "prod"]
on.success = { goto = "verify" }
on.fail    = { goto = "rollback" }

[[step]]
id  = "verify"
run = "curl -fsS https://app/health"
max_visits = 10
[[step.transition]]
when = { exit = 0 }
then = { end = "success" }
[[step.transition]]
when = { stdout_contains = "starting" }
then = { goto = "verify", after = { start = "30s", factor = 2, max = "10m" } }
[[step.transition]]
when = "always"
then = { goto = "rollback" }

[[step]]
id  = "rollback"
run = "./rollback.sh"
"#;

    /// Shared with the §6.3 loop-bound tests, which care that the
    /// documented poll loop is the shape that rule exists for.
    pub(super) fn design_example() -> JobSpec {
        parse(DESIGN_EXAMPLE).expect("the documented example must parse")
    }

    #[test]
    fn the_design_section_6_2_example_parses() {
        let spec = parse(DESIGN_EXAMPLE).expect("the documented example must parse");
        assert_eq!(spec.name.as_deref(), Some("nightly-deploy"));
        assert_eq!(spec.graph.entry, "build");
        assert_eq!(spec.graph.steps.len(), 5);
        assert_eq!(spec.cwd, "/srv/app");
        assert!(spec.policies.deadline.is_some());
        assert!(spec.hooks.on_hold.is_some());
        assert!(matches!(spec.schedule, Schedule::Once { .. }));
        // The graph the daemon will run has to pass the same §6.3 gate any
        // other front-end's does.
        validate(&spec).expect("the documented example must validate");
    }

    /// §6's promise: "dump a running job, edit it, resubmit — it
    /// round-trips." Export is the writing half and this is the reader, so
    /// the property is testable at last: parse → export → parse → export
    /// must reach a fixed point.
    #[test]
    fn a_workflow_survives_export_and_resubmission() {
        let first = parse(DESIGN_EXAMPLE).expect("parse");
        let exported = job_to_toml(&as_job(first), now().time_zone());

        let second = parse(&exported).expect("the export must be submittable");
        let re_exported = job_to_toml(&as_job(second), now().time_zone());

        assert_eq!(
            exported, re_exported,
            "export/parse is not a fixed point:\n--- first ---\n{exported}\n--- second ---\n{re_exported}"
        );
    }

    /// §6.2's stated precedence: timeout, then fail / success, then always.
    /// It matters because §3.2 is first-match-wins — an `always` ahead of a
    /// `fail` would swallow it.
    #[test]
    fn sugar_keys_desugar_in_the_documented_precedence() {
        let spec = parse(
            r#"
at = "9am"
[[step]]
id = "a"
run = "/bin/true"
on.always  = { end = "failure" }
on.success = { end = "success" }
on.fail    = { goto = "a" }
on.timeout = { end = "failure" }
"#,
        )
        .expect("parse");
        let order: Vec<String> = spec.graph.steps["a"]
            .transitions
            .iter()
            .map(|transition| format!("{:?}", transition.when))
            .collect();
        assert_eq!(
            order,
            ["TimedOut", "Failed", "Succeeded", "Always"],
            "{order:?}"
        );
    }

    /// Both tiers are ordered and first-match-wins, so interleaving them
    /// would silently shadow edges. Refusing is the honest answer.
    #[test]
    fn mixing_the_two_transition_tiers_is_refused() {
        let error = parse(
            r#"
at = "9am"
[[step]]
id = "a"
run = "/bin/true"
on.success = { end = "success" }
[[step.transition]]
when = "always"
then = { end = "failure" }
"#,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("one tier or the other"), "{error}");
    }

    /// A misspelled key must not be silently ignored — a scheduler that
    /// quietly drops `on_faliure` runs a job that isn't the one you wrote.
    #[test]
    fn unknown_keys_are_rejected_rather_than_ignored() {
        for text in [
            "at = \"9am\"\non_faliure = { title = \"x\" }\n[[step]]\nid = \"a\"\nrun = \"/bin/true\"\n",
            "at = \"9am\"\n[[step]]\nid = \"a\"\nrun = \"/bin/true\"\non.sucess = { end = \"success\" }\n",
            "at = \"9am\"\n[defaults]\nkill_grace = \"5s\"\n[[step]]\nid = \"a\"\nrun = \"/bin/true\"\n",
        ] {
            // `{:#}` walks the anyhow chain — the serde detail is the
            // cause, under our own "parsing the workflow file" context.
            let error = format!("{:#}", parse(text).unwrap_err());
            assert!(
                error.contains("unknown field"),
                "expected a rejection, got: {error}"
            );
        }
    }

    /// §4.1's three spellings, plus §6.3's rule that until/count are
    /// recurrence-only.
    #[test]
    fn the_schedule_keys_cover_the_four_one_shapes() {
        let once = parse("at = \"9am\"\n[[step]]\nid=\"a\"\nrun=\"/bin/true\"\n").expect("once");
        assert!(matches!(once.schedule, Schedule::Once { .. }));

        let every =
            parse("every = \"6h\"\n[[step]]\nid=\"a\"\nrun=\"/bin/true\"\n").expect("every");
        assert!(matches!(every.schedule, Schedule::Every { .. }));

        let anchored = parse(
            "at = \"9am\"\nevery = \"6h\"\ncount = 30\n[[step]]\nid=\"a\"\nrun=\"/bin/true\"\n",
        )
        .expect("anchored");
        match anchored.schedule {
            Schedule::Every { anchor, count, .. } => {
                assert_eq!(
                    anchor.to_zoned(now().time_zone().clone()).hour(),
                    9,
                    "the `at` anchors the cadence, read back in the zone it was given in"
                );
                assert_eq!(count, Some(30));
            }
            other => panic!("expected an anchored Every, got {other:?}"),
        }

        let calendar = parse(
            "every = \"mon,wed,fri 17:30\"\nzone = \"UTC\"\n[[step]]\nid=\"a\"\nrun=\"/bin/true\"\n",
        )
        .expect("calendar");
        match calendar.schedule {
            // An explicit zone survives, so a rule exported on one machine
            // means the same wall clock when resubmitted on another (§9).
            Schedule::Calendar { zone, .. } => assert_eq!(zone, "UTC"),
            other => panic!("expected a Calendar, got {other:?}"),
        }

        let error = parse("at = \"9am\"\ncount = 3\n[[step]]\nid=\"a\"\nrun=\"/bin/true\"\n")
            .unwrap_err()
            .to_string();
        assert!(error.contains("recurring"), "{error}");
    }

    /// codex #8: §10.1's precedence is built-in < config file < job < step,
    /// and the TOML path skipped the config layer — `cued submit` silently
    /// ignored a `[policy]` block that every other front-end honoured.
    #[test]
    fn a_file_overrides_configured_defaults_but_inherits_what_it_omits() {
        // Stand in for a config whose every policy is the non-default one.
        let configured = Policies {
            missed_wait: MissedWait::Abandon,
            on_interrupt: OnInterrupt::Fail,
            catch_up: CatchUp::Skip,
            overlap: Overlap::Queue,
            deadline: Some(SignedDuration::from_hours(3)),
        };
        let reference = now();
        let parse_with = |text: &str| {
            from_toml(
                text,
                SubmitContext {
                    now: &reference,
                    at: None,
                    every: None,
                    zone: None,
                    env: CapturedEnv::default(),
                    policies: configured.clone(),
                    cwd: "/tmp",
                },
            )
        };

        // A file that says nothing about policy inherits all of it.
        let quiet =
            parse_with("at = \"9am\"\n[[step]]\nid=\"a\"\nrun=\"/bin/true\"\n").expect("parse");
        assert_eq!(quiet.policies.missed_wait, MissedWait::Abandon);
        assert_eq!(quiet.policies.on_interrupt, OnInterrupt::Fail);
        assert_eq!(quiet.policies.catch_up, CatchUp::Skip);
        assert_eq!(quiet.policies.overlap, Overlap::Queue);
        assert_eq!(quiet.policies.deadline, Some(SignedDuration::from_hours(3)));

        // And a file that speaks wins over the config, key by key — the
        // ones it names change, the ones it doesn't still inherit.
        let loud = parse_with(
            "every = \"1h\"\noverlap = \"skip\"\n\
             [defaults]\nmissed_wait = \"run_asap\"\ndeadline = \"30m\"\n\
             [[step]]\nid=\"a\"\nrun=\"/bin/true\"\n",
        )
        .expect("parse");
        assert_eq!(loud.policies.missed_wait, MissedWait::RunAsap, "file wins");
        assert_eq!(loud.policies.overlap, Overlap::Skip, "file wins");
        assert_eq!(
            loud.policies.deadline,
            Some(SignedDuration::from_secs(1800))
        );
        assert_eq!(
            loud.policies.on_interrupt,
            OnInterrupt::Fail,
            "still inherited"
        );
        assert_eq!(loud.policies.catch_up, CatchUp::Skip, "still inherited");
    }

    /// §2.1: capture is what stops a job working interactively and failing at
    /// 3am, so a file's `env` adds to it rather than replacing it — and an
    /// explicit pin beats the §7.5 denylist, since writing it down *is* the
    /// visible decision the denylist asks for.
    #[test]
    fn file_env_overlays_the_capture_rather_than_replacing_it() {
        let reference = now();
        let mut captured = CapturedEnv::default();
        captured.vars.insert("PATH".into(), "/usr/bin".into());
        captured.stripped.push("DEPLOY_TOKEN".into());

        let spec = from_toml(
            "at = \"9am\"\n[defaults]\nenv = { DEPLOY_TOKEN = \"pinned\", EXTRA = \"1\" }\n[[step]]\nid=\"a\"\nrun=\"/bin/true\"\n",
            SubmitContext {
                now: &reference,
                at: None,
                every: None,
                zone: None,
                env: captured,
                policies: Policies::default(),
                cwd: "/tmp",
            },
        )
        .expect("parse");

        assert_eq!(
            spec.env.vars.get("PATH").map(String::as_str),
            Some("/usr/bin")
        );
        assert_eq!(spec.env.vars.get("EXTRA").map(String::as_str), Some("1"));
        assert_eq!(
            spec.env.vars.get("DEPLOY_TOKEN").map(String::as_str),
            Some("pinned")
        );
        assert!(
            !spec.env.stripped.contains(&"DEPLOY_TOKEN".to_string()),
            "a pinned var is no longer stripped"
        );
    }

    /// §2.2's two shell forms have to stay distinct through the file, or a
    /// resubmitted argv job would get re-shelled.
    #[test]
    fn the_shell_forms_stay_distinct_through_the_file() {
        let spec = parse(
            r#"
at = "9am"
entry = "shell"
[[step]]
id = "shell"
run = "make build && make deploy"
[[step]]
id = "argv"
run = ["./deploy.sh", "--target", "prod"]
"#,
        )
        .expect("parse");
        match &spec.graph.steps["shell"].action {
            Action::Shell { argv } => assert_eq!(argv[..2], ["/bin/sh", "-c"]),
            other => panic!("expected Shell, got {other:?}"),
        }
        match &spec.graph.steps["argv"].action {
            Action::Shell { argv } => assert_eq!(argv[0], "./deploy.sh"),
            other => panic!("expected Shell, got {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// §6.1 chain tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod chain_tests {
    use super::*;

    fn args(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|arg| arg.to_string()).collect()
    }

    fn goto_of(graph: &Graph, step: &str) -> Option<(String, Option<SignedDuration>)> {
        match graph.steps[step].transitions.first().map(|t| &t.then) {
            Some(Effect::Goto { step, after }) => Some((
                step.clone(),
                match after {
                    Some(Wait::In(duration)) => Some(*duration),
                    _ => None,
                },
            )),
            _ => None,
        }
    }

    /// The reason this walks argv at all: clap collects `--then` and
    /// `--then-after` into two vectors, so these two command lines are
    /// indistinguishable to it — and they are different pipelines.
    #[test]
    fn the_order_the_links_were_typed_in_is_recovered() {
        let after_first = chain_links(&args(&[
            "cued",
            "chain",
            "A",
            "--then-after",
            "1h",
            "B",
            "--then",
            "C",
        ]))
        .expect("links");
        assert_eq!(
            after_first,
            [
                Link::ThenAfter("1h".into(), "B".into()),
                Link::Then("C".into()),
            ]
        );

        let after_last = chain_links(&args(&[
            "cued",
            "chain",
            "A",
            "--then",
            "B",
            "--then-after",
            "1h",
            "C",
        ]))
        .expect("links");
        assert_eq!(
            after_last,
            [
                Link::Then("B".into()),
                Link::ThenAfter("1h".into(), "C".into()),
            ]
        );
        assert_ne!(after_first, after_last, "the two orders must not collapse");
    }

    #[test]
    fn the_equals_form_and_value_flags_are_handled() {
        let links = chain_links(&args(&[
            "cued",
            "chain",
            "A",
            "--then=B",
            "--then-after=30s",
            "C",
        ]))
        .expect("links");
        assert_eq!(
            links,
            [
                Link::Then("B".into()),
                Link::ThenAfter("30s".into(), "C".into()),
            ]
        );

        // A value-taking flag's value must not be read as a link, even when
        // it looks exactly like one.
        let tricky = chain_links(&args(&[
            "cued", "chain", "A", "--name", "--then", "--then", "B",
        ]))
        .expect("links");
        assert_eq!(tricky, [Link::Then("B".into())], "{tricky:?}");
    }

    #[test]
    fn a_dangling_flag_is_an_error_not_a_silent_drop() {
        assert!(chain_links(&args(&["cued", "chain", "A", "--then"])).is_err());
        assert!(chain_links(&args(&["cued", "chain", "A", "--then-after", "1h"])).is_err());
    }

    /// §6.1: `--then` is "on success, go to the next link"; `--then-after`
    /// puts a durable sleep-edge *before* that link, not after it.
    #[test]
    fn the_chain_is_a_straight_line_with_the_wait_on_the_right_edge() {
        let graph = chain_graph(
            "./build.sh",
            &[
                Link::Then("./test.sh".into()),
                Link::ThenAfter("1h".into(), "./deploy.sh".into()),
            ],
            ChainFailure::Stop,
        )
        .expect("graph");

        assert_eq!(graph.entry, "step1");
        assert_eq!(graph.steps.len(), 3);
        assert_eq!(goto_of(&graph, "step1"), Some(("step2".into(), None)));
        // The hour belongs to the edge *into* deploy.
        assert_eq!(
            goto_of(&graph, "step2"),
            Some(("step3".into(), Some(SignedDuration::from_hours(1))))
        );
        assert!(
            graph.steps["step3"].transitions.is_empty(),
            "the last link ends the run by §3.2's derived End"
        );
        validate(&spec_for(graph.clone(), Policies::default()))
            .expect("a chain must pass the §6.3 gate");
    }

    /// §3.2 fail-fast does the work for `stop`: nothing handles the failure,
    /// so the run ends there. `continue` needs the edge to be unconditional.
    #[test]
    fn the_failure_policy_is_one_edge_condition() {
        let stop = chain_graph("A", &[Link::Then("B".into())], ChainFailure::Stop).expect("graph");
        assert!(matches!(
            stop.steps["step1"].transitions[0].when,
            Condition::Succeeded
        ));

        let keep_going =
            chain_graph("A", &[Link::Then("B".into())], ChainFailure::Continue).expect("graph");
        assert!(matches!(
            keep_going.steps["step1"].transitions[0].when,
            Condition::Always
        ));
    }

    /// §2.2: a chain link is one quoted string, so it's the shell form.
    #[test]
    fn links_are_the_shell_form() {
        let graph = chain_graph("make build && make test", &[], ChainFailure::Stop).expect("graph");
        match &graph.steps["step1"].action {
            Action::Shell { argv } => {
                assert_eq!(argv, &["/bin/sh", "-c", "make build && make test"]);
            }
            other => panic!("expected Shell, got {other:?}"),
        }
    }

    #[test]
    fn a_bad_wait_is_refused_at_submit() {
        let error = chain_graph(
            "A",
            &[Link::ThenAfter("later".into(), "B".into())],
            ChainFailure::Stop,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("--then-after"), "{error}");
    }
}

// ---------------------------------------------------------------------------
// §6.3 loop-bound tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod loop_bound_tests {
    use super::*;
    use crate::model::Transition;

    fn step_with(goto: &[&str]) -> Step {
        Step {
            action: Action::Shell {
                argv: vec!["/bin/true".into()],
            },
            cwd: None,
            env: None,
            timeout: None,
            kill_grace: None,
            transitions: goto
                .iter()
                .map(|target| Transition {
                    when: Condition::Always,
                    then: Effect::Goto {
                        step: (*target).into(),
                        after: None,
                    },
                })
                .collect(),
            max_visits: None,
            restart_safe: false,
            missed_wait: None,
        }
    }

    fn graph_of(entry: &str, steps: &[(&str, &[&str])]) -> Graph {
        Graph {
            entry: entry.into(),
            steps: steps
                .iter()
                .map(|(id, goto)| ((*id).to_string(), step_with(goto)))
                .collect(),
        }
    }

    /// §6.3: "Every back-edge sits under a bound (`max_visits` and/or job
    /// `deadline`) so a loop can't run forever."
    #[test]
    fn an_unbounded_loop_is_refused_and_either_bound_frees_it() {
        let looping = graph_of("poll", &[("poll", &["poll"][..])]);

        let error = validate(&spec_for(looping.clone(), Policies::default()))
            .unwrap_err()
            .to_string();
        assert!(error.contains("loops back"), "{error}");
        assert!(
            error.contains("max_visits") && error.contains("deadline"),
            "{error}"
        );

        // §3.2 caps how often the step may be re-entered…
        let mut capped = looping.clone();
        capped.steps.get_mut("poll").expect("poll").max_visits = Some(10);
        validate(&spec_for(capped, Policies::default())).expect("max_visits bounds the loop");

        // …and a deadline caps the whole run in wall-clock instead, which is
        // a real bound now that §3.2's kill is implemented.
        let timed = Policies {
            deadline: Some(SignedDuration::from_hours(1)),
            ..Policies::default()
        };
        validate(&spec_for(looping, timed)).expect("a deadline bounds every loop in the run");
    }

    /// The distinction the three-colour walk exists for: a step reached
    /// twice by different paths is not a loop. A naive "have I seen this
    /// node" check would reject the commonest shape there is — two branches
    /// rejoining.
    #[test]
    fn a_rejoining_branch_is_not_a_back_edge() {
        let diamond = graph_of(
            "a",
            &[("a", &["b", "c"][..]), ("b", &["c"][..]), ("c", &[][..])],
        );
        assert!(
            back_edges(&diamond).is_empty(),
            "{:?}",
            back_edges(&diamond)
        );
        validate(&spec_for(diamond, Policies::default())).expect("a diamond has no loop to bound");
    }

    #[test]
    fn a_longer_cycle_is_found_and_named() {
        let ring = graph_of(
            "a",
            &[("a", &["b"][..]), ("b", &["c"][..]), ("c", &["a"][..])],
        );
        let edges = back_edges(&ring);
        assert_eq!(edges, [("c".to_string(), "a".to_string())], "{edges:?}");
    }

    /// A step unreachable from `entry` is not unreachable: `cued retry
    /// --from <step>` starts a run anywhere in the graph (§3.4), so a loop
    /// hiding there still has to be bounded.
    #[test]
    fn a_cycle_unreachable_from_entry_is_still_checked() {
        let stranded = graph_of("a", &[("a", &[][..]), ("x", &["y"][..]), ("y", &["x"][..])]);
        assert!(
            validate(&spec_for(stranded, Policies::default())).is_err(),
            "a loop only reachable via `retry --from` is still a loop"
        );
    }

    /// The §6.2 example's `verify` step polls itself — the exact shape this
    /// rule exists for — and the document bounds it both ways.
    #[test]
    fn the_documented_poll_loop_is_bounded() {
        let example = super::file_tests::design_example();
        let back = back_edges(&example.graph);
        assert_eq!(
            back,
            [("verify".to_string(), "verify".to_string())],
            "the documented poll loop should be the only back-edge: {back:?}"
        );
        validate(&example).expect("the example must validate");
    }
}

// ---------------------------------------------------------------------------
// §6.3 schedule tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod schedule_gate_tests {
    use super::*;
    use crate::model::Weekday;
    use jiff::civil::time;

    fn spec_with(schedule: Schedule) -> JobSpec {
        let mut spec = spec_for(
            single_shell_graph(vec!["/bin/true".into()]),
            Policies::default(),
        );
        spec.schedule = schedule;
        spec
    }

    fn every(interval_secs: i64) -> Schedule {
        Schedule::Every {
            interval: SignedDuration::from_secs(interval_secs),
            anchor: "2026-07-16T09:00:00-06:00[America/Denver]"
                .parse()
                .expect("anchor"),
            until: None,
            count: None,
        }
    }

    fn calendar(spec: CalendarSpec, zone: &str) -> Schedule {
        Schedule::Calendar {
            spec,
            zone: zone.into(),
            until: None,
            count: None,
        }
    }

    /// The gate has to hold against the *wire*, not just the CLI. `timeparse`
    /// constrains what a user can type; a client speaking §5.1 directly is
    /// under no such limit, and the daemon is the trust boundary.
    #[test]
    fn a_non_positive_interval_is_refused_at_submit() {
        // Divides by zero in §4.2's catch-up arithmetic if it gets through.
        for seconds in [0, -60] {
            let error = validate(&spec_with(every(seconds)))
                .unwrap_err()
                .to_string();
            assert!(error.contains("must be positive"), "{error}");
        }
        validate(&spec_with(every(60))).expect("a positive interval is fine");
    }

    #[test]
    fn an_unknown_time_zone_is_refused() {
        let error = validate(&spec_with(calendar(
            CalendarSpec::Daily {
                at: time(9, 0, 0, 0),
            },
            "Nowhere/Nothing",
        )))
        .unwrap_err()
        .to_string();
        assert!(error.contains("time zone"), "{error}");

        validate(&spec_with(calendar(
            CalendarSpec::Daily {
                at: time(9, 0, 0, 0),
            },
            "America/Denver",
        )))
        .expect("a real zone is fine");
    }

    /// An empty day set matches no date, so §9's walk searches a thousand
    /// days and gives up — a slow, confusing failure a long way from the
    /// mistake.
    #[test]
    fn a_calendar_rule_that_can_never_match_is_refused() {
        let empty_week = calendar(
            CalendarSpec::Weekly {
                days: Vec::new(),
                at: time(9, 0, 0, 0),
            },
            "UTC",
        );
        assert!(validate(&spec_with(empty_week)).is_err());

        let empty_month = calendar(
            CalendarSpec::Monthly {
                days: Vec::new(),
                at: time(9, 0, 0, 0),
            },
            "UTC",
        );
        assert!(validate(&spec_with(empty_month)).is_err());

        // §9.2 clamps 29-31 onto short months, but 0 and 32 are not days of
        // any month and would simply never fire.
        for day in [0u8, 32, 200] {
            let impossible = calendar(
                CalendarSpec::Monthly {
                    days: vec![MonthDay::Day(day)],
                    at: time(9, 0, 0, 0),
                },
                "UTC",
            );
            let error = validate(&spec_with(impossible)).unwrap_err().to_string();
            assert!(error.contains("doesn't exist"), "day {day}: {error}");
        }

        let clamped = calendar(
            CalendarSpec::Monthly {
                days: vec![MonthDay::Day(31), MonthDay::Last],
                at: time(9, 0, 0, 0),
            },
            "UTC",
        );
        validate(&spec_with(clamped)).expect("31 and last are both real (§9.2)");

        let weekdays = calendar(
            CalendarSpec::Weekly {
                days: vec![Weekday::Mon, Weekday::Fri],
                at: time(17, 30, 0, 0),
            },
            "UTC",
        );
        validate(&spec_with(weekdays)).expect("a real weekday set is fine");
    }

    /// A zero `count` is the one limit that fails *silently*: the job is
    /// accepted, finishes without running anything, and reads as cued
    /// having lost it.
    #[test]
    fn a_zero_firing_count_is_refused() {
        let mut schedule = every(60);
        if let Schedule::Every { count, .. } = &mut schedule {
            *count = Some(0);
        }
        let error = validate(&spec_with(schedule)).unwrap_err().to_string();
        assert!(error.contains("count"), "{error}");
    }

    /// §6.3's "until/count only on recurring" needs no check — `Once` has
    /// no such fields, so the model makes it unrepresentable. This is here
    /// to fail loudly if that ever stops being true.
    #[test]
    fn a_one_off_cannot_carry_recurrence_limits() {
        let once = Schedule::Once {
            at: "2026-07-16T09:00:00-06:00[America/Denver]"
                .parse()
                .expect("at"),
        };
        assert_eq!(once.count(), None, "Once must have no firing cap to carry");
        validate(&spec_with(once)).expect("a plain one-off is valid");
    }

    /// The gate is one call: a front-end can't pass the graph half and skip
    /// the schedule half, because there is only one door.
    #[test]
    fn both_halves_of_the_gate_run() {
        let mut spec = spec_with(every(0));
        spec.graph.entry = "nowhere".into();
        // Whichever fires first, submitting must not succeed.
        assert!(validate(&spec).is_err());
    }
}

#[cfg(test)]
mod zone_tests {
    use super::*;
    use crate::model::Wait;

    /// codex #4: the file's `zone` was resolved *after* the steps were built,
    /// so a workflow declaring `zone = "America/New_York"` parsed its
    /// transition `until` wall times against the submitting machine's zone
    /// instead. The schedule honoured the key and the graph did not.
    #[test]
    fn the_files_zone_reaches_transition_until_times() {
        let denver: Zoned = "2026-07-16T09:00:00-06:00[America/Denver]"
            .parse()
            .expect("now");
        let parse = |text: &str| {
            from_toml(
                text,
                SubmitContext {
                    now: &denver,
                    at: None,
                    every: None,
                    zone: None,
                    env: CapturedEnv::default(),
                    policies: Policies::default(),
                    cwd: "/tmp",
                },
            )
        };

        let file = r#"
at    = "2026-08-01 09:00:00"
zone  = "America/New_York"
entry = "a"
[[step]]
id = "a"
run = "/bin/true"
[[step.transition]]
when = "always"
then = { goto = "b", after = { until = "2026-08-01 17:00:00" } }
[[step]]
id = "b"
run = "/bin/true"
"#;
        let spec = parse(file).expect("parse");

        let eastern = jiff::tz::TimeZone::get("America/New_York").expect("tzdb");
        // 09:00 in New York, not in Denver.
        match spec.schedule {
            Schedule::Once { at } => assert_eq!(at.to_zoned(eastern.clone()).hour(), 9),
            other => panic!("expected Once, got {other:?}"),
        }
        // …and the transition's wall time is read in the same zone, which is
        // the half that used to fall through to the machine's.
        let wait = match &spec.graph.steps["a"].transitions[0].then {
            Effect::Goto {
                after: Some(wait), ..
            } => wait,
            other => panic!("expected a goto with a wait, got {other:?}"),
        };
        match wait {
            Wait::Until(at) => assert_eq!(
                at.to_zoned(eastern).hour(),
                17,
                "the transition's `until` was read in the wrong zone"
            ),
            other => panic!("expected Until, got {other:?}"),
        }
    }
}
