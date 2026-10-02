//! A run drawn as its workflow: steps as boxes in columns, left to right,
//! with the transitions between them as arrows. The path the run took is
//! drawn bold; branches it didn't take stay faint. The layout is computed
//! here, away from drawing, so it is tested directly.
use crate::model::{self, Mark, PlanRow};
use crate::theme::{self, Palette, icon};
use cued::model::{Effect, Graph};
use cued::proto::LogAttempt;
use eframe::egui::{
    self, Color32, CornerRadius, Pos2, Rect, Stroke, Vec2, epaint::CubicBezierShape,
};
use jiff::Timestamp;
use std::collections::{BTreeMap, VecDeque};

const NODE: Vec2 = Vec2::new(150.0, 48.0);
/// Between columns: room for an arrow, its label, and loop arrows' turns.
const COLUMN_GAP: f32 = 72.0;
const ROW_GAP: f32 = 22.0;
const MARGIN: f32 = 12.0;
/// Below the boxes: the first loop lane, then one more per further loop.
const LANE: f32 = 18.0;
const LANE_STEP: f32 = 12.0;
/// The smallest the drawing shrinks to fit the pane; beyond that it scrolls.
const MIN_SCALE: f32 = 0.7;

/// One arrow: every goto transition from one step to another, merged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    pub from: usize,
    pub to: usize,
    /// The conditions, in evaluation order: "failed", "exit == 3".
    pub label: String,
    /// Indexes into `from`'s transitions.
    pub transitions: Vec<u32>,
    /// It leads back to a step the run passes on its way here: a loop.
    pub back: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// Step ids, in the order a walk from the entry first meets them.
    pub steps: Vec<String>,
    /// Each step's column.
    pub column: Vec<usize>,
    /// Each step's row within its column.
    pub row: Vec<usize>,
    pub edges: Vec<Edge>,
}

impl Layout {
    pub fn columns(&self) -> usize {
        self.column.iter().max().map_or(0, |c| c + 1)
    }

    pub fn rows(&self) -> usize {
        self.row.iter().max().map_or(0, |r| r + 1)
    }

    pub fn index(&self, step: &str) -> Option<usize> {
        self.steps.iter().position(|s| s == step)
    }
}

/// Every goto transition of a step, by target, in evaluation order.
fn targets(graph: &Graph, step: &str) -> Vec<(String, u32)> {
    graph.steps.get(step).map_or_else(Vec::new, |node| {
        node.transitions
            .iter()
            .enumerate()
            .filter_map(|(index, transition)| match &transition.then {
                Effect::Goto { step, .. } if graph.steps.contains_key(step) => {
                    Some((step.clone(), index as u32))
                }
                _ => None,
            })
            .collect()
    })
}

