//! Interaction tests: drive the real window code through egui's
//! accessibility tree, with the backend's two ends held by the test.
use crate::app::App;
use crate::backend::{Backend, Command, Detail, Link, Log, Update};
use crate::model::Verb;
use cued::model::{JobId, JobSource, JobStatus, RunId, RunStatus};
use cued::proto::{JobEntry, LogAttempt, RunEntry};
use egui_kittest::{Harness, kittest::Queryable};
use jiff::{SignedDuration, Timestamp};
use std::sync::mpsc::{Receiver, Sender};

struct Ui {
    harness: Harness<'static, App>,
    updates: Sender<Update>,
    commands: Receiver<Command>,
}

impl Ui {
    fn new() -> Self {
        let (backend, updates, commands) = Backend::detached();
        let harness = Harness::builder()
            .with_size([1100.0, 700.0])
            .build_ui_state(|ui, app: &mut App| app.show(ui), App::new(backend));
        let mut ui = Self {
            harness,
            updates,
            commands,
        };
        ui.settle();
        ui
    }

    fn settle(&mut self) {
        for _ in 0..4 {
            self.harness.step();
        }
    }

    fn send(&mut self, update: Update) {
        self.updates.send(update).unwrap();
        self.settle();
    }

    fn command(&self) -> Command {
        self.commands
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("a command")
    }
}

fn ago(seconds: i64) -> Timestamp {
    Timestamp::now()
        .checked_sub(SignedDuration::from_secs(seconds))
        .unwrap()
}

fn job(id: i64, name: &str, status: JobStatus, run: Option<RunEntry>) -> JobEntry {
    JobEntry {
        id: JobId(id),
        name: Some(name.into()),
        status,
        approval: None,
        source: JobSource::Cli,
        expired_at: None,
        expiry_reason: None,
        action: format!("./{name}.sh"),
        next_at: None,
        last_run: run,
    }
}

fn run(status: RunStatus, step: Option<&str>, ended: bool) -> RunEntry {
    RunEntry {
        id: RunId(1),
        status,
        started_at: Some(ago(90)),
        ended_at: ended.then(|| ago(30)),
        step: step.map(String::from),
        fail_reason: None,
    }
}

fn jobs() -> Vec<JobEntry> {
    vec![
        job(
            1,
            "pipeline",
            JobStatus::Active,
            Some(run(RunStatus::Running, Some("test"), false)),
        ),
        job(
            2,
            "deploy",
            JobStatus::Active,
            Some(run(RunStatus::Held, Some("ship"), false)),
        ),
        job(
            3,
            "backup",
            JobStatus::Done,
            Some(run(RunStatus::Done, None, true)),
        ),
    ]
}

fn attempt(step: &str, exit_code: Option<i32>, running: bool) -> LogAttempt {
    LogAttempt {
        step: step.into(),
        attempt: 1,
        started_at: ago(60),
        ended_at: (!running).then(|| ago(40)),
        running,
        exit_code,
        timed_out: false,
        outcome_edge: None,
        epoch: 0,
    }
}

fn log(step: &str, text: &str) -> Log {
    Log {
        step: step.into(),
        attempt: 1,
        text: text.into(),
        truncated: false,
    }
}

#[test]
fn jobs_are_grouped_by_where_they_stand() {
    let mut ui = Ui::new();
    ui.harness.get_by_label("Connecting…");
    ui.send(Update::Link(Link::Live));
    ui.send(Update::Jobs(Ok(jobs())));

    ui.harness.get_by_label("Live");
    ui.harness.get_by_label("NEEDS ATTENTION 1");
    ui.harness.get_by_label("RUNNING 1");
    ui.harness.get_by_label("RECENT 1");
    assert!(ui.harness.query_by_label("UP NEXT 0").is_none());
    ui.harness.get_by_label("j1 pipeline, running, Running");
    ui.harness.get_by_label("j2 deploy, held, Needs attention");
    ui.harness.get_by_label("j3 backup, done, Recent");
    ui.harness.get_by_label("Select a job to see its steps.");
}

