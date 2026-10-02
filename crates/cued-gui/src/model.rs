//! What the window shows, decided away from drawing: which section a job
//! belongs in, and the mark and words for each status. Pure, so it is tested
//! directly rather than through pixels.
use crate::theme::icon;
use cued::model::{ApprovalState, JobStatus, RunStatus};
use cued::proto::{JobEntry, LogAttempt};
use jiff::{SignedDuration, Timestamp};

/// The window's groups, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Section {
    /// Held runs and jobs awaiting approval: nothing moves until someone acts.
    Attention,
    /// A run is executing a step, or waiting between steps.
    Running,
    /// Live jobs whose next run has not started.
    UpNext,
    /// Everything that has ended, newest first.
    Recent,
}

impl Section {
    pub const ALL: [Section; 4] = [
        Section::Attention,
        Section::Running,
        Section::UpNext,
        Section::Recent,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Section::Attention => "Needs attention",
            Section::Running => "Running",
            Section::UpNext => "Up next",
            Section::Recent => "Recent",
        }
    }
}

/// How a mark is colored, resolved against the palette when drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Accent,
    Success,
    Danger,
    Warning,
    Muted,
}

/// A status as the window shows it: an icon, its tone, and a word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mark {
    pub icon: &'static str,
    pub tone: Tone,
    pub label: String,
}

impl Mark {
    fn new(icon: &'static str, tone: Tone, label: impl Into<String>) -> Self {
        Self {
            icon,
            tone,
            label: label.into(),
        }
    }
}

fn awaiting_approval(job: &JobEntry) -> bool {
    job.status.is_live()
        && job
            .approval
            .as_ref()
            .is_some_and(|approval| approval.state == ApprovalState::Pending)
}

fn run_status(job: &JobEntry) -> Option<RunStatus> {
    job.last_run.as_ref().map(|run| run.status)
}

/// A run that has started and not ended: executing a step, or waiting on a
/// sleep edge between steps.
fn in_progress(job: &JobEntry) -> bool {
    job.last_run.as_ref().is_some_and(|run| {
        run.status == RunStatus::Running
            || (run.status == RunStatus::Waiting && run.started_at.is_some())
    })
}

pub fn section(job: &JobEntry) -> Section {
    if awaiting_approval(job) || run_status(job) == Some(RunStatus::Held) {
        Section::Attention
    } else if in_progress(job) {
        Section::Running
    } else if job.status.is_live()
        && (job.next_at.is_some()
            || matches!(
                run_status(job),
                None | Some(RunStatus::Pending | RunStatus::Waiting)
            ))
    {
        Section::UpNext
    } else {
        Section::Recent
    }
}

/// When a job last ended, for ordering Recent.
fn ended(job: &JobEntry) -> Option<Timestamp> {
    job.last_run
        .as_ref()
        .and_then(|run| run.ended_at)
        .or(job.expired_at)
}

/// The non-empty sections, each in its own order: Running by start, Up next
/// by when it fires (soonest first), Recent newest first, the rest by id.
pub fn group(jobs: &[JobEntry]) -> Vec<(Section, Vec<&JobEntry>)> {
    let mut groups: Vec<(Section, Vec<&JobEntry>)> = Section::ALL
        .into_iter()
        .map(|section| (section, Vec::new()))
        .collect();
    for job in jobs {
        let index = Section::ALL
            .iter()
            .position(|s| *s == section(job))
            .expect("every section is listed");
        groups[index].1.push(job);
    }
    for (section, jobs) in &mut groups {
        match section {
            Section::Attention => jobs.sort_by_key(|job| job.id),
            Section::Running => {
                jobs.sort_by_key(|job| (job.last_run.as_ref().and_then(|r| r.started_at), job.id))
            }
            // `None` (nothing armed) sorts after every instant.
            Section::UpNext => jobs.sort_by_key(|job| (job.next_at.is_none(), job.next_at, job.id)),
            Section::Recent => {
                jobs.sort_by_key(|job| std::cmp::Reverse((ended(job), job.id)));
            }
        }
    }
    groups.retain(|(_, jobs)| !jobs.is_empty());
    groups
}

