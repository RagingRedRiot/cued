//! The window: jobs grouped by where they stand on the left, the selected
//! job's latest run step by step on the right, with the log of one attempt.
//! Everything shown comes from the backend; this only draws and forwards
//! clicks. It repaints when the backend has something new, and once a second
//! only while a run on screen is counting up.
use crate::backend::{Backend, Command, Detail, Link, Update};
use crate::model::{self, Mark, Section, Tone};
use crate::theme::{self, Palette, icon};
use cued::model::JobId;
use cued::proto::JobEntry;
use eframe::egui::{self, Color32, CornerRadius, RichText};
use jiff::Timestamp;
use std::time::Duration;

pub struct App {
    backend: Backend,
    link: Link,
    /// `None` until the first list arrives.
    jobs: Option<Vec<JobEntry>>,
    jobs_error: Option<String>,
    selected: Option<JobId>,
    detail: Option<Result<Detail, String>>,
    log_choice: Option<(String, u32)>,
}

impl App {
    pub fn new(backend: Backend) -> Self {
        Self {
            backend,
            link: Link::Connecting,
            jobs: None,
            jobs_error: None,
            selected: None,
            detail: None,
            log_choice: None,
        }
    }

    fn apply(&mut self, update: Update) {
        match update {
            Update::Link(link) => self.link = link,
            Update::Jobs(Ok(jobs)) => {
                self.jobs = Some(jobs);
                self.jobs_error = None;
            }
            // Keep showing the last list; say why it is not current.
            Update::Jobs(Err(error)) => self.jobs_error = Some(error),
            Update::Detail(detail) => {
                // A fetch for a job no longer selected is stale.
                let current = match &detail {
                    Some(Ok(detail)) => Some(detail.job) == self.selected,
                    _ => true,
                };
                if current {
                    self.detail = detail;
                }
            }
            Update::Log { job, run, log } => {
                if let Some(Ok(detail)) = &mut self.detail
                    && detail.job == job
                    && detail.run == Some(run)
                    && detail
                        .log
                        .as_ref()
                        .is_some_and(|shown| shown.step == log.step && shown.attempt == log.attempt)
                {
                    detail.log = Some(log);
                }
            }
        }
    }

    fn select(&mut self, job: JobId) {
        if self.selected != Some(job) {
            self.selected = Some(job);
            self.detail = None;
            self.log_choice = None;
            self.backend.send(Command::Select(Some(job)));
        }
    }