pub fn layout(graph: &Graph) -> Layout {
    // Discovery order: a depth-first walk from the entry, edges in the order
    // they are tried, then any step the entry can't reach. An edge to a step
    // still on the walk's path leads back: a loop.
    let mut steps: Vec<String> = Vec::new();
    let mut back: Vec<(String, String)> = Vec::new();
    let mut roots = vec![graph.entry.clone()];
    roots.extend(graph.steps.keys().cloned());
    for root in roots {
        if steps.contains(&root) || !graph.steps.contains_key(&root) {
            continue;
        }
        steps.push(root.clone());
        let mut path: Vec<(String, usize)> = vec![(root, 0)];
        while let Some((step, next)) = path.last().cloned() {
            let outgoing = targets(graph, &step);
            let Some((target, _)) = outgoing.get(next) else {
                path.pop();
                continue;
            };
            path.last_mut().expect("non-empty").1 += 1;
            if path.iter().any(|(on_path, _)| on_path == target) {
                back.push((step.clone(), target.clone()));
            } else if !steps.contains(target) {
                steps.push(target.clone());
                path.push((target.clone(), 0));
            }
        }
    }
    let index = |step: &str| steps.iter().position(|s| s == step).expect("discovered");

    let mut edges: Vec<Edge> = Vec::new();
    for (from, step) in steps.iter().enumerate() {
        let transitions = &graph.steps[step].transitions;
        for (target, transition) in targets(graph, step) {
            let to = index(&target);
            let when = cued::client::describe_condition(&transitions[transition as usize].when);
            match edges.iter_mut().find(|e| e.from == from && e.to == to) {
                Some(edge) => {
                    edge.label.push_str(&format!(" · {when}"));
                    edge.transitions.push(transition);
                }
                None => edges.push(Edge {
                    from,
                    to,
                    label: when,
                    transitions: vec![transition],
                    back: back.contains(&(step.clone(), target)),
                }),
            }
        }
    }

    // Columns: the longest forward path to each step, so every step sits
    // right of all that can lead to it.
    let n = steps.len();
    let mut column = vec![0usize; n];
    let mut incoming = vec![0usize; n];
    for edge in edges.iter().filter(|e| !e.back) {
        incoming[edge.to] += 1;
    }
    let mut ready: VecDeque<usize> = (0..n).filter(|&s| incoming[s] == 0).collect();
    while let Some(step) = ready.pop_front() {
        for edge in edges.iter().filter(|e| !e.back && e.from == step) {
            column[edge.to] = column[edge.to].max(column[step] + 1);
            incoming[edge.to] -= 1;
            if incoming[edge.to] == 0 {
                ready.push_back(edge.to);
            }
        }
    }

    // Rows: discovery order, then one pass pulling each step toward the
    // average row of what leads to it, which uncrosses most arrows.
    let mut row = vec![0usize; n];
    let mut by_column: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (step, &at) in column.iter().enumerate() {
        by_column.entry(at).or_default().push(step);
    }
    for members in by_column.values_mut() {
        let pull = |step: usize| {
            let rows: Vec<f32> = edges
                .iter()
                .filter(|e| !e.back && e.to == step)
                .map(|e| row[e.from] as f32)
                .collect();
            if rows.is_empty() {
                f32::MAX
            } else {
                rows.iter().sum::<f32>() / rows.len() as f32
            }
        };
        // A step whose only way on is back (a retry's detour) is a side
        // trip: below the main line, whichever order its edges are tried in.
        let detour = |step: usize| {
            let mut out = edges.iter().filter(|e| e.from == step).peekable();
            out.peek().is_some() && out.all(|e| e.back)
        };
        let mut keyed: Vec<(f32, bool, usize)> =
            members.iter().map(|&s| (pull(s), detour(s), s)).collect();
        keyed.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
        for (slot, (_, _, step)) in keyed.into_iter().enumerate() {
            row[step] = slot;
        }
    }

    Layout {
        steps,
        column,
        row,
        edges,
    }
}

/// The edges the run took: each closed attempt's transition, when it was a
/// goto drawn here.
pub fn taken(layout: &Layout, attempts: &[LogAttempt]) -> Vec<usize> {
    let mut taken = Vec::new();
    for attempt in attempts {
        let (Some(from), Some(edge)) = (layout.index(&attempt.step), attempt.outcome_edge) else {
            continue;
        };
        if let Some(index) = layout
            .edges
            .iter()
            .position(|e| e.from == from && e.transitions.contains(&edge))
            && !taken.contains(&index)
        {
            taken.push(index);
        }
    }
    taken
}

/// What a box says about its step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Its latest attempt, and how many times the run has visited it.
    Ran {
        mark: Mark,
        detail: String,
        visits: usize,
        running: bool,
        latest: (String, u32),
    },
    Next(String),
    Pending,
    Unreached,
}

/// Each step's state, from the run's plan.
pub fn states(rows: &[PlanRow], now: Timestamp) -> BTreeMap<String, State> {
    let mut states = BTreeMap::new();
    for row in rows {
        let (step, state) = match row {
            PlanRow::Ran { attempt, .. } => {
                let visits = match states.get(&attempt.step) {
                    Some(State::Ran { visits, .. }) => visits + 1,
                    _ => 1,
                };
                let mark = model::attempt_mark(attempt);
                let took = model::elapsed(attempt.started_at, attempt.ended_at, now);
                (
                    attempt.step.clone(),
                    State::Ran {
                        detail: format!("{} · {took}", mark.label),
                        mark,
                        visits,
                        running: attempt.running,
                        latest: (attempt.step.clone(), attempt.attempt),
                    },
                )
            }
            PlanRow::Next { step, at } => (
                step.to_string(),
                State::Next(match at {
                    Some(at) => format!("next, {}", model::relative(*at, now)),
                    None => "next".into(),
                }),
            ),
            PlanRow::Pending { step } => (step.to_string(), State::Pending),
            PlanRow::Unreached { step } => (step.to_string(), State::Unreached),
        };
        states.insert(step, state);
    }
    states
}