/// The job's mark: what most needs saying about it now.
pub fn job_mark(job: &JobEntry) -> Mark {
    if awaiting_approval(job) {
        return Mark::new(icon::WARNING_CIRCLE, Tone::Warning, "awaiting approval");
    }
    match run_status(job) {
        Some(RunStatus::Held) => return Mark::new(icon::PAUSE_CIRCLE, Tone::Warning, "held"),
        Some(RunStatus::Running) => {
            return Mark::new(icon::CIRCLE_NOTCH, Tone::Accent, "running");
        }
        _ if in_progress(job) => return Mark::new(icon::CLOCK, Tone::Accent, "waiting"),
        _ => {}
    }
    if section(job) == Section::UpNext {
        return if job.status == JobStatus::Paused {
            Mark::new(icon::PAUSE_CIRCLE, Tone::Muted, "paused")
        } else {
            Mark::new(icon::CIRCLE_DASHED, Tone::Muted, "scheduled")
        };
    }
    match (job.status, run_status(job)) {
        (JobStatus::Expired, _) => Mark::new(icon::PROHIBIT, Tone::Muted, "expired"),
        (_, Some(RunStatus::Done)) => Mark::new(icon::CHECK_CIRCLE, Tone::Success, "done"),
        (_, Some(RunStatus::Failed)) => Mark::new(icon::X_CIRCLE, Tone::Danger, "failed"),
        (_, Some(RunStatus::Missed)) => Mark::new(icon::SKIP_FORWARD_CIRCLE, Tone::Muted, "missed"),
        (_, Some(RunStatus::Skipped)) => {
            Mark::new(icon::SKIP_FORWARD_CIRCLE, Tone::Muted, "skipped")
        }
        (JobStatus::Cancelled, _) | (_, Some(RunStatus::Cancelled)) => {
            Mark::new(icon::MINUS_CIRCLE, Tone::Muted, "cancelled")
        }
        _ => Mark::new(icon::CHECK_CIRCLE, Tone::Muted, job.status.as_str()),
    }
}

/// "j3 backup", or "j3" for an unnamed job.
pub fn job_title(job: &JobEntry) -> String {
    match &job.name {
        Some(name) => format!("{} {name}", job.id),
        None => job.id.to_string(),
    }
}

/// "in 5m", "5m ago", or "now".
pub fn relative(at: Timestamp, now: Timestamp) -> String {
    let seconds = at.as_second() - now.as_second();
    if seconds.abs() < 1 {
        return "now".into();
    }
    let human = cued::timeparse::describe_duration(SignedDuration::from_secs(seconds.abs()));
    if seconds > 0 {
        format!("in {human}")
    } else {
        format!("{human} ago")
    }
}

/// From `start` to `end`, or to `now` while it is still going.
pub fn elapsed(start: Timestamp, end: Option<Timestamp>, now: Timestamp) -> String {
    let end = end.unwrap_or(now);
    cued::timeparse::describe_duration(end.duration_since(start).max(SignedDuration::ZERO))
}