#[test]
fn selecting_a_job_shows_its_steps_and_the_running_log() {
    let mut ui = Ui::new();
    ui.send(Update::Link(Link::Live));
    ui.send(Update::Jobs(Ok(jobs())));

    ui.harness
        .get_by_label("j1 pipeline, running, Running")
        .click();
    ui.settle();
    assert_eq!(ui.command(), Command::Select(Some(JobId(1))));
    ui.harness.get_by_label("Loading…");

    ui.send(Update::Detail(Some(Ok(Detail {
        job: JobId(1),
        run: Some(RunId(1)),
        attempts: vec![
            attempt("build", Some(0), false),
            attempt("test", None, true),
        ],
        log: Some(log("test", "running tests\n")),
        graph: None,
    }))));
    ui.harness.get_by_label_contains("build, exit 0, 20s");
    ui.harness.get_by_label_contains("test, running, 1m");
    ui.harness.get_by_label("OUTPUT · TEST");
    ui.harness.get_by_label("running tests\n");

    // The followed log grows without a new manifest.
    ui.send(Update::Log {
        job: JobId(1),
        run: RunId(1),
        log: log("test", "running tests\nall passed\n"),
    });
    ui.harness.get_by_label("running tests\nall passed\n");

    // Picking another attempt asks for its log.
    ui.harness.get_by_label_contains("build, exit 0").click();
    ui.settle();
    assert_eq!(ui.command(), Command::ShowLog(Some(("build".into(), 1))));
}

#[test]
fn a_detail_for_a_job_no_longer_selected_is_ignored() {
    let mut ui = Ui::new();
    ui.send(Update::Link(Link::Live));
    ui.send(Update::Jobs(Ok(jobs())));
    ui.harness.get_by_label("j3 backup, done, Recent").click();
    ui.settle();
    assert_eq!(ui.command(), Command::Select(Some(JobId(3))));

    // A slow fetch for an earlier selection lands after the new one.
    ui.send(Update::Detail(Some(Ok(Detail {
        job: JobId(1),
        run: Some(RunId(1)),
        attempts: vec![attempt("build", Some(0), false)],
        log: None,
        graph: None,
    }))));
    assert!(
        ui.harness
            .query_by_label_contains("build, exit 0")
            .is_none()
    );
    ui.harness.get_by_label("Loading…");
}

#[test]
fn no_daemon_offers_to_start_one() {
    let mut ui = Ui::new();
    ui.send(Update::Link(Link::NoDaemon));
    ui.harness.get_by_label("Daemon not running");
    ui.harness.get_by_label("Start daemon").click();
    ui.settle();
    assert_eq!(ui.command(), Command::StartDaemon);
}

#[test]
fn an_empty_list_says_how_to_schedule_something() {
    let mut ui = Ui::new();
    ui.send(Update::Link(Link::Live));
    ui.send(Update::Jobs(Ok(Vec::new())));
    ui.harness.get_by_label("Nothing scheduled.");
}

fn select_held(ui: &mut Ui) {
    ui.send(Update::Link(Link::Live));
    ui.send(Update::Jobs(Ok(jobs())));
    ui.harness
        .get_by_label("j2 deploy, held, Needs attention")
        .click();
    ui.settle();
    assert_eq!(ui.command(), Command::Select(Some(JobId(2))));
}

#[test]
fn controls_act_on_the_selected_job_and_report_back() {
    let mut ui = Ui::new();
    select_held(&mut ui);
    for label in ["Continue", "Retry", "Pause", "Cancel"] {
        ui.harness.get_by_label(label);
    }
    assert!(ui.harness.query_by_label("Resume").is_none());

    ui.harness.get_by_label("Continue").click();
    ui.settle();
    assert_eq!(ui.command(), Command::Act(JobId(2), Verb::Continue));
    // No second request while the first is unanswered.
    ui.harness.get_by_label("Retry").click();
    ui.settle();
    assert!(
        ui.commands.try_recv().is_err(),
        "a button stayed live mid-request"
    );

    ui.send(Update::Acted(Ok("j2.r1 continuing from ship".into())));
    ui.harness.get_by_label("j2.r1 continuing from ship");
    ui.harness.get_by_label("Dismiss").click();
    ui.settle();
    assert!(
        ui.harness
            .query_by_label("j2.r1 continuing from ship")
            .is_none()
    );

    ui.harness.get_by_label("Retry").click();
    ui.settle();
    assert_eq!(ui.command(), Command::Act(JobId(2), Verb::Retry));
    ui.send(Update::Acted(Err("j2.r1 is still live".into())));
    ui.harness.get_by_label("j2.r1 is still live");
}