/// Where everything goes, at a scale.
struct Geometry {
    origin: Pos2,
    scale: f32,
    /// The bottom of the lowest row of boxes.
    grid_bottom: f32,
}

impl Geometry {
    fn natural(layout: &Layout) -> Vec2 {
        let loops = layout.edges.iter().filter(|e| e.back).count();
        let lanes = if loops == 0 {
            0.0
        } else {
            LANE + (loops - 1) as f32 * LANE_STEP + 14.0
        };
        Vec2::new(
            2.0 * MARGIN + layout.columns() as f32 * (NODE.x + COLUMN_GAP) - COLUMN_GAP,
            2.0 * MARGIN + layout.rows() as f32 * (NODE.y + ROW_GAP) - ROW_GAP + lanes,
        )
    }

    fn node(&self, layout: &Layout, step: usize) -> Rect {
        let x = MARGIN + layout.column[step] as f32 * (NODE.x + COLUMN_GAP);
        let y = MARGIN + layout.row[step] as f32 * (NODE.y + ROW_GAP);
        Rect::from_min_size(
            self.origin + Vec2::new(x, y) * self.scale,
            NODE * self.scale,
        )
    }

    fn px(&self, length: f32) -> f32 {
        length * self.scale
    }
}

fn arrowhead(painter: &egui::Painter, tip: Pos2, from: Pos2, color: Color32, scale: f32) {
    let direction = (tip - from).normalized();
    let side = direction.rot90() * 4.5 * scale;
    let base = tip - direction * 9.0 * scale;
    painter.add(egui::Shape::convex_polygon(
        vec![tip, base + side, base - side],
        color,
        Stroke::NONE,
    ));
}

/// A polyline with its corners rounded off.
fn rounded(painter: &egui::Painter, points: &[Pos2], radius: f32, stroke: Stroke) {
    let mut path = vec![points[0]];
    for window in points.windows(3) {
        let (before, corner, after) = (window[0], window[1], window[2]);
        let r = radius
            .min((corner - before).length() / 2.0)
            .min((after - corner).length() / 2.0);
        let enter = corner - (corner - before).normalized() * r;
        let leave = corner + (after - corner).normalized() * r;
        path.push(enter);
        for i in 1..=6 {
            let t = i as f32 / 6.0;
            let h = 1.0 - t;
            path.push(Pos2::new(
                h * h * enter.x + 2.0 * h * t * corner.x + t * t * leave.x,
                h * h * enter.y + 2.0 * h * t * corner.y + t * t * leave.y,
            ));
        }
    }
    path.push(*points.last().expect("a path has points"));
    painter.add(egui::Shape::line(path, stroke));
}