    fn show_log(&mut self, choice: (String, u32)) {
        if self.log_choice.as_ref() != Some(&choice) {
            self.log_choice = Some(choice.clone());
            self.backend.send(Command::ShowLog(Some(choice)));
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui) {
        theme::install(ui.ctx());
        if !theme::ready(ui.ctx()) {
            return;
        }
        while let Some(update) = self.backend.try_recv() {
            self.apply(update);
        }
        let now = Timestamp::now();
        let attempts = match &self.detail {
            Some(Ok(detail)) => detail.attempts.as_slice(),
            _ => &[],
        };
        if model::ticking(self.jobs.as_deref().unwrap_or_default(), attempts) {
            ui.ctx().request_repaint_after(Duration::from_secs(1));
        }

        let fill = ui.visuals().panel_fill;
        let panel = |x, y| {
            egui::Frame::new()
                .fill(fill)
                .inner_margin(egui::Margin::symmetric(x, y))
        };
        egui::Panel::top("bar")
            .frame(panel(12, 8))
            .show(ui, |ui| self.bar(ui));
        egui::Panel::left("jobs")
            .default_size(400.0)
            .frame(panel(10, 10))
            .show(ui, |ui| self.job_list(ui, now));
        egui::CentralPanel::default()
            .frame(
                egui::Frame::NONE
                    .fill(Palette::of(ui.visuals()).canvas)
                    .inner_margin(16),
            )
            .show(ui, |ui| self.detail(ui, now));
    }

    fn bar(&mut self, ui: &mut egui::Ui) {
        let p = Palette::of(ui.visuals());
        ui.horizontal(|ui| {
            ui.label(
                theme::glyph(icon::TERMINAL_WINDOW)
                    .size(18.0)
                    .color(p.accent),
            );
            ui.label(theme::strong("cued").size(15.0));
            ui.label(RichText::new("local runs").color(p.faint));
            ui.with_layout(
                egui::Layout::right_to_left(egui::Align::Center),
                |ui| match &self.link {
                    Link::Live => {
                        theme::pill(ui, "Live", p.accent_soft, p.accent_text);
                    }
                    Link::Connecting => {
                        ui.label(RichText::new("Connecting…").color(p.muted));
                    }
                    Link::NoDaemon => {
                        if ui.button("Start daemon").clicked() {
                            self.backend.send(Command::StartDaemon);
                        }
                        theme::pill(ui, "Daemon not running", p.warning_soft, p.warning);
                    }
                    Link::Lost(reason) => {
                        theme::pill(ui, "Disconnected, retrying", p.warning_soft, p.warning)
                            .on_hover_text(reason);
                    }
                },
            );
        });
    }

    fn job_list(&mut self, ui: &mut egui::Ui, now: Timestamp) {
        let p = Palette::of(ui.visuals());
        if let Some(error) = &self.jobs_error {
            ui.add(egui::Label::new(RichText::new(error).color(p.danger)).wrap());
        }
        let Some(jobs) = &self.jobs else {
            ui.label(RichText::new("Loading…").color(p.muted));
            return;
        };
        if jobs.is_empty() {
            ui.add_space(8.0);
            ui.label(RichText::new("Nothing scheduled.").color(p.muted));
            ui.label(
                RichText::new("cued at \"in 1m\" -- echo hello")
                    .monospace()
                    .color(p.faint),
            );
            return;
        }
        let mut clicked = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (section, jobs) in model::group(jobs) {
                    ui.add_space(4.0);
                    let title = format!("{} {}", section.title(), jobs.len());
                    ui.label(theme::eyebrow(ui, &title));
                    for job in jobs {
                        if job_row(ui, job, section, now, self.selected == Some(job.id)).clicked() {
                            clicked = Some(job.id);
                        }
                    }
                    ui.add_space(6.0);
                }
            });
        if let Some(job) = clicked {
            self.select(job);
        }
    }

    fn detail(&mut self, ui: &mut egui::Ui, now: Timestamp) {
        let p = Palette::of(ui.visuals());
        let Some(selected) = self.selected else {
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new("Select a job to see its steps.").color(p.muted));
            });
            return;
        };
        let job = self
            .jobs
            .as_ref()
            .and_then(|jobs| jobs.iter().find(|job| job.id == selected));
        let Some(job) = job else {
            ui.label(RichText::new(format!("{selected} is no longer listed.")).color(p.muted));
            return;
        };
        header(ui, job, now);
        ui.add_space(12.0);
        let detail = match &self.detail {
            None => {
                ui.label(RichText::new("Loading…").color(p.muted));
                return;
            }
            Some(Err(error)) => {
                ui.add(egui::Label::new(RichText::new(error).color(p.danger)).wrap());
                return;
            }
            Some(Ok(detail)) => detail,
        };
        if detail.run.is_none() {
            ui.label(RichText::new("No runs yet.").color(p.muted));
            return;
        }
        ui.label(theme::eyebrow(ui, "Steps"));
        let shown = detail
            .log
            .as_ref()
            .map(|log| (log.step.clone(), log.attempt));
        let mut picked = None;
        for attempt in &detail.attempts {
            let key = (attempt.step.clone(), attempt.attempt);
            let mark = model::attempt_mark(attempt);
            let took = model::elapsed(attempt.started_at, attempt.ended_at, now);
            let mut name = attempt.step.clone();
            if attempt.attempt > 1 {
                name.push_str(&format!(" · attempt {}", attempt.attempt));
            }
            let response = theme::list_row(ui, shown.as_ref() == Some(&key), |row| {
                row.label(mark_glyph(row, &mark));
                theme::row_text(row, theme::strong(name.clone()));
                row.with_layout(egui::Layout::right_to_left(egui::Align::Center), |row| {
                    row.label(RichText::new(&took).color(p.faint));
                    row.label(RichText::new(&mark.label).color(tone(p, mark.tone)));
                });
            });
            theme::name(&response, &format!("{name}, {}, {took}", mark.label));
            if response.clicked() {
                picked = Some(key);
            }
        }
        if detail.attempts.is_empty() {
            ui.label(RichText::new("No step has started yet.").color(p.muted));
        }
        if let Some(log) = &detail.log {
            ui.add_space(12.0);
            let mut title = format!("Output · {}", log.step);
            if log.attempt > 1 {
                title.push_str(&format!(" · attempt {}", log.attempt));
            }
            ui.label(theme::eyebrow(ui, &title));
            if log.truncated {
                ui.label(
                    RichText::new(format!(
                        "showing the last {} KiB; cued logs {} has it all",
                        crate::backend::LOG_TAIL / 1024,
                        job.id
                    ))
                    .size(11.5)
                    .color(p.faint),
                );
            }
            egui::Frame::new()
                .fill(p.surface)
                .stroke(egui::Stroke::new(1.0, p.hairline))
                .corner_radius(CornerRadius::same(theme::RADIUS_MD))
                .inner_margin(10)
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            if log.text.is_empty() {
                                ui.label(RichText::new("No output.").color(p.faint));
                            } else {
                                ui.add(
                                    egui::Label::new(RichText::new(&log.text).monospace())
                                        .wrap_mode(egui::TextWrapMode::Extend),
                                );
                            }
                        });
                });
        }
        if let Some(choice) = picked {
            self.show_log(choice);
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.show(ui);
    }
}