/// The row's second line: where the job stands, in its section's terms.
pub fn job_summary(job: &JobEntry, now: Timestamp) -> String {
    let run = job.last_run.as_ref();
    let step = run.and_then(|run| run.step.as_deref());
    match section(job) {
        Section::Attention if awaiting_approval(job) => match job.next_at {
            Some(at) => format!(
                "review with cued approve {} · due {}",
                job.id,
                relative(at, now)
            ),
            None => format!("review with cued approve {}", job.id),
        },
        Section::Attention => match step {
            Some(step) => format!("held at {step} · cued continue or retry"),
            None => "held · cued continue or retry".into(),
        },
        Section::Running => {
            let run = run.expect("a running job has a run");
            let took = run
                .started_at
                .map(|start| elapsed(start, None, now))
                .unwrap_or_default();
            match (run.status, step, job.next_at) {
                (RunStatus::Waiting, Some(step), Some(at)) => {
                    format!("{took} · {step} {}", relative(at, now))
                }
                (_, Some(step), _) => format!("{took} · {step}"),
                _ => took,
            }
        }
        Section::UpNext => {
            let next = match job.next_at {
                Some(at) => format!("next {}", relative(at, now)),
                None => "not armed".into(),
            };
            match run.filter(|run| run.status.is_settled()) {
                Some(last) => format!("{next} · last {}", last.status.as_str()),
                None => next,
            }
        }
        Section::Recent => {
            let mut text = job_mark(job).label;
            if let Some(reason) = run.and_then(|run| run.fail_reason.as_deref()) {
                text.push_str(&format!(" ({reason})"));
            }
            if let Some(at) = ended(job) {
                text.push_str(&format!(" {}", relative(at, now)));
            }
            if let Some(run) = run
                && let (Some(start), Some(end)) = (run.started_at, run.ended_at)
            {
                text.push_str(&format!(" · took {}", elapsed(start, Some(end), now)));
            }
            text
        }
    }
}

/// Whether anything shown counts up by the second, so the window repaints
/// while it is open and only then.
pub fn ticking(jobs: &[JobEntry], attempts: &[LogAttempt]) -> bool {
    jobs.iter().any(in_progress) || attempts.iter().any(|attempt| attempt.running)
}

/// One attempt's mark. An ended attempt with no exit code ran no process to
/// completion: a notification step, or one killed with its daemon.
pub fn attempt_mark(attempt: &LogAttempt) -> Mark {
    if attempt.running {
        Mark::new(icon::CIRCLE_NOTCH, Tone::Accent, "running")
    } else if attempt.timed_out {
        Mark::new(icon::CLOCK, Tone::Danger, "timed out")
    } else {
        match (attempt.exit_code, attempt.ended_at) {
            (Some(0), _) => Mark::new(icon::CHECK_CIRCLE, Tone::Success, "exit 0"),
            (Some(code), _) => Mark::new(icon::X_CIRCLE, Tone::Danger, format!("exit {code}")),
            (None, Some(_)) => Mark::new(icon::MINUS_CIRCLE, Tone::Muted, "no exit code"),
            (None, None) => Mark::new(icon::WARNING_CIRCLE, Tone::Warning, "interrupted"),
        }
    }
}

/// The attempt a log pane shows when the viewer hasn't picked one: the one
/// running, else the latest.
pub fn default_attempt(attempts: &[LogAttempt]) -> Option<&LogAttempt> {
    attempts
        .iter()
        .find(|attempt| attempt.running)
        .or(attempts.last())
}

/// What a viewer can do to a job, mirroring the CLI verbs of the same names.
/// Approval is deliberately absent: it stays a review at a terminal (§7.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Continue,
    Retry,
    Pause,
    Resume,
    Cancel,
}

impl Verb {
    pub fn label(self) -> &'static str {
        match self {
            Verb::Continue => "Continue",
            Verb::Retry => "Retry",
            Verb::Pause => "Pause",
            Verb::Resume => "Resume",
            Verb::Cancel => "Cancel",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Verb::Continue => "Resume the held run from the step it stopped at",
            Verb::Retry => "Run the latest run again, from the held step or the start",
            Verb::Pause => "Start no new work; a step already running finishes",
            Verb::Resume => "Let the job start work again",
            Verb::Cancel => "Stop the job for good, terminating any running step",
        }
    }
}

