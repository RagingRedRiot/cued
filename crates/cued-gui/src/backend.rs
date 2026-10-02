//! The daemon, off the UI thread. Two threads, both asleep while nothing
//! changes:
//!
//! - the watcher holds the §5.1 subscription and turns its notices into
//!   messages, reconnecting when the daemon stops or upgrades;
//! - the fetcher refetches what the window shows on each batch of messages
//!   and hands the results to the UI, asking it to repaint.
//!
//! The only timer is the log tail of a step that is running, re-read once a
//! second while it is on screen: a step's output is a file, and the daemon
//! sends no notice per line.
use crate::model::Verb;
use cued::client::{self, CallError, Subscription};
use cued::model::{Graph, JobId, RunId};
use cued::paths::Paths;
use cued::proto::{JobEntry, LogAttempt, RequestBody, Response};
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

/// The most of a log the pane holds: its end.
pub const LOG_TAIL: u64 = 64 * 1024;
/// How often a running step's log is re-read while it is shown.
pub const FOLLOW: Duration = Duration::from_secs(1);
/// Reconnect delays: at once after a stream ends (an upgrade's exec keeps
/// the socket queueing), then backing off while there is no daemon.
const RETRY_MIN: Duration = Duration::from_millis(250);
const RETRY_MAX: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Connecting,
    Live,
    NoDaemon,
    /// The stream or a request failed; retrying.
    Lost(String),
}

#[derive(Debug, Clone)]
pub struct Log {
    pub step: String,
    pub attempt: u32,
    pub text: String,
    /// Only the end of the file is held.
    pub truncated: bool,
}

/// The selected job's latest run, step by step.
#[derive(Debug, Clone)]
pub struct Detail {
    pub job: JobId,
    /// `None` while the job has no run yet.
    pub run: Option<RunId>,
    pub attempts: Vec<LogAttempt>,
    pub log: Option<Log>,
    /// The job's workflow, for the steps not yet run; `None` if it couldn't
    /// be fetched, and the run's history is shown alone.
    pub graph: Option<Graph>,
}

pub enum Update {
    Link(Link),
    Jobs(Result<Vec<JobEntry>, String>),
    /// For the job selected when it was fetched; `None` when none is.
    Detail(Option<Result<Detail, String>>),
    /// A fresh read of the running attempt's log already shown in `Detail`.
    Log {
        job: JobId,
        run: RunId,
        log: Log,
    },
    /// What a [`Command::Act`] did, in a sentence. Its effect on the job
    /// arrives as a change notice like any other.
    Acted(Result<String, String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Select(Option<JobId>),
    /// Show this attempt's log, or `None` for the running or latest one.
    ShowLog(Option<(String, u32)>),
    StartDaemon,
    Act(JobId, Verb),
}

enum Message {
    Connected,
    Changed,
    NoDaemon,
    Lost(Option<String>),
    Command(Command),
}

/// The UI's end.
pub struct Backend {
    commands: Sender<Message>,
    updates: Receiver<Update>,
}

impl Backend {
    /// Start both threads. `cued` is the binary that starts a daemon, from
    /// [`crate::find_cued`]; with `auto_start`, one is started if none is
    /// running when the window opens (never later: a daemon stopped on
    /// purpose stays stopped).
    pub fn start(
        paths: Paths,
        cued: Option<PathBuf>,
        auto_start: bool,
        repaint: impl Fn() + Send + 'static,
    ) -> Self {
        let (messages, inbox) = mpsc::channel();
        let (updates, outbox) = mpsc::channel();
        let (wake, woken) = mpsc::channel();
        let watcher = {
            let paths = paths.clone();
            let messages = messages.clone();
            move || watch(&paths, &messages, &woken)
        };
        std::thread::Builder::new()
            .name("cued-watch".into())
            .spawn(watcher)
            .expect("spawning the watcher thread");
        let fetcher = Fetcher {
            paths,
            cued,
            auto_start,
            updates,
            repaint: Box::new(repaint),
            wake,
            selected: None,
            log_choice: None,
            jobs: Vec::new(),
            link: Link::Connecting,
            following: None,
            graphs: std::collections::HashMap::new(),
        };
        std::thread::Builder::new()
            .name("cued-fetch".into())
            .spawn(move || fetcher.run(&inbox))
            .expect("spawning the fetch thread");
        Self {
            commands: messages,
            updates: outbox,
        }
    }

    /// A backend with no threads, for tests: the returned ends stand in for
    /// the daemon side.
    #[cfg(test)]
    pub fn detached() -> (Self, Sender<Update>, Receiver<Command>) {
        let (messages, inbox) = mpsc::channel();
        let (updates, outbox) = mpsc::channel();
        let (commands, received) = mpsc::channel();
        std::thread::spawn(move || {
            while let Ok(message) = inbox.recv() {
                if let Message::Command(command) = message
                    && commands.send(command).is_err()
                {
                    return;
                }
            }
        });
        (
            Self {
                commands: messages,
                updates: outbox,
            },
            updates,
            received,
        )
    }