fn tone(p: &Palette, tone: Tone) -> Color32 {
    match tone {
        Tone::Accent => p.accent,
        Tone::Success => p.success,
        Tone::Danger => p.danger,
        Tone::Warning => p.warning,
        Tone::Muted => p.faint,
    }
}

fn mark_glyph(ui: &egui::Ui, mark: &Mark) -> RichText {
    theme::glyph(mark.icon)
        .size(16.0)
        .color(tone(Palette::of(ui.visuals()), mark.tone))
}

/// A two-line job row: its mark, title, and where it stands.
fn job_row(
    ui: &mut egui::Ui,
    job: &JobEntry,
    section: Section,
    now: Timestamp,
    selected: bool,
) -> egui::Response {
    let p = Palette::of(ui.visuals());
    let mark = model::job_mark(job);
    let title = model::job_title(job);
    let summary = model::job_summary(job, now);
    let response = theme::list_row_sized(ui, selected, 44.0, |row| {
        row.label(mark_glyph(row, &mark));
        row.vertical(|lines| {
            lines.spacing_mut().item_spacing.y = 1.0;
            lines.horizontal(|line| {
                theme::row_text(line, theme::strong(title.clone()));
                theme::row_text(line, RichText::new(&job.action).monospace().color(p.faint));
            });
            theme::row_text(lines, RichText::new(&summary).size(12.0).color(p.muted));
        });
    });
    theme::name(
        &response,
        &format!("{title}, {}, {}", mark.label, section.title()),
    );
    response
}

/// The selected job: mark, title, command, and its run's timing.
fn header(ui: &mut egui::Ui, job: &JobEntry, now: Timestamp) {
    let p = Palette::of(ui.visuals());
    let mark = model::job_mark(job);
    ui.horizontal(|ui| {
        ui.label(theme::glyph(mark.icon).size(22.0).color(tone(p, mark.tone)));
        ui.label(RichText::new(model::job_title(job)).heading());
        theme::pill(ui, &mark.label, p.raised, tone(p, mark.tone));
    });
    ui.label(RichText::new(&job.action).monospace().color(p.muted));
    let mut facts = Vec::new();
    if let Some(run) = &job.last_run {
        facts.push(format!("run {}", run.id));
        if let Some(start) = run.started_at {
            facts.push(format!("started {}", model::relative(start, now)));
            let verb = if run.ended_at.is_some() {
                "took"
            } else {
                "running for"
            };
            facts.push(format!(
                "{verb} {}",
                model::elapsed(start, run.ended_at, now)
            ));
        }
    }
    if let Some(next) = job.next_at {
        facts.push(format!("next {}", model::relative(next, now)));
    }
    if !facts.is_empty() {
        ui.label(RichText::new(facts.join(" · ")).color(p.faint));
    }
}