/// The verbs that apply to a job as it stands, in button order. The daemon
/// has the last word; this only keeps buttons that can't work off screen.
pub fn verbs(job: &JobEntry) -> Vec<Verb> {
    let mut verbs = Vec::new();
    let approved = !awaiting_approval(job) && job.status != JobStatus::Expired;
    match run_status(job) {
        Some(RunStatus::Held) if approved => verbs.extend([Verb::Continue, Verb::Retry]),
        Some(RunStatus::Done | RunStatus::Failed | RunStatus::Missed | RunStatus::Cancelled)
            if approved =>
        {
            verbs.push(Verb::Retry)
        }
        _ => {}
    }
    match job.status {
        JobStatus::Active if !awaiting_approval(job) => verbs.push(Verb::Pause),
        JobStatus::Paused if !awaiting_approval(job) => verbs.push(Verb::Resume),
        _ => {}
    }
    if job.status.is_live() {
        verbs.push(Verb::Cancel);
    }
    verbs
}

#[cfg(test)]
mod tests {
    use super::*;
    use cued::model::{Approval, JobId, JobSource, RunId};
    use cued::proto::RunEntry;

    fn at(seconds: i64) -> Timestamp {
        Timestamp::from_second(1_800_000_000 + seconds).unwrap()
    }

    fn job(id: i64, status: JobStatus, run: Option<RunEntry>) -> JobEntry {
        JobEntry {
            id: JobId(id),
            name: None,
            status,
            approval: None,
            source: JobSource::Cli,
            expired_at: None,
            expiry_reason: None,
            action: "./build.sh".into(),
            next_at: None,
            last_run: run,
        }
    }

    fn run(status: RunStatus) -> RunEntry {
        RunEntry {
            id: RunId(1),
            status,
            started_at: None,
            ended_at: None,
            step: None,
            fail_reason: None,
        }
    }

    #[test]
    fn jobs_land_in_the_section_their_state_calls_for() {
        let mut held = job(1, JobStatus::Active, Some(run(RunStatus::Held)));
        held.last_run.as_mut().unwrap().step = Some("deploy".into());
        let mut approval = job(2, JobStatus::Active, None);
        approval.approval = Some(Approval {
            state: ApprovalState::Pending,
            definition_hash: [0; 32],
            approved_at: None,
        });
        let running = job(3, JobStatus::Active, Some(run(RunStatus::Running)));
        let mut between = job(4, JobStatus::Active, Some(run(RunStatus::Waiting)));
        between.last_run.as_mut().unwrap().started_at = Some(at(0));
        let not_started = job(5, JobStatus::Active, Some(run(RunStatus::Waiting)));
        let mut recurring = job(6, JobStatus::Active, Some(run(RunStatus::Done)));
        recurring.next_at = Some(at(60));
        let finished = job(7, JobStatus::Done, Some(run(RunStatus::Done)));
        let cancelled = job(8, JobStatus::Cancelled, Some(run(RunStatus::Cancelled)));

        assert_eq!(section(&held), Section::Attention);
        assert_eq!(section(&approval), Section::Attention);
        assert_eq!(section(&running), Section::Running);
        assert_eq!(section(&between), Section::Running, "waiting mid-run");
        assert_eq!(section(&not_started), Section::UpNext);
        assert_eq!(section(&recurring), Section::UpNext);
        assert_eq!(section(&finished), Section::Recent);
        assert_eq!(section(&cancelled), Section::Recent);

        assert_eq!(job_mark(&between).label, "waiting");
        assert_eq!(job_mark(&recurring).label, "scheduled");
        assert_eq!(job_summary(&recurring, at(0)), "next in 1m · last done");
        assert_eq!(
            job_summary(&held, at(0)),
            "held at deploy · cued continue or retry"
        );
    }