#[test]
fn cancel_asks_first() {
    let mut ui = Ui::new();
    select_held(&mut ui);

    ui.harness.get_by_label("Cancel").click();
    ui.settle();
    ui.harness.get_by_label("Cancel j2 deploy?");
    ui.harness.get_by_label("Keep it").click();
    ui.settle();
    assert!(ui.harness.query_by_label("Cancel j2 deploy?").is_none());
    assert!(
        ui.commands.try_recv().is_err(),
        "kept, yet something was sent"
    );

    ui.harness.get_by_label("Cancel").click();
    ui.settle();
    ui.harness.get_by_label("Cancel job").click();
    ui.settle();
    assert_eq!(ui.command(), Command::Act(JobId(2), Verb::Cancel));
}

#[test]
fn the_steps_still_to_come_show_after_those_that_ran() {
    let mut ui = Ui::new();
    ui.send(Update::Link(Link::Live));
    ui.send(Update::Jobs(Ok(jobs())));
    ui.harness
        .get_by_label("j1 pipeline, running, Running")
        .click();
    ui.settle();
    assert_eq!(ui.command(), Command::Select(Some(JobId(1))));

    // j1 is listed as running at "test": build ran, test is running, and
    // ship is still ahead.
    let graph = cued::submit::chain_graph(
        "./build.sh",
        &[
            cued::submit::Link::Then("./test.sh".into()),
            cued::submit::Link::Then("./ship.sh".into()),
        ],
        cued::submit::ChainFailure::Stop,
    )
    .unwrap();
    let names: Vec<String> = graph.steps.keys().cloned().collect();
    let (first, second, third) = (&names[0], &names[1], &names[2]);
    let mut ran = attempt(first, Some(0), false);
    ran.outcome_edge = Some(0);
    let running = attempt(second, None, true);
    let mut jobs = jobs();
    jobs[0].last_run.as_mut().unwrap().step = Some(second.clone());
    ui.send(Update::Jobs(Ok(jobs)));
    ui.send(Update::Detail(Some(Ok(Detail {
        job: JobId(1),
        run: Some(RunId(1)),
        attempts: vec![ran, running],
        log: None,
        graph: Some(graph),
    }))));

    ui.harness
        .get_by_label_contains(&format!("{first}, exit 0, 20s, succeeded → goto {second}"));
    ui.harness.get_by_label_contains(&format!(
        "{second}, running, 1m, then succeeded → goto {third}"
    ));
    ui.harness.get_by_label(&format!("{third}, pending"));
}

#[test]
fn the_graph_view_draws_each_step_as_a_box_and_a_click_shows_its_output() {
    let mut ui = Ui::new();
    ui.send(Update::Link(Link::Live));
    ui.send(Update::Jobs(Ok(jobs())));
    ui.harness.get_by_label("j3 backup, done, Recent").click();
    ui.settle();
    assert_eq!(ui.command(), Command::Select(Some(JobId(3))));
    let graph = cued::submit::chain_graph(
        "./dump.sh",
        &[cued::submit::Link::Then("./upload.sh".into())],
        cued::submit::ChainFailure::Stop,
    )
    .unwrap();
    let names: Vec<String> = graph.steps.keys().cloned().collect();
    let mut first = attempt(&names[0], Some(0), false);
    first.outcome_edge = Some(0);
    let second = attempt(&names[1], Some(0), false);
    ui.send(Update::Detail(Some(Ok(Detail {
        job: JobId(3),
        run: Some(RunId(1)),
        attempts: vec![first, second],
        log: Some(log(&names[1], "uploaded\n")),
        graph: Some(graph),
    }))));

    assert!(
        ui.harness.query_by_label_contains(" box, ").is_none(),
        "the list is the default"
    );
    ui.harness.get_by_label("Graph").click();
    ui.settle();
    ui.harness
        .get_by_label(&format!("{} box, exit 0 · 20s", names[0]))
        .click();
    ui.settle();
    assert_eq!(ui.command(), Command::ShowLog(Some((names[0].clone(), 1))));
    ui.harness.get_by_label("List").click();
    ui.settle();
    assert!(ui.harness.query_by_label_contains(" box, ").is_none());
}
