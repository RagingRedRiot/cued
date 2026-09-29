//! Rendering a stored job back out as a §6.2 workflow file.
//!
//! §6 banks a consequence: "the file format is also the export/inspection
//! format… dump a running job, edit it, resubmit — it round-trips." So this
//! emits the *authoring* shape — `[[step]]` with `run` / `notify`, schedule
//! keys, `[defaults]` — not `serde`'s view of the model structs. The serde
//! form is what the store holds (§5.3) and what `--json` prints; nobody
//! designed it to be read or edited, and `action.shell.argv = [...]` is not
//! a thing to hand someone and say "edit this".
//!
//! Transitions are emitted in the explicit `[[step.transition]]` tier rather
//! than the `on.success` sugar. Both are legal input (§6.2), but sugar is a
//! lossy projection — deciding a set of edges is "exactly an on.fail" means
//! pattern-matching that can be subtly wrong, and the explicit tier is
//! fully expressive with no such judgement. Sugar is for humans writing;
//! export is a machine writing.
//!
//! The captured environment is deliberately absent — see `job_to_toml`.

use jiff::{SignedDuration, Timestamp};

use crate::model::{
    Action, CalendarSpec, CatchUp, Condition, Effect, Job, MissedWait, MonthDay, NotifySpec,
    OnInterrupt, Outcome, OutputMatch, Overlap, Schedule, Step, Transition, Wait, Weekday,
};