    #[test]
    fn verbs_follow_the_job_and_its_run() {
        use Verb::*;
        let held = job(1, JobStatus::Active, Some(run(RunStatus::Held)));
        assert_eq!(verbs(&held), [Continue, Retry, Pause, Cancel]);
        let running = job(2, JobStatus::Active, Some(run(RunStatus::Running)));
        assert_eq!(verbs(&running), [Pause, Cancel]);
        let paused = job(3, JobStatus::Paused, Some(run(RunStatus::Done)));
        assert_eq!(verbs(&paused), [Retry, Resume, Cancel]);
        let failed = job(4, JobStatus::Done, Some(run(RunStatus::Failed)));
        assert_eq!(verbs(&failed), [Retry]);
        let expired = job(5, JobStatus::Expired, None);
        assert_eq!(verbs(&expired), []);
        let mut pending = job(6, JobStatus::Active, None);
        pending.approval = Some(Approval {
            state: ApprovalState::Pending,
            definition_hash: [0; 32],
            approved_at: None,
        });
        assert_eq!(verbs(&pending), [Cancel], "approval stays at the terminal");
        let skipped = job(7, JobStatus::Done, Some(run(RunStatus::Skipped)));
        assert_eq!(verbs(&skipped), []);
    }

    #[test]
    fn sections_keep_their_own_order_and_drop_when_empty() {
        let mut soon = job(1, JobStatus::Active, None);
        soon.next_at = Some(at(60));
        let mut later = job(2, JobStatus::Active, None);
        later.next_at = Some(at(3600));
        let mut old = job(3, JobStatus::Done, Some(run(RunStatus::Done)));
        old.last_run.as_mut().unwrap().ended_at = Some(at(-600));
        let mut new = job(4, JobStatus::Done, Some(run(RunStatus::Failed)));
        new.last_run.as_mut().unwrap().ended_at = Some(at(-60));

        let jobs = [later, old, new, soon];
        let groups = group(&jobs);
        let ids: Vec<(Section, Vec<i64>)> = groups
            .iter()
            .map(|(section, jobs)| (*section, jobs.iter().map(|job| job.id.0).collect()))
            .collect();
        assert_eq!(
            ids,
            [(Section::UpNext, vec![1, 2]), (Section::Recent, vec![4, 3])]
        );
    }

    #[test]
    fn a_running_job_reads_as_its_elapsed_time_and_step() {
        let mut running = job(1, JobStatus::Active, Some(run(RunStatus::Running)));
        let entry = running.last_run.as_mut().unwrap();
        entry.started_at = Some(at(0));
        entry.step = Some("test".into());
        assert_eq!(job_summary(&running, at(80)), "1m 20s · test");
        assert!(ticking(std::slice::from_ref(&running), &[]));

        let mut failed = job(2, JobStatus::Done, Some(run(RunStatus::Failed)));
        let entry = failed.last_run.as_mut().unwrap();
        entry.started_at = Some(at(0));
        entry.ended_at = Some(at(12));
        entry.fail_reason = Some("deadline".into());
        assert_eq!(
            job_summary(&failed, at(72)),
            "failed (deadline) 1m ago · took 12s"
        );
        assert!(!ticking(std::slice::from_ref(&failed), &[]));
    }

    #[test]
    fn attempts_say_how_they_ended() {
        let attempt =
            |exit_code: Option<i32>, ended: bool, running: bool, timed_out: bool| LogAttempt {
                step: "build".into(),
                attempt: 1,
                started_at: at(0),
                ended_at: ended.then(|| at(5)),
                running,
                exit_code,
                timed_out,
            };
        let label = |a: &LogAttempt| attempt_mark(a).label;
        assert_eq!(label(&attempt(None, false, true, false)), "running");
        assert_eq!(label(&attempt(Some(0), true, false, false)), "exit 0");
        assert_eq!(label(&attempt(Some(2), true, false, false)), "exit 2");
        assert_eq!(label(&attempt(None, true, false, true)), "timed out");
        assert_eq!(label(&attempt(None, true, false, false)), "no exit code");
        assert_eq!(label(&attempt(None, false, false, false)), "interrupted");

        let attempts = [
            attempt(Some(0), true, false, false),
            attempt(None, false, true, false),
            attempt(Some(1), true, false, false),
        ];
        assert!(default_attempt(&attempts).unwrap().running);
        assert_eq!(default_attempt(&attempts[..1]).unwrap().exit_code, Some(0));
        assert!(default_attempt(&[]).is_none());
    }
}