    pub fn send(&self, command: Command) {
        let _ = self.commands.send(Message::Command(command));
    }

    pub fn try_recv(&self) -> Option<Update> {
        self.updates.try_recv().ok()
    }
}

/// The watcher thread: subscribe, forward notices, reconnect. Ends when the
/// fetcher is gone.
fn watch(paths: &Paths, messages: &Sender<Message>, woken: &Receiver<()>) {
    let mut delay = RETRY_MIN;
    loop {
        let sent = match Subscription::open(paths) {
            Ok(mut subscription) => {
                delay = RETRY_MIN;
                messages.send(Message::Connected).is_ok() && {
                    let ended = loop {
                        match subscription.changed() {
                            Ok(true) => {
                                if messages.send(Message::Changed).is_err() {
                                    return;
                                }
                            }
                            Ok(false) => break None,
                            Err(error) => break Some(format!("{error:#}")),
                        }
                    };
                    messages.send(Message::Lost(ended)).is_ok()
                }
            }
            Err(CallError::NoDaemon) => messages.send(Message::NoDaemon).is_ok(),
            Err(error) => messages
                .send(Message::Lost(Some(error.to_string())))
                .is_ok(),
        };
        if !sent {
            return;
        }
        // Woken early when the fetcher starts a daemon.
        if let Err(RecvTimeoutError::Disconnected) = woken.recv_timeout(delay) {
            return;
        }
        delay = (delay * 2).min(RETRY_MAX);
    }
}

struct Fetcher {
    paths: Paths,
    cued: Option<PathBuf>,
    auto_start: bool,
    updates: Sender<Update>,
    repaint: Box<dyn Fn() + Send>,
    wake: Sender<()>,
    selected: Option<JobId>,
    log_choice: Option<(String, u32)>,
    /// The latest list, to find the selected job's run.
    jobs: Vec<JobEntry>,
    link: Link,
    /// The running attempt whose log is on screen, re-read every [`FOLLOW`].
    following: Option<(JobId, RunId, String, u32)>,
    /// Workflows by job. A definition never changes once submitted, so each
    /// is fetched once.
    graphs: std::collections::HashMap<JobId, Graph>,
}

impl Fetcher {
    fn run(mut self, inbox: &Receiver<Message>) {
        loop {
            let first = if self.following.is_some() {
                match inbox.recv_timeout(FOLLOW) {
                    Ok(message) => Some(message),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            } else {
                match inbox.recv() {
                    Ok(message) => Some(message),
                    Err(_) => return,
                }
            };
            let Some(first) = first else {
                if !self.follow() {
                    return;
                }
                continue;
            };
            // Whatever else arrived meanwhile is one batch: one refetch.
            let batch = std::iter::once(first).chain(std::iter::from_fn(|| inbox.try_recv().ok()));
            let (mut jobs, mut detail) = (false, false);
            for message in batch.collect::<Vec<_>>() {
                match message {
                    Message::Connected => {
                        self.set_link(Link::Live);
                        (jobs, detail) = (true, true);
                    }
                    Message::Changed => (jobs, detail) = (true, true),
                    Message::NoDaemon => {
                        self.set_link(Link::NoDaemon);
                        if std::mem::take(&mut self.auto_start) {
                            self.start_daemon();
                        }
                    }
                    Message::Lost(reason) => self.set_link(match reason {
                        Some(reason) => Link::Lost(reason),
                        None => Link::Connecting,
                    }),
                    Message::Command(Command::Select(job)) => {
                        self.selected = job;
                        self.log_choice = None;
                        detail = true;
                    }
                    Message::Command(Command::ShowLog(choice)) => {
                        self.log_choice = choice;
                        detail = true;
                    }
                    Message::Command(Command::StartDaemon) => self.start_daemon(),
                    Message::Command(Command::Act(job, verb)) => {
                        let outcome = act(&self.paths, job, verb);
                        if !self.send(Update::Acted(outcome)) {
                            return;
                        }
                    }
                }
            }
            if self.link != Link::Live {
                continue;
            }
            if jobs {
                let fetched = self.fetch_jobs();
                if let Ok(list) = &fetched {
                    self.jobs = list.clone();
                }
                if !self.send(Update::Jobs(fetched)) {
                    return;
                }
            }
            if detail {
                let fetched = self.fetch_detail();
                if !self.send(Update::Detail(fetched)) {
                    return;
                }
            }
        }
    }

    fn send(&self, update: Update) -> bool {
        let sent = self.updates.send(update).is_ok();
        (self.repaint)();
        sent
    }

    fn set_link(&mut self, link: Link) {
        if self.link != link {
            self.link = link.clone();
            self.send(Update::Link(link));
        }
    }

    fn start_daemon(&mut self) {
        let Some(cued) = self.cued.clone() else {
            self.set_link(Link::Lost(
                "no cued binary found to start the daemon (set CUED_EXECUTABLE)".into(),
            ));
            return;
        };
        self.set_link(Link::Connecting);
        if let Err(error) = client::start_daemon(&self.paths, &cued) {
            self.set_link(Link::Lost(format!("{error:#}")));
        }
        let _ = self.wake.send(());
    }

    fn fetch_jobs(&self) -> Result<Vec<JobEntry>, String> {
        match client::call(&self.paths, RequestBody::List { all: false }) {
            Ok(Response::JobList { jobs }) => Ok(jobs),
            Ok(other) => Err(client::unexpected(other).to_string()),
            Err(error) => Err(error.to_string()),
        }
    }

    fn fetch_detail(&mut self) -> Option<Result<Detail, String>> {
        self.following = None;
        let job = self.selected?;
        let Some(run) = self
            .jobs
            .iter()
            .find(|entry| entry.id == job)
            .map(|entry| entry.last_run.as_ref().map(|run| run.id))
        else {
            return Some(Err(format!("{job} is no longer listed")));
        };
        let Some(run) = run else {
            return Some(Ok(Detail {
                job,
                run: None,
                attempts: Vec::new(),
                log: None,
                graph: self.graph(job),
            }));
        };
        let body = RequestBody::Logs {
            job: job.to_string(),
            run: Some(run.0),
            step: None,
            attempt: None,
        };
        let attempts = match client::call(&self.paths, body) {
            Ok(Response::LogManifest { attempts, .. }) => attempts,
            Ok(other) => return Some(Err(client::unexpected(other).to_string())),
            Err(error) => return Some(Err(error.to_string())),
        };
        let shown = self
            .log_choice
            .as_ref()
            .and_then(|(step, attempt)| {
                attempts
                    .iter()
                    .find(|a| &a.step == step && a.attempt == *attempt)
            })
            .or_else(|| crate::model::default_attempt(&attempts));
        let log = shown.map(|attempt| {
            if attempt.running {
                self.following = Some((job, run, attempt.step.clone(), attempt.attempt));
            }
            self.read_log(job, run, &attempt.step, attempt.attempt)
        });
        Some(Ok(Detail {
            job,
            run: Some(run),
            attempts,
            log,
            graph: self.graph(job),
        }))
    }

    fn graph(&mut self, job: JobId) -> Option<Graph> {
        if let Some(graph) = self.graphs.get(&job) {
            return Some(graph.clone());
        }
        match client::call(
            &self.paths,
            RequestBody::Show {
                job: job.to_string(),
            },
        ) {
            Ok(Response::JobDetail { job: detail, .. }) => {
                self.graphs.insert(job, detail.graph.clone());
                Some(detail.graph)
            }
            _ => None,
        }
    }

    /// Re-read the followed log; `false` once the UI is gone.
    fn follow(&mut self) -> bool {
        let Some((job, run, step, attempt)) = self.following.clone() else {
            return true;
        };
        let log = self.read_log(job, run, &step, attempt);
        self.send(Update::Log { job, run, log })
    }

    fn read_log(&self, job: JobId, run: RunId, step: &str, attempt: u32) -> Log {
        let path = self.paths.step_log(job, run, step, attempt);
        let (text, truncated) = match tail(&path, LOG_TAIL) {
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (String::new(), false),
            Err(error) => (format!("couldn't read {}: {error}", path.display()), false),
        };
        Log {
            step: step.to_owned(),
            attempt,
            text,
            truncated,
        }
    }
}

/// One control request, as the CLI would send it.
fn act(paths: &Paths, job: JobId, verb: Verb) -> Result<String, String> {
    let reference = job.to_string();
    let body = match verb {
        Verb::Continue => RequestBody::Continue { job: reference },
        Verb::Retry => RequestBody::Retry {
            job: reference,
            from: None,
        },
        Verb::Pause => RequestBody::Pause { job: reference },
        Verb::Resume => RequestBody::Resume { job: reference },
        Verb::Cancel => RequestBody::Cancel { job: reference },
    };
    match client::call(paths, body).map_err(|error| error.to_string())? {
        Response::Rearmed { job, run, step } if verb == Verb::Continue => {
            Ok(format!("{job}.{run} continuing from {step}"))
        }
        Response::Rearmed { job, run, step } => Ok(format!("{job}.{run} rerunning from {step}")),
        Response::Paused { job } => Ok(format!("{job} paused")),
        Response::Resumed { job, .. } => Ok(format!("{job} resumed")),
        Response::JobCancelled { job, .. } => Ok(format!("{job} cancelled")),
        other => Err(client::unexpected(other).to_string()),
    }
}

/// The last `limit` bytes of a file, and whether there was more before.
fn tail(path: &std::path::Path, limit: u64) -> std::io::Result<(String, bool)> {
    let mut file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    let start = size.saturating_sub(limit);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(limit).read_to_end(&mut bytes)?;
    Ok((String::from_utf8_lossy(&bytes).into_owned(), start > 0))
}