/// Draw the workflow, shrunk to fit the width available (down to
/// [`MIN_SCALE`]). Returns the attempt whose output was asked for, if a step
/// that ran was clicked.
pub fn show(
    ui: &mut egui::Ui,
    layout: &Layout,
    states: &BTreeMap<String, State>,
    taken: &[usize],
    shown: Option<&(String, u32)>,
) -> Option<(String, u32)> {
    let p = Palette::of(ui.visuals());
    let natural = Geometry::natural(layout);
    let scale = (ui.available_width() / natural.x).clamp(MIN_SCALE, 1.0);
    let (canvas, _) = ui.allocate_exact_size(natural * scale, egui::Sense::hover());
    let painter = ui.painter_at(canvas.expand(2.0));
    let rows_height = layout.rows() as f32 * (NODE.y + ROW_GAP) - ROW_GAP;
    let g = Geometry {
        origin: canvas.min,
        scale,
        grid_bottom: canvas.min.y + (MARGIN + rows_height) * scale,
    };
    let label = |text: &str, color: Color32| {
        egui::WidgetText::from(egui::RichText::new(text).size(11.0 * scale).color(color))
            .into_galley(
                ui,
                Some(egui::TextWrapMode::Truncate),
                g.px(COLUMN_GAP + NODE.x / 2.0),
                egui::TextStyle::Small,
            )
    };

    // Untaken edges first, so the path is drawn over them.
    let mut order: Vec<usize> = (0..layout.edges.len()).collect();
    order.sort_by_key(|i| taken.contains(i));
    let mut lane = 0;
    let lanes: Vec<Option<usize>> = layout
        .edges
        .iter()
        .map(|edge| {
            edge.back.then(|| {
                lane += 1;
                lane - 1
            })
        })
        .collect();
    for index in order {
        let edge = &layout.edges[index];
        let on_path = taken.contains(&index);
        let color = if on_path { p.accent } else { p.faint };
        let stroke = Stroke::new(if on_path { 2.0 } else { 1.2 }, color);
        let from = g.node(layout, edge.from);
        let to = g.node(layout, edge.to);
        let (tip, tail, middle) = if let Some(lane) = lanes[index] {
            // A loop: out into the gap right of its step, along a lane under
            // every box, and up the gap left of the step it repeats, so it
            // crosses arrows but never a box.
            let y = g.grid_bottom + g.px(LANE + lane as f32 * LANE_STEP);
            let offset = Vec2::new(0.0, g.px(10.0));
            let start = from.right_center() + offset;
            let end = to.left_center() + offset;
            let out = start.x + g.px(COLUMN_GAP * 0.3 + lane as f32 * 6.0);
            let back = end.x - g.px(COLUMN_GAP * 0.3 + lane as f32 * 6.0);
            let points = [
                start,
                Pos2::new(out, start.y),
                Pos2::new(out, y),
                Pos2::new(back, y),
                Pos2::new(back, end.y),
                end,
            ];
            rounded(&painter, &points, g.px(8.0), stroke);
            (end, points[4], Pos2::new((out + back) / 2.0, y))
        } else {
            let start = from.right_center();
            let end = to.left_center();
            let bend = (end.x - start.x) / 2.0;
            let points = [
                start,
                start + Vec2::new(bend, 0.0),
                end - Vec2::new(bend, 0.0),
                end,
            ];
            let curve =
                CubicBezierShape::from_points_stroke(points, false, Color32::TRANSPARENT, stroke);
            let middle = curve.sample(0.5);
            painter.add(curve);
            (end, points[2], middle)
        };
        arrowhead(&painter, tip, tail, color, scale);
        let galley = label(&edge.label, if on_path { p.accent_text } else { p.faint });
        let at = middle - galley.size() / 2.0;
        painter.rect_filled(
            Rect::from_min_size(at, galley.size()).expand2(Vec2::new(3.0, 1.0)),
            CornerRadius::same(3),
            p.canvas,
        );
        painter.galley(at, galley, color);
    }

    let mut clicked = None;
    for (step_index, step) in layout.steps.iter().enumerate() {
        let rect = g.node(layout, step_index);
        let state = states.get(step).cloned().unwrap_or(State::Unreached);
        let response = ui.interact(rect, ui.id().with(("flow", step)), egui::Sense::click());
        let selected = matches!(&state, State::Ran { latest, .. } if Some(latest) == shown);
        let (glyph, glyph_color, status, text_color, border) = match &state {
            State::Ran {
                mark,
                detail,
                visits,
                running,
                ..
            } => {
                let status = if *visits > 1 {
                    format!("{detail} · ×{visits}")
                } else {
                    detail.clone()
                };
                let border = if *running {
                    Stroke::new(1.5, p.accent)
                } else {
                    Stroke::new(1.0, p.hairline)
                };
                let color = match mark.tone {
                    model::Tone::Accent => p.accent,
                    model::Tone::Success => p.success,
                    model::Tone::Danger => p.danger,
                    model::Tone::Warning => p.warning,
                    model::Tone::Muted => p.faint,
                };
                (mark.icon, color, status, p.text, border)
            }
            State::Next(when) => (
                icon::CLOCK,
                p.accent,
                when.clone(),
                p.text,
                Stroke::new(1.5, p.accent),
            ),
            State::Pending => (
                icon::CIRCLE_DASHED,
                p.muted,
                "pending".into(),
                p.muted,
                Stroke::new(1.0, p.hairline),
            ),
            State::Unreached => (
                icon::MINUS_CIRCLE,
                p.faint,
                "not reached".into(),
                p.faint,
                Stroke::new(1.0, p.hairline),
            ),
        };
        let fill = if selected {
            p.accent_soft
        } else if response.hovered() && matches!(state, State::Ran { .. }) {
            p.hover
        } else if matches!(state, State::Unreached) {
            p.canvas
        } else {
            p.surface
        };
        painter.rect(
            rect,
            CornerRadius::same(theme::RADIUS_MD),
            fill,
            border,
            egui::StrokeKind::Inside,
        );
        let inner = rect.shrink2(Vec2::new(g.px(10.0), g.px(7.0)));
        painter.text(
            inner.left_center(),
            egui::Align2::LEFT_CENTER,
            glyph,
            egui::FontId::new(16.0 * scale, theme::icons()),
            glyph_color,
        );
        let text_left = inner.left() + g.px(24.0);
        let width = inner.right() - text_left;
        for (line, (text, size, color, family)) in [
            (step.as_str(), 13.0, text_color, theme::medium()),
            (
                status.as_str(),
                11.0,
                p.faint,
                egui::FontFamily::Proportional,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let galley = egui::WidgetText::from(
                egui::RichText::new(text)
                    .size(size * scale)
                    .family(family)
                    .color(color),
            )
            .into_galley(
                ui,
                Some(egui::TextWrapMode::Truncate),
                width,
                egui::TextStyle::Body,
            );
            let y = if line == 0 {
                inner.top()
            } else {
                inner.bottom() - galley.size().y
            };
            painter.galley(Pos2::new(text_left, y), galley, color);
        }
        theme::name(&response, &format!("{step} box, {status}"));
        if let State::Ran { latest, .. } = &state {
            if response.clicked() {
                clicked = Some(latest.clone());
            }
            if response.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
        }
    }
    clicked
}

#[cfg(test)]
mod tests {
    use super::*;
    use cued::model::{Action, Condition, Outcome, Step, Transition};

    fn graph(entry: &str, steps: &[(&str, Vec<(Condition, Effect)>)]) -> Graph {
        Graph {
            entry: entry.into(),
            steps: steps
                .iter()
                .map(|(id, edges)| {
                    (
                        id.to_string(),
                        Step {
                            action: Action::Shell {
                                argv: vec!["true".into()],
                            },
                            cwd: None,
                            env: None,
                            timeout: None,
                            kill_grace: None,
                            transitions: edges
                                .iter()
                                .cloned()
                                .map(|(when, then)| Transition { when, then })
                                .collect(),
                            max_visits: None,
                            restart_safe: false,
                            missed_wait: None,
                        },
                    )
                })
                .collect(),
        }
    }

    fn goto(step: &str) -> Effect {
        Effect::Goto {
            step: step.into(),
            after: None,
        }
    }

    fn ship_it() -> Graph {
        use Condition::{Always, Failed, Succeeded};
        graph(
            "build",
            &[
                ("build", vec![(Succeeded, goto("test"))]),
                (
                    "test",
                    vec![(Succeeded, goto("deploy")), (Failed, goto("clear-cache"))],
                ),
                ("clear-cache", vec![(Succeeded, goto("test"))]),
                (
                    "deploy",
                    vec![(Succeeded, goto("smoke-test")), (Failed, goto("rollback"))],
                ),
                ("smoke-test", vec![]),
                (
                    "rollback",
                    vec![(
                        Always,
                        Effect::End {
                            outcome: Outcome::Failure,
                        },
                    )],
                ),
            ],
        )
    }

    /// Each step as "column.row".
    fn places(layout: &Layout) -> BTreeMap<&str, String> {
        layout
            .steps
            .iter()
            .enumerate()
            .map(|(i, step)| {
                (
                    step.as_str(),
                    format!("{}.{}", layout.column[i], layout.row[i]),
                )
            })
            .collect()
    }

    #[test]
    fn steps_sit_right_of_what_leads_to_them_and_loops_lead_back() {
        let layout = layout(&ship_it());
        assert_eq!(
            places(&layout),
            BTreeMap::from([
                ("build", "0.0".into()),
                ("test", "1.0".into()),
                ("deploy", "2.0".into()),
                ("clear-cache", "2.1".into()),
                ("smoke-test", "3.0".into()),
                ("rollback", "3.1".into()),
            ])
        );
        let back: Vec<(&str, &str)> = layout
            .edges
            .iter()
            .filter(|e| e.back)
            .map(|e| (layout.steps[e.from].as_str(), layout.steps[e.to].as_str()))
            .collect();
        assert_eq!(back, [("clear-cache", "test")]);
        // End effects draw no arrow: six gotos, six edges.
        assert_eq!(layout.edges.len(), 6);
    }

    #[test]
    fn a_retry_detour_sits_below_the_main_line_whatever_the_edge_order() {
        use Condition::{Failed, Succeeded};
        // Failure tried first, as the file's `on.fail` sugar orders it.
        let layout = layout(&graph(
            "test",
            &[
                (
                    "test",
                    vec![(Failed, goto("clear-cache")), (Succeeded, goto("deploy"))],
                ),
                ("clear-cache", vec![(Succeeded, goto("test"))]),
                ("deploy", vec![]),
            ],
        ));
        let places = places(&layout);
        assert_eq!(
            (places["deploy"].as_str(), places["clear-cache"].as_str()),
            ("1.0", "1.1")
        );
    }

    #[test]
    fn transitions_to_the_same_step_share_one_arrow() {
        use Condition::{ExitEq, Failed};
        let layout = layout(&graph(
            "probe",
            &[
                (
                    "probe",
                    vec![(ExitEq(3), goto("page")), (Failed, goto("page"))],
                ),
                ("page", vec![]),
            ],
        ));
        assert_eq!(layout.edges.len(), 1);
        assert_eq!(layout.edges[0].label, "exit == 3 · failed");
        assert_eq!(layout.edges[0].transitions, [0, 1]);
    }

    #[test]
    fn a_step_looping_to_itself_and_unreachable_steps_still_place() {
        use Condition::{Failed, Succeeded};
        let layout = layout(&graph(
            "poll",
            &[
                (
                    "poll",
                    vec![(Failed, goto("poll")), (Succeeded, goto("done"))],
                ),
                ("done", vec![]),
                ("orphan", vec![]),
            ],
        ));
        let self_loop = layout.edges.iter().find(|e| e.from == e.to).unwrap();
        assert!(self_loop.back);
        assert_eq!(places(&layout)["done"], "1.0");
        assert_eq!(layout.steps.last().map(String::as_str), Some("orphan"));
    }

    #[test]
    fn the_path_taken_is_each_closed_attempt_s_transition() {
        let layout = layout(&ship_it());
        let attempt = |step: &str, edge: Option<u32>| LogAttempt {
            step: step.into(),
            attempt: 1,
            started_at: Timestamp::UNIX_EPOCH,
            ended_at: Some(Timestamp::UNIX_EPOCH),
            running: false,
            exit_code: Some(0),
            timed_out: false,
            outcome_edge: edge,
        };
        let attempts = [
            attempt("build", Some(0)),
            attempt("test", Some(1)),
            attempt("clear-cache", Some(0)),
            attempt("test", Some(0)),
            attempt("deploy", Some(1)),
            // rollback's transition is an end: no arrow.
            attempt("rollback", Some(0)),
        ];
        let named: Vec<String> = taken(&layout, &attempts)
            .into_iter()
            .map(|i| {
                let e = &layout.edges[i];
                format!("{}→{}", layout.steps[e.from], layout.steps[e.to])
            })
            .collect();
        assert_eq!(
            named,
            [
                "build→test",
                "test→clear-cache",
                "clear-cache→test",
                "test→deploy",
                "deploy→rollback"
            ]
        );
    }
}