/// The §6.2 file, as text.
///
/// **The captured env (§2.1) is not included.** It isn't part of the
/// definition — it's context captured at submit time, and a workflow file
/// that pinned one machine's several-hundred-variable environment would be
/// both unreadable and wrong to resubmit, since resubmitting should capture
/// afresh. `cued show` reports it (and §7.5's stripped names) in the human
/// view instead, and `--json` carries it in full.
pub fn job_to_toml(job: &Job, reader: &jiff::tz::TimeZone) -> String {
    // A file has exactly one `zone` key, and every wall clock in it is read
    // against that one zone on import — so every wall clock written must be
    // rendered in it too. For a `Calendar` job that zone is forced: the rule
    // means what it means, so the rule's zone wins and `until` and any
    // `Wait::Until` are written in it as well. Rendering those in the
    // reader's zone while declaring the rule's would shift them on import by
    // exactly the difference between the two.
    let zone = match &job.schedule {
        Schedule::Calendar { zone, .. } => {
            jiff::tz::TimeZone::get(zone).unwrap_or_else(|_| reader.clone())
        }
        _ => reader.clone(),
    };
    let zone = &zone;
    let mut out = String::new();
    out.push_str("# cued job ");
    out.push_str(&job.id.to_string());
    out.push_str(" (cued show --toml). Policy keys at their defaults are omitted.\n");
    out.push_str(&format!(
        "# status: {}; source: {:?}\n",
        job.display_status(),
        job.source
    ));
    if let Some(at) = job.expired_at {
        out.push_str(&format!(
            "# expired: {at}; reason: {:?}\n",
            job.expiry_reason
        ));
    }
    out.push('\n');

    if let Some(name) = &job.name {
        out.push_str(&format!("name  = {}\n", quote(name)));
    }
    out.push_str(&format!("entry = {}\n", quote(&job.graph.entry)));
    out.push_str(&schedule_keys(&job.schedule, zone));
    // §6.2 groups these with the scheduling keys, not [defaults]: they
    // answer "what happens to a *firing*", which is not something a step
    // inherits.
    if job.policies.catch_up != CatchUp::default() {
        out.push_str(&format!(
            "catch_up = {}\n",
            quote(match job.policies.catch_up {
                CatchUp::RunOnce => "run_once",
                CatchUp::Skip => "skip",
            })
        ));
    }
    if job.policies.overlap != Overlap::default() {
        out.push_str(&format!(
            "overlap = {}\n",
            quote(match job.policies.overlap {
                Overlap::Skip => "skip",
                Overlap::Queue => "queue",
            })
        ));
    }

    out.push_str(&defaults_table(job));
    for (key, hook) in [
        ("on_hold", &job.hooks.on_hold),
        ("on_failure", &job.hooks.on_failure),
        ("on_success", &job.hooks.on_success),
        ("on_missed", &job.hooks.on_missed),
    ] {
        if let Some(spec) = hook {
            out.push_str(&hook_table(key, spec));
        }
    }

    // Entry step first — a reader starts where a run starts — then the rest
    // in the map's own order.
    let entry = &job.graph.entry;
    if let Some(step) = job.graph.steps.get(entry) {
        out.push_str(&step_table(entry, step, zone));
    }
    for (id, step) in &job.graph.steps {
        if id != entry {
            out.push_str(&step_table(id, step, zone));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Schedule (§4.1, §9)
// ---------------------------------------------------------------------------

fn schedule_keys(schedule: &Schedule, zone: &jiff::tz::TimeZone) -> String {
    let mut out = String::new();
    match schedule {
        Schedule::Once { at } => {
            out.push_str(&format!("at    = {}\n", quote(&instant(at, zone))));
            out.push_str(&zone_key(zone));
        }
        Schedule::Every {
            interval,
            anchor,
            until,
            count,
        } => {
            // Always emit the anchor. "`every` alone anchors at submit time"
            // (§4.1) would re-anchor to *now* on resubmit, silently shifting
            // every future firing; `at` + `every` is the anchored form and
            // preserves the cadence exactly.
            out.push_str(&format!("at    = {}\n", quote(&instant(anchor, zone))));
            out.push_str(&format!("every = {}\n", quote(&duration(*interval))));
            out.push_str(&zone_key(zone));
            out.push_str(&limit_keys(until, count, zone));
        }
        // The rule names the zone it *means*, and `job_to_toml` has already
        // made that the file's zone, so every other instant here is written
        // in it too and the one `zone` key is true of all of them.
        Schedule::Calendar {
            spec, until, count, ..
        } => {
            out.push_str(&format!("every = {}\n", quote(&calendar(spec))));
            out.push_str(&zone_key(zone));
            out.push_str(&limit_keys(until, count, zone));
        }
    }
    out
}

fn limit_keys(until: &Option<Timestamp>, count: &Option<u32>, zone: &jiff::tz::TimeZone) -> String {
    let mut out = String::new();
    if let Some(until) = until {
        out.push_str(&format!("until = {}\n", quote(&instant(until, zone))));
    }
    if let Some(count) = count {
        out.push_str(&format!("count = {count}\n"));
    }
    out
}

/// §9.2's calendar grammar, back out. Weekday sugar (`weekdays`/`weekends`)
/// is not reconstructed — the explicit list means the same thing and can't
/// be misread.
///
/// Public because `cued show`'s human view wants the same phrasing: the
/// grammar reads well enough that rendering it twice, differently, would
/// only invite the two to drift.
pub fn describe_calendar(spec: &CalendarSpec) -> String {
    calendar(spec)
}

fn calendar(spec: &CalendarSpec) -> String {
    match spec {
        CalendarSpec::Daily { at } => format!("day {}", clock(at)),
        CalendarSpec::Weekly { days, at } => {
            let days: Vec<&str> = days.iter().map(weekday).collect();
            format!("{} {}", days.join(","), clock(at))
        }
        CalendarSpec::Monthly { days, at } => {
            let days: Vec<String> = days
                .iter()
                .map(|day| match day {
                    MonthDay::Day(n) => n.to_string(),
                    MonthDay::Last => "last".to_string(),
                })
                .collect();
            format!("month on {} at {}", days.join(","), clock(at))
        }
    }
}

/// The zone the file's wall-clock times are to be read in (§9). Always
/// emitted: an export that omitted it would resubmit to a different instant
/// in any other zone, which is the one thing the round trip must not do.
fn zone_key(zone: &jiff::tz::TimeZone) -> String {
    format!("zone  = {}\n", quote(zone.iana_name().unwrap_or("UTC")))
}

fn weekday(day: &Weekday) -> &'static str {
    match day {
        Weekday::Mon => "mon",
        Weekday::Tue => "tue",
        Weekday::Wed => "wed",
        Weekday::Thu => "thu",
        Weekday::Fri => "fri",
        Weekday::Sat => "sat",
        Weekday::Sun => "sun",
    }
}

fn clock(at: &jiff::civil::Time) -> String {
    if at.second() == 0 {
        format!("{:02}:{:02}", at.hour(), at.minute())
    } else {
        format!("{:02}:{:02}:{:02}", at.hour(), at.minute(), at.second())
    }
}

/// ISO, which is the only numeric date form §9.1 accepts — deliberately, so
/// nothing here can be read as 6/25 vs 25/6.
///
/// Rendered in `zone`, and the file carries a `zone` key naming it. That is
/// what makes the round trip zone-safe: a stored instant has no wall clock
/// of its own, so writing one without saying which zone it was written in
/// would mean a file that resubmits to a *different instant* wherever the
/// reader's zone differs.
fn instant(at: &Timestamp, zone: &jiff::tz::TimeZone) -> String {
    at.to_zoned(zone.clone())
        .strftime("%Y-%m-%d %H:%M:%S")
        .to_string()
}

/// §9.1's duration grammar: units `s m h d w`, compounds concatenate. No
/// month or year units exist — those are `Calendar`'s job — and sub-second
/// precision has no spelling, which is fine: the grammar can't produce it.
fn duration(value: SignedDuration) -> String {
    let mut secs = value.as_secs().max(0);
    if secs == 0 {
        return "0s".to_string();
    }
    let mut out = String::new();
    for (unit, size) in [
        ("w", 604_800),
        ("d", 86_400),
        ("h", 3_600),
        ("m", 60),
        ("s", 1),
    ] {
        let whole = secs / size;
        if whole > 0 {
            out.push_str(&format!("{whole}{unit}"));
            secs -= whole * size;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tables
// ---------------------------------------------------------------------------

fn defaults_table(job: &Job) -> String {
    let mut body = format!("cwd = {}\n", quote(&job.cwd));
    if let Some(deadline) = job.policies.deadline {
        body.push_str(&format!("deadline = {}\n", quote(&duration(deadline))));
    }
    if job.policies.on_interrupt != OnInterrupt::default() {
        body.push_str(&format!(
            "on_interrupt = {}\n",
            quote(match job.policies.on_interrupt {
                OnInterrupt::Hold => "hold",
                OnInterrupt::Fail => "fail",
                OnInterrupt::Retry => "retry",
            })
        ));
    }
    if job.policies.missed_wait != MissedWait::default() {
        body.push_str(&format!(
            "missed_wait = {}\n",
            quote(missed_wait(job.policies.missed_wait))
        ));
    }
    format!("\n[defaults]\n{body}")
}

fn missed_wait(policy: MissedWait) -> &'static str {
    match policy {
        MissedWait::RunAsap => "run_asap",
        MissedWait::Abandon => "abandon",
    }
}

fn hook_table(key: &str, spec: &NotifySpec) -> String {
    format!(
        "\n[{key}]\ntitle = {}\nbody  = {}\n",
        quote(&spec.title),
        quote(&spec.body)
    )
}

fn step_table(id: &str, step: &Step, zone: &jiff::tz::TimeZone) -> String {
    let mut out = format!("\n[[step]]\nid = {}\n", quote(id));
    match &step.action {
        // §2.2: the sh -c form reads back as the original string; anything
        // else is argv and stays an array.
        Action::Shell { argv } => match argv.as_slice() {
            [shell, flag, script] if shell == "/bin/sh" && flag == "-c" => {
                out.push_str(&format!("run = {}\n", quote(script)));
            }
            argv => {
                let items: Vec<String> = argv.iter().map(|arg| quote(arg)).collect();
                out.push_str(&format!("run = [{}]\n", items.join(", ")));
            }
        },
        Action::Notify { title, body } => {
            out.push_str(&format!(
                "notify = {{ title = {}, body = {} }}\n",
                quote(title),
                quote(body)
            ));
        }
    }
    if let Some(cwd) = &step.cwd {
        out.push_str(&format!("cwd = {}\n", quote(cwd)));
    }
    if let Some(env) = &step.env {
        let items: Vec<String> = env
            .iter()
            .map(|(name, value)| format!("{} = {}", quote(name), quote(value)))
            .collect();
        out.push_str(&format!("env = {{ {} }}\n", items.join(", ")));
    }
    if let Some(timeout) = step.timeout {
        out.push_str(&format!("timeout = {}\n", quote(&duration(timeout))));
    }
    if let Some(grace) = step.kill_grace {
        out.push_str(&format!("kill_grace = {}\n", quote(&duration(grace))));
    }
    if let Some(max) = step.max_visits {
        out.push_str(&format!("max_visits = {max}\n"));
    }
    if step.restart_safe {
        out.push_str("restart_safe = true\n");
    }
    if let Some(policy) = step.missed_wait {
        out.push_str(&format!("missed_wait = {}\n", quote(missed_wait(policy))));
    }
    for transition in &step.transitions {
        out.push_str(&transition_table(transition, zone));
    }
    out
}

fn transition_table(transition: &Transition, zone: &jiff::tz::TimeZone) -> String {
    format!(
        "[[step.transition]]\nwhen = {}\nthen = {}\n",
        condition(&transition.when),
        effect(&transition.then, zone)
    )
}

fn condition(when: &Condition) -> String {
    match when {
        Condition::Always => quote("always"),
        Condition::Succeeded => quote("succeeded"),
        Condition::Failed => quote("failed"),
        Condition::TimedOut => quote("timed_out"),
        Condition::ExitEq(code) => format!("{{ exit = {code} }}"),
        Condition::ExitNe(code) => format!("{{ exit_ne = {code} }}"),
        Condition::ExitIn(codes) => {
            let items: Vec<String> = codes.iter().map(i32::to_string).collect();
            format!("{{ exit_in = [{}] }}", items.join(", "))
        }
        Condition::Stdout(matcher) => output("stdout", matcher),
        Condition::Stderr(matcher) => output("stderr", matcher),
        Condition::All(inner) => {
            let items: Vec<String> = inner.iter().map(condition).collect();
            format!("{{ all = [{}] }}", items.join(", "))
        }
    }
}

fn output(stream: &str, matcher: &OutputMatch) -> String {
    match matcher {
        OutputMatch::Contains(needle) => {
            format!("{{ {stream}_contains = {} }}", quote(needle))
        }
        OutputMatch::Regex(pattern) => {
            format!("{{ {stream}_matches = {} }}", quote(pattern))
        }
    }
}

fn effect(then: &Effect, zone: &jiff::tz::TimeZone) -> String {
    match then {
        Effect::End { outcome } => format!(
            "{{ end = {} }}",
            quote(match outcome {
                Outcome::Success => "success",
                Outcome::Failure => "failure",
            })
        ),
        Effect::Goto { step, after: None } => format!("{{ goto = {} }}", quote(step)),
        Effect::Goto {
            step,
            after: Some(wait),
        } => {
            format!(
                "{{ goto = {}, after = {} }}",
                quote(step),
                after(wait, zone)
            )
        }
    }
}

fn after(wait: &Wait, zone: &jiff::tz::TimeZone) -> String {
    match wait {
        Wait::In(value) => quote(&duration(*value)),
        Wait::Until(at) => format!("{{ until = {} }}", quote(&instant(at, zone))),
        Wait::Backoff { start, factor, max } => format!(
            "{{ start = {}, factor = {factor}, max = {} }}",
            quote(&duration(*start)),
            quote(&duration(*max))
        ),
    }
}

/// A TOML basic string. Hand-rolled rather than borrowed from a serializer
/// because this file's *layout* is hand-built — but the escaping still has
/// to be exactly right, since step ids, commands and notification bodies are
/// all arbitrary user text.
fn quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            // Everything else below 0x20, plus DEL, needs the \uXXXX form.
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\u{:04X}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use jiff::civil::time;

    use super::*;
    use crate::model::{CapturedEnv, Graph, Hooks, JobId, JobStatus, OutputMatch, Policies, Step};

    fn zoned(text: &str) -> Timestamp {
        text.parse().expect(text)
    }

    /// The zone these fixtures are written in; export renders into it.
    fn denver() -> jiff::tz::TimeZone {
        jiff::tz::TimeZone::get("America/Denver").expect("tzdb")
    }

    fn step(action: Action) -> Step {
        Step {
            action,
            cwd: None,
            env: None,
            timeout: None,
            kill_grace: None,
            transitions: Vec::new(),
            max_visits: None,
            restart_safe: false,
            missed_wait: None,
        }
    }

    fn job(schedule: Schedule, steps: Vec<(&str, Step)>, entry: &str) -> Job {
        Job {
            id: JobId(7),
            name: Some("nightly".into()),
            schedule,
            status: JobStatus::Active,
            approval: None,
            source: crate::model::JobSource::Cli,
            expired_at: None,
            expiry_reason: None,
            graph: Graph {
                entry: entry.into(),
                steps: steps
                    .into_iter()
                    .map(|(id, step)| (id.to_string(), step))
                    .collect::<BTreeMap<_, _>>(),
            },
            cwd: "/srv/app".into(),
            env: CapturedEnv::default(),
            policies: Policies::default(),
            hooks: Hooks::default(),
            created_at: zoned("2026-07-16T09:00:00-06:00[America/Denver]"),
        }
    }

    /// §9.1: units s/m/h/d/w, compounds concatenate, and no month or year
    /// unit exists — a month isn't a fixed length, which is exactly why
    /// those cadences are `Calendar`'s job.
    #[test]
    fn durations_render_in_the_grammar_that_parses_them() {
        let render = |secs| duration(SignedDuration::from_secs(secs));
        assert_eq!(render(30), "30s");
        assert_eq!(render(90 * 60), "1h30m");
        assert_eq!(render(6 * 3600), "6h");
        assert_eq!(render(14 * 86_400), "2w");
        assert_eq!(render(604_800 + 86_400 + 3_600 + 60 + 1), "1w1d1h1m1s");
        assert_eq!(render(0), "0s");
    }

    #[test]
    fn calendar_rules_render_in_the_9_2_grammar() {
        assert_eq!(
            calendar(&CalendarSpec::Daily {
                at: time(9, 0, 0, 0)
            }),
            "day 09:00"
        );
        assert_eq!(
            calendar(&CalendarSpec::Weekly {
                days: vec![Weekday::Mon, Weekday::Wed, Weekday::Fri],
                at: time(17, 30, 0, 0),
            }),
            "mon,wed,fri 17:30"
        );
        assert_eq!(
            calendar(&CalendarSpec::Monthly {
                days: vec![MonthDay::Day(1), MonthDay::Last],
                at: time(23, 0, 15, 0),
            }),
            "month on 1,last at 23:00:15"
        );
    }

    /// §4.1: `every` alone re-anchors at submit time, so an export that
    /// dropped the anchor would silently shift every future firing on
    /// resubmit. The anchored form (`at` + `every`) is the faithful one.
    #[test]
    fn an_every_schedule_exports_its_anchor() {
        let schedule = Schedule::Every {
            interval: SignedDuration::from_secs(6 * 3600),
            anchor: zoned("2026-07-16T09:00:00-06:00[America/Denver]"),
            until: None,
            count: Some(30),
        };
        let rendered = job_to_toml(
            &job(
                schedule,
                vec![(
                    "run",
                    step(Action::Shell {
                        argv: vec!["/bin/true".into()],
                    }),
                )],
                "run",
            ),
            &denver(),
        );
        assert!(
            rendered.contains(r#"at    = "2026-07-16 09:00:00""#),
            "{rendered}"
        );
        assert!(rendered.contains(r#"every = "6h""#), "{rendered}");
        assert!(rendered.contains("count = 30"), "{rendered}");
    }

    /// §2.2: the sh -c form reads back as the original string; anything else
    /// is argv and must stay an array, or resubmitting would re-shell it.
    #[test]
    fn the_two_shell_forms_survive_the_round_trip_distinctly() {
        let sh = step(Action::Shell {
            argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                "make build && make deploy".into(),
            ],
        });
        let argv = step(Action::Shell {
            argv: vec!["./deploy.sh".into(), "--target".into(), "prod".into()],
        });
        let rendered = job_to_toml(
            &job(
                Schedule::Once {
                    at: zoned("2026-07-16T09:00:00-06:00[America/Denver]"),
                },
                vec![("a", sh), ("b", argv)],
                "a",
            ),
            &denver(),
        );
        assert!(
            rendered.contains(r#"run = "make build && make deploy""#),
            "{rendered}"
        );
        assert!(
            rendered.contains(r#"run = ["./deploy.sh", "--target", "prod"]"#),
            "{rendered}"
        );
    }

    /// The §6.2 escape hatch, which is what export always emits: ordered,
    /// fully expressive, no lossy guess about whether a set of edges "is"
    /// an `on.fail`.
    #[test]
    fn transitions_export_as_ordered_explicit_tables() {
        let mut verify = step(Action::Shell {
            argv: vec!["curl".into(), "-fsS".into(), "https://app/health".into()],
        });
        verify.max_visits = Some(10);
        verify.timeout = Some(SignedDuration::from_secs(600));
        verify.transitions = vec![
            Transition {
                when: Condition::ExitEq(0),
                then: Effect::End {
                    outcome: Outcome::Success,
                },
            },
            Transition {
                when: Condition::Stdout(OutputMatch::Contains("starting".into())),
                then: Effect::Goto {
                    step: "verify".into(),
                    after: Some(Wait::Backoff {
                        start: SignedDuration::from_secs(30),
                        factor: 2.0,
                        max: SignedDuration::from_secs(600),
                    }),
                },
            },
            Transition {
                when: Condition::All(vec![Condition::Failed, Condition::ExitIn(vec![1, 2])]),
                then: Effect::Goto {
                    step: "verify".into(),
                    after: None,
                },
            },
            Transition {
                when: Condition::Always,
                then: Effect::End {
                    outcome: Outcome::Failure,
                },
            },
        ];
        let rendered = job_to_toml(
            &job(
                Schedule::Once {
                    at: zoned("2026-07-16T09:00:00-06:00[America/Denver]"),
                },
                vec![("verify", verify)],
                "verify",
            ),
            &denver(),
        );

        assert!(rendered.contains("max_visits = 10"), "{rendered}");
        assert!(rendered.contains(r#"timeout = "10m""#), "{rendered}");
        assert!(rendered.contains("when = { exit = 0 }"), "{rendered}");
        assert!(
            rendered.contains(r#"then = { end = "success" }"#),
            "{rendered}"
        );
        assert!(
            rendered.contains(r#"when = { stdout_contains = "starting" }"#),
            "{rendered}"
        );
        assert!(
            rendered.contains(r#"after = { start = "30s", factor = 2, max = "10m" }"#),
            "{rendered}"
        );
        assert!(
            rendered.contains(r#"when = { all = ["failed", { exit_in = [1, 2] }] }"#),
            "{rendered}"
        );
        // §3.2: first match wins, so the emitted order has to be the stored
        // order — check the catch-all really is last.
        let always = rendered.find(r#"when = "always""#).expect("always edge");
        let first = rendered.find("when = { exit = 0 }").expect("exit edge");
        assert!(
            first < always,
            "transition order was not preserved:\n{rendered}"
        );
    }

    /// Step ids, commands and notification bodies are arbitrary user text;
    /// a quote or a newline in any of them must not produce a broken file.
    /// Asserted as the property that actually matters — escape it, parse it
    /// back, get the original — rather than against hand-computed literals,
    /// which in a test about escaping is the one place a typo hides best.
    #[test]
    fn arbitrary_text_is_escaped_into_valid_toml() {
        fn through_toml(value: &str) -> String {
            let document = format!("v = {}", quote(value));
            let parsed: toml::Value = document
                .parse()
                .unwrap_or_else(|error| panic!("quote({value:?}) is not valid TOML: {error}"));
            parsed["v"].as_str().expect("a string").to_string()
        }

        for value in [
            r#"say "hi""#,
            "a\\b",
            "line\nnext",
            "tab\there",
            "\u{1}",
            "unicode ✓ é — ok",
            "",
        ] {
            assert_eq!(through_toml(value), value, "escaping lost {value:?}");
        }

        // And a whole document survives such text in the places it can land.
        let notify = step(Action::Notify {
            title: r#"deploy "prod" paused"#.into(),
            body: "needs a look\nrun: cued continue j7".into(),
        });
        let rendered = job_to_toml(
            &job(
                Schedule::Once {
                    at: zoned("2026-07-16T09:00:00-06:00[America/Denver]"),
                },
                vec![("nudge", notify)],
                "nudge",
            ),
            &denver(),
        );
        let parsed: toml::Value = rendered.parse().expect("export must be valid TOML");
        assert_eq!(parsed["name"].as_str(), Some("nightly"));
        let steps = parsed["step"].as_array().expect("[[step]]");
        assert_eq!(
            steps[0]["notify"]["title"].as_str(),
            Some(r#"deploy "prod" paused"#)
        );
        assert_eq!(
            steps[0]["notify"]["body"].as_str(),
            Some("needs a look\nrun: cued continue j7")
        );
    }

    /// §2.1: the captured environment is context, not definition. Exporting
    /// it would pin one machine's environment into a file meant to be
    /// resubmitted — and resubmitting should capture afresh.
    #[test]
    fn the_captured_environment_is_not_exported() {
        let mut detail = job(
            Schedule::Once {
                at: zoned("2026-07-16T09:00:00-06:00[America/Denver]"),
            },
            vec![(
                "run",
                step(Action::Shell {
                    argv: vec!["/bin/true".into()],
                }),
            )],
            "run",
        );
        detail
            .env
            .vars
            .insert("AWS_REGION".into(), "us-east-1".into());
        detail.env.stripped.push("DEPLOY_TOKEN".into());

        let rendered = job_to_toml(&detail, &denver());
        assert!(!rendered.contains("AWS_REGION"), "{rendered}");
        assert!(!rendered.contains("DEPLOY_TOKEN"), "{rendered}");
        assert!(rendered.parse::<toml::Value>().is_ok());
    }
}

#[cfg(test)]
mod zone_tests {
    use super::*;
    use crate::model::{CapturedEnv, Graph, Hooks, JobId, JobStatus, Policies, Step};
    use std::collections::BTreeMap;

    /// codex #5: a `Calendar` export declared the *rule's* zone but rendered
    /// `until` — and any `Wait::Until` — in the caller's display zone. On
    /// import every wall clock is read against the one declared zone, so
    /// those instants shifted by the difference between the two.
    ///
    /// A file has one `zone` key, so it must have one zone: for a calendar
    /// job the rule's zone wins and everything is written in it.
    #[test]
    fn a_calendar_export_writes_every_time_in_the_zone_it_declares() {
        let tokyo = jiff::tz::TimeZone::get("Asia/Tokyo").expect("tzdb");
        let until: Timestamp = "2026-12-31T05:00:00Z".parse().expect("until");

        let job = Job {
            id: JobId(1),
            name: None,
            schedule: Schedule::Calendar {
                spec: CalendarSpec::Daily {
                    at: jiff::civil::time(9, 0, 0, 0),
                },
                zone: "America/New_York".into(),
                until: Some(until),
                count: None,
            },
            status: JobStatus::Active,
            approval: None,
            source: crate::model::JobSource::Cli,
            expired_at: None,
            expiry_reason: None,
            graph: Graph {
                entry: "run".into(),
                steps: BTreeMap::from([(
                    "run".to_string(),
                    Step {
                        action: Action::Shell {
                            argv: vec!["/bin/true".into()],
                        },
                        cwd: None,
                        env: None,
                        timeout: None,
                        kill_grace: None,
                        transitions: Vec::new(),
                        max_visits: None,
                        restart_safe: false,
                        missed_wait: None,
                    },
                )]),
            },
            cwd: "/".into(),
            env: CapturedEnv::default(),
            policies: Policies::default(),
            hooks: Hooks::default(),
            created_at: until,
        };

        // Exported by a reader in Tokyo — a zone that is neither UTC nor the
        // rule's, so a mismatch cannot hide.
        let rendered = job_to_toml(&job, &tokyo);

        assert!(
            rendered.contains(r#"zone  = "America/New_York""#),
            "{rendered}"
        );
        // 05:00Z is midnight in New York and 14:00 in Tokyo. The file says
        // New York, so it has to say midnight.
        assert!(
            rendered.contains(r#"until = "2026-12-31 00:00:00""#),
            "`until` was written in the reader's zone, not the declared one:\n{rendered}"
        );
        assert!(
            !rendered.contains("14:00:00"),
            "the reader's zone leaked into a file declaring another:\n{rendered}"
        );
    }
}
