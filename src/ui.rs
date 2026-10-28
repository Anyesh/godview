use std::time::Instant;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, HighlightSpacing, List, ListItem, ListState, Paragraph, Wrap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::anim::{FLASH, flow, pulse, sparkline, spinner};
use crate::children::Liveness;
use crate::herdr::Status;
use crate::live::StreamItem;
use crate::model::{AgentEntry, ChildEntry, ChildInfo, Model, NodeId, Row};
use crate::transcript::{Activity, RATE_BUCKETS};

const NAME_W: usize = 12;
const PROFILE_W: usize = 8;
const TYPE_W: usize = 15;
const TOOL_W: usize = 8;
const WHO_W: usize = 26;
/// Where an agent's title starts: indent, glyph, name, profile and context columns.
const TITLE_COL: usize = 2 + 2 + NAME_W + 1 + PROFILE_W + 1 + 4 + 2;
const SPARK_COLS: usize = RATE_BUCKETS + 2;
const SPLIT_MIN_WIDTH: u16 = 80;
const SPARK_MIN_WIDTH: u16 = 100;
const STREAM_ROWS: u16 = 7;
const STREAM_MIN_HEIGHT: u16 = 30;
const WIRE: Color = Color::Indexed(30);
const PULSE: Color = Color::Indexed(51);
const FLASH_BG: Color = Color::Indexed(23);
const SELECTED_BG: Color = Color::Indexed(236);
const SPARK: Color = Color::Indexed(73);

pub struct View<'a> {
    pub model: &'a Model,
    pub now: i64,
    /// The pane last read for peek, and its output.
    pub peek: Option<(&'a str, &'a str)>,
    pub status: Option<&'a str>,
    pub running_children: usize,
    pub frame: u64,
    pub instant: Instant,
    pub motion: bool,
    pub stream: &'a [StreamItem],
}

impl View<'_> {
    /// The frame to draw motion at, `None` when motion is off.
    fn motion_frame(&self) -> Option<u64> {
        self.motion.then_some(self.frame)
    }

    fn flashing(&self, since: Option<Instant>) -> bool {
        since.is_some_and(|t| self.instant < t + FLASH)
    }
}

type Rows<'a> = [(NodeId, Row<'a>)];

pub fn draw(frame: &mut Frame, view: &View, list: &mut ListState) {
    let area = frame.area();
    let stream_rows = if area.height >= STREAM_MIN_HEIGHT {
        STREAM_ROWS
    } else {
        0
    };
    let [header, body, stream, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(stream_rows),
        Constraint::Length(1),
    ])
    .areas(area);
    frame.render_widget(header_line(view, header.width as usize), header);
    let rows = view.model.rows();
    let selected = view.model.selected_index_in(&rows);
    let selection = selected.and_then(|i| rows.get(i)?.1.selection());
    if body.width >= SPLIT_MIN_WIDTH {
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(62), Constraint::Percentage(38)])
                .areas(body);
        draw_tree(frame, view, &rows, selected, left, list);
        draw_detail(frame, view, selection, right);
    } else {
        draw_tree(frame, view, &rows, selected, body, list);
    }
    if stream_rows > 0 {
        draw_stream(frame, view, stream);
    }
    frame.render_widget(footer_line(view, footer.width as usize), footer);
}

fn header_line(view: &View, width: usize) -> Line<'static> {
    let agents = view.model.groups().iter().flat_map(|g| &g.agents);
    let total = agents.clone().count();
    let working = agents
        .filter(|e| matches!(e.agent.status, Status::Working | Status::Blocked))
        .count();
    let left = vec![
        Span::styled(" godview", Style::new().add_modifier(Modifier::BOLD)),
        Span::raw(format!(
            "   {} · {working} working · {} running",
            count(total, "agent"),
            count(view.running_children, "subagent")
        )),
    ];
    let toggle = |on: bool, text: &'static str| {
        let style = if on {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::new().fg(Color::DarkGray)
        };
        Span::styled(text, style)
    };
    let sep = || Span::styled(" · ", Style::new().fg(Color::DarkGray));
    let right = vec![
        toggle(view.motion, "m motion"),
        sep(),
        toggle(view.model.working_only(), "w working only"),
        sep(),
        toggle(view.model.all_children, "a all"),
        Span::raw(" "),
    ];
    justify(left, right, width)
}

fn footer_line(view: &View, width: usize) -> Line<'static> {
    let keys = Span::styled(
        " j/k move · enter go to pane · p peek · space fold · m motion · q quit",
        Style::new().fg(Color::DarkGray),
    );
    let status = view
        .status
        .map(|s| Span::styled(format!("{s} "), Style::new().fg(Color::Red)));
    justify(vec![keys], status.into_iter().collect(), width)
}

fn justify(left: Vec<Span<'static>>, right: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let used: usize = left.iter().chain(&right).map(Span::width).sum();
    let mut spans = left;
    spans.push(Span::raw(" ".repeat(width.saturating_sub(used))));
    spans.extend(right);
    Line::from(spans)
}

fn draw_tree(
    frame: &mut Frame,
    view: &View,
    rows: &Rows,
    selected: Option<usize>,
    area: Rect,
    list: &mut ListState,
) {
    let block = Block::bordered().title(" agents ");
    if rows.is_empty() {
        let text = if view.status.is_some() {
            "waiting for herdr"
        } else {
            "no agents"
        };
        frame.render_widget(Paragraph::new(format!(" {text}")).block(block), area);
        return;
    }
    // The highlight symbol column takes two cells of every row.
    let width = (area.width as usize).saturating_sub(4);
    let spark = area.width >= SPARK_MIN_WIDTH;
    // The selection background goes under the text rather than over it, as List's highlight
    // style would, so a flashing line on the selected row keeps its own background.
    let items: Vec<ListItem> = rows
        .iter()
        .enumerate()
        .map(|(i, (_, row))| {
            let text = row_text(row, width, view, spark);
            ListItem::new(if Some(i) == selected {
                text.patch_style(Style::new().bg(SELECTED_BG))
            } else {
                text
            })
        })
        .collect();
    list.select(selected);
    let widget = List::new(items)
        .block(block)
        .highlight_symbol("▶ ")
        .highlight_spacing(HighlightSpacing::Always);
    frame.render_stateful_widget(widget, area, list);
}

fn row_text(row: &Row, width: usize, view: &View, spark: bool) -> Text<'static> {
    match row {
        Row::Group { group, collapsed } => {
            let arrow = if *collapsed { "▸" } else { "▾" };
            Text::from(Line::styled(
                format!("{arrow} {}", group.label),
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ))
        }
        Row::Agent { entry } => agent_text(entry, width, view, spark),
        Row::Child {
            entry,
            child,
            guides,
            last,
            ordinal,
            period,
        } => {
            let lit = ordinal.is_some() && entry.wire_running();
            let wire = Wire {
                lit,
                pulse: lit
                    && view
                        .motion_frame()
                        .zip(*ordinal)
                        .is_some_and(|(f, o)| pulse(f, o, *period)),
            };
            Text::from(child_line(child, guides, *last, wire, width, view))
        }
        Row::More { hidden, .. } => {
            let (text, mark) = if *hidden == 0 {
                ("  ▴ show fewer".to_string(), "[-]")
            } else {
                (format!("  ✳ {hidden} more done"), "[+]")
            };
            let dim = Style::new().fg(Color::DarkGray);
            Text::from(spread(
                width,
                Vec::new(),
                Span::styled(text, dim),
                Some(Span::styled(mark, dim)),
            ))
        }
    }
}

fn status_glyph(s: Status) -> Span<'static> {
    let (glyph, color) = match s {
        Status::Blocked => ("◉", Color::Red),
        Status::Working => ("◐", Color::Yellow),
        Status::Done => ("✳", Color::Green),
        Status::Idle => ("○", Color::DarkGray),
        Status::Unknown => ("·", Color::DarkGray),
    };
    Span::styled(glyph, Style::new().fg(color))
}

fn agent_text(e: &AgentEntry, width: usize, view: &View, spark: bool) -> Text<'static> {
    let a = &e.agent;
    let now = view.now;
    let busy = matches!(a.status, Status::Working | Status::Blocked);
    let dim = Style::new().fg(Color::DarkGray);
    let context = a
        .context
        .as_ref()
        .map_or(String::new(), |c| format!("{}%", c.percent));
    let glyph = match view.motion_frame() {
        Some(f) if e.active => Span::styled(spinner(f), Style::new().fg(Color::Yellow)),
        _ => status_glyph(a.status),
    };
    let mut fixed = vec![
        Span::raw("  "),
        glyph,
        Span::raw(" "),
        Span::styled(
            pad(&a.label, NAME_W),
            Style::new().add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(pad(e.profile.as_deref().unwrap_or("?"), PROFILE_W), dim),
        Span::raw(" "),
        Span::raw(format!("{context:>4}")),
        Span::raw("  "),
    ];
    let mut title_col = TITLE_COL;
    if spark {
        let style = if e.rate.iter().any(|&n| n > 0) {
            Style::new().fg(SPARK)
        } else {
            dim
        };
        fixed.push(Span::styled(sparkline(&e.rate), style));
        fixed.push(Span::raw("  "));
        title_col += SPARK_COLS;
    }
    let title = match &e.crew {
        Some(item) => format!("{} {} · {}", item.id, item.status, item.deliverable),
        None => a.title.clone(),
    };
    let age = if busy {
        None
    } else {
        e.active_at.map(|t| Span::styled(fmt_age(now - t), dim))
    };
    let mut lines = vec![spread(width, fixed, Span::raw(title), age)];
    if busy
        && let Some(activity) = &e.activity
        && let Some(tool) = activity.current()
    {
        // Only the current call's own result settles it; an earlier parallel call that is
        // still out does not mean this one runs.
        let running = e.active && activity.pending.contains(&tool.id);
        let marker = match view.motion_frame() {
            Some(f) if running => {
                Span::styled(format!("{} ", flow(f)), Style::new().fg(Color::Yellow))
            }
            _ => Span::raw("    "),
        };
        let indent = Span::raw(" ".repeat(title_col - 4));
        let name = Span::styled(format!("{}  ", tool.name), Style::new().fg(Color::Magenta));
        let age = tool.at.map(|t| Span::styled(fmt_age(now - t), dim));
        let mut line = spread(
            width,
            vec![indent, marker, name],
            Span::raw(tool.summary.clone()),
            age,
        );
        if view.flashing(e.tool_changed_at) {
            line = line.style(Style::new().bg(FLASH_BG));
        }
        lines.push(line);
    }
    Text::from(lines)
}

/// How a child row draws the agent's wire in its first column.
struct Wire {
    lit: bool,
    pulse: bool,
}

fn heavy(glyph: &'static str) -> &'static str {
    match glyph {
        "│" => "┃",
        "├" => "┣",
        "└" => "┗",
        other => other,
    }
}

fn child_line(
    c: &ChildEntry,
    guides: &[bool],
    last: bool,
    wire: Wire,
    width: usize,
    view: &View,
) -> Line<'static> {
    let now = view.now;
    let dim = Style::new().fg(Color::DarkGray);
    let mut cells: Vec<&'static str> = guides
        .iter()
        .map(|&more| if more { "│" } else { " " })
        .collect();
    cells.push(if last { "└" } else { "├" });
    let mut fixed = vec![Span::raw("    ")];
    for (depth, glyph) in cells.into_iter().enumerate() {
        let cell = match depth {
            0 if wire.pulse => Span::styled(
                heavy(glyph),
                Style::new().fg(PULSE).add_modifier(Modifier::BOLD),
            ),
            0 if wire.lit => Span::styled(glyph, Style::new().fg(WIRE)),
            _ => Span::styled(glyph, dim),
        };
        fixed.push(cell);
        fixed.push(Span::raw(" "));
    }
    let text_style = if c.liveness == Liveness::Running {
        Style::new()
    } else {
        dim
    };
    match &c.info {
        ChildInfo::Subagent {
            agent_type,
            description,
            activity,
        } => {
            let (glyph, color) = match c.liveness {
                Liveness::Running => ("●", Color::Green),
                Liveness::Done => ("✓", Color::DarkGray),
                Liveness::Stale => ("○", Color::DarkGray),
            };
            fixed.push(Span::styled(glyph, Style::new().fg(color)));
            fixed.push(Span::raw(" "));
            fixed.push(Span::styled(pad(agent_type, TYPE_W), text_style));
            fixed.push(Span::raw(" "));
            let current = activity.as_ref().and_then(Activity::current);
            let tool = current.map_or(String::new(), |t| t.name.clone());
            let at = current.and_then(|t| t.at).or(c.active_at);
            let age = at.map_or(String::new(), |t| fmt_age(now - t));
            let right = Span::styled(format!("{} {age:>4}", pad(&tool, TOOL_W)), dim);
            let line = spread(
                width,
                fixed,
                Span::styled(description.clone(), text_style),
                Some(right),
            );
            if view.flashing(c.tool_changed_at) {
                line.style(Style::new().bg(FLASH_BG))
            } else {
                line
            }
        }
        ChildInfo::Workflow {
            name,
            phase,
            running,
            ..
        } => {
            fixed.push(Span::styled("◆", Style::new().fg(Color::Blue)));
            fixed.push(Span::styled(format!(" workflow {name}   "), text_style));
            let phase = phase.as_ref().map_or(String::new(), |p| {
                format!("{} {}/{} · ", p.title, p.index, p.total)
            });
            let text = format!("{phase}{running} agents");
            let age = c.active_at.map(|t| Span::styled(fmt_age(now - t), dim));
            spread(width, fixed, Span::styled(text, text_style), age)
        }
    }
}

fn draw_stream(frame: &mut Frame, view: &View, area: Rect) {
    let width = area.width.saturating_sub(2) as usize;
    let height = area.height.saturating_sub(2) as usize;
    let lines: Vec<Line> = view
        .stream
        .iter()
        .take(height)
        .map(|item| stream_line(item, view, width))
        .collect();
    let block = Block::bordered().title(" live ");
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn stream_line(item: &StreamItem, view: &View, width: usize) -> Line<'static> {
    let dim = Style::new().fg(Color::DarkGray);
    let (text, tool) = if view.flashing(item.highlighted_at) {
        let bright = Style::new().fg(Color::White).add_modifier(Modifier::BOLD);
        (bright, bright.fg(Color::LightMagenta))
    } else {
        (Style::new(), Style::new().fg(Color::Magenta))
    };
    let who = match &item.child {
        Some(child) => format!("{} › {child}", item.who),
        None => item.who.clone(),
    };
    let fixed = vec![
        Span::styled(format!(" {:>4}  ", fmt_age(view.now - item.at)), dim),
        Span::styled(pad(&who, WHO_W), text),
        Span::raw("  "),
        Span::styled(pad(&item.tool, TOOL_W - 2), tool),
        Span::raw(" "),
    ];
    spread(width, fixed, Span::styled(item.summary.clone(), text), None)
}

fn draw_detail(
    frame: &mut Frame,
    view: &View,
    selection: Option<(&AgentEntry, Option<&ChildEntry>)>,
    area: Rect,
) {
    let title = selection.map_or(" details ".to_string(), |(e, _)| {
        format!(
            " {} · {} · {} ",
            e.agent.label,
            e.profile.as_deref().unwrap_or("?"),
            e.group_label
        )
    });
    let block = Block::bordered().title(title);
    if view.model.peek {
        let height = area.height.saturating_sub(2) as usize;
        let pane = selection.map(|(e, _)| e.agent.pane_id.as_str());
        let text = view.peek.filter(|(p, _)| Some(*p) == pane).map(|(_, t)| t);
        let lines: Vec<&str> = text.unwrap_or("").lines().collect();
        let shown = lines[lines.len().saturating_sub(height)..].join("\n");
        frame.render_widget(Paragraph::new(shown).block(block), area);
        return;
    }
    let width = area.width.saturating_sub(2) as usize;
    let lines = match selection {
        Some((entry, None)) => agent_detail(entry, view.now, width),
        Some((_, Some(child))) => child_detail(child, view.now, width),
        None => Vec::new(),
    };
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn bold(text: String) -> Line<'static> {
    Line::styled(text, Style::new().add_modifier(Modifier::BOLD))
}

fn agent_detail(e: &AgentEntry, now: i64, width: usize) -> Vec<Line<'static>> {
    let a = &e.agent;
    let mut lines = vec![bold(a.title.clone())];
    let mut usage = Vec::new();
    if let Some(c) = &a.context {
        usage.push(format!("context {}% ({})", c.percent, c.tokens));
    }
    if let Some(cache) = &a.cache {
        usage.push(cache.clone());
    }
    if !usage.is_empty() {
        lines.push(Line::raw(usage.join(" · ")));
    }
    if let Some(item) = &e.crew {
        lines.push(Line::raw(format!(
            "crew {} {} · {}",
            item.id, item.status, item.deliverable
        )));
    }
    lines.push(Line::styled(
        a.cwd.clone(),
        Style::new().fg(Color::DarkGray),
    ));
    if let Some(activity) = &e.activity {
        lines.extend(activity_lines(activity, now, width));
    }
    lines
}

/// Tool lines are cut to `width`; only the last text wraps.
fn activity_lines(activity: &Activity, now: i64, width: usize) -> Vec<Line<'static>> {
    let dim = Style::new().fg(Color::DarkGray);
    let mut lines = Vec::new();
    if !activity.recent.is_empty() {
        lines.push(Line::raw(""));
        lines.push(bold("recent tools".into()));
        for t in &activity.recent {
            let age = t.at.map_or(String::new(), |at| fmt_age(now - at));
            let fixed = vec![
                Span::styled(format!(" {age:>4}  "), dim),
                Span::styled(format!("{:<6} ", t.name), Style::new().fg(Color::Magenta)),
            ];
            lines.push(spread(width, fixed, Span::raw(t.summary.clone()), None));
        }
    }
    if let Some(text) = &activity.last_text {
        lines.push(Line::raw(""));
        lines.push(bold("last text".into()));
        lines.push(Line::raw(format!(" {text}")));
    }
    lines
}

fn child_detail(c: &ChildEntry, now: i64, width: usize) -> Vec<Line<'static>> {
    let state = match c.liveness {
        Liveness::Running => "running",
        Liveness::Done => "done",
        Liveness::Stale => "stale",
    };
    match &c.info {
        ChildInfo::Subagent {
            agent_type,
            description,
            activity,
        } => {
            let mut lines = vec![
                bold(format!("{agent_type} · {state}")),
                Line::raw(description.clone()),
            ];
            if let Some(a) = activity {
                lines.extend(activity_lines(a, now, width));
            }
            lines
        }
        ChildInfo::Workflow {
            name,
            status,
            phase,
            agents,
            ..
        } => {
            let mut lines = vec![bold(format!("workflow {name} · {status}"))];
            if let Some(p) = phase {
                lines.push(Line::raw(format!(
                    "phase {}/{} {}",
                    p.index, p.total, p.title
                )));
            }
            lines.push(Line::raw(""));
            lines.push(bold("agents".into()));
            for a in agents {
                let tool = match (&a.last_tool, &a.last_summary) {
                    (Some(t), Some(s)) => format!("{t}  {s}"),
                    (Some(t), None) => t.clone(),
                    _ => String::new(),
                };
                let line = format!(" {:<8} {}  {tool}", a.state, a.label);
                lines.push(Line::raw(fit(&line, width)));
            }
            lines
        }
    }
}

/// `fixed` spans, then `flex` cut to the room left, then `right` against the right edge.
fn spread(
    width: usize,
    fixed: Vec<Span<'static>>,
    flex: Span<'static>,
    right: Option<Span<'static>>,
) -> Line<'static> {
    let used: usize = fixed.iter().map(Span::width).sum();
    let right_w = right.as_ref().map_or(0, |r| r.width() + 1);
    let room = width.saturating_sub(used + right_w);
    let text = fit(&flex.content, room);
    let gap = room.saturating_sub(text.width());
    let mut spans = fixed;
    spans.push(Span::styled(text, flex.style));
    spans.push(Span::raw(" ".repeat(gap)));
    if let Some(r) = right {
        spans.push(Span::raw(" "));
        spans.push(r);
    }
    Line::from(spans)
}

fn fit(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

fn pad(s: &str, width: usize) -> String {
    let text = fit(s, width);
    let gap = width.saturating_sub(text.width());
    text + &" ".repeat(gap)
}

fn count(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

pub fn fmt_age(secs: i64) -> String {
    let secs = secs.max(0);
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86_400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::children::{ChildKey, Liveness};
    use crate::crew;
    use crate::herdr::{ContextUse, Status};
    use crate::model::tests::{agent, entry, subagent};
    use crate::model::{AgentEntry, ChildEntry, ChildInfo};
    use crate::transcript::{Activity, ToolCall};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::time::Duration;

    const NOW: i64 = 1_000_000;

    fn tool(name: &str, summary: &str, ago: i64) -> ToolCall {
        ToolCall {
            id: format!("{name}{ago}"),
            name: name.into(),
            summary: summary.into(),
            at: Some(NOW - ago),
        }
    }

    fn working() -> AgentEntry {
        let mut e = entry("incr", agent("p1", "worker-1", Status::Working, 2));
        e.agent.context = Some(ContextUse {
            percent: 31,
            tokens: "310k".into(),
        });
        e.agent.cache = Some("cache hit 99.7%".into());
        let mut act = Activity::default();
        act.recent.push_back(tool("Read", "src/lib.rs", 40));
        act.recent
            .push_back(tool("Bash", "cargo nextest run -p sched", 12));
        act.pending.insert("Bash12".into());
        act.last_text = Some("Audit subagent is still running".into());
        e.activity = Some(act);
        let mut child = subagent("c1", Liveness::Running, vec![]);
        if let ChildInfo::Subagent { activity, .. } = &mut child.info {
            let mut a = Activity::default();
            a.recent.push_back(tool("Grep", "stale refs", 1));
            *activity = Some(a);
        }
        let workflow = ChildEntry {
            key: ChildKey::Workflow("wf_1".into()),
            liveness: Liveness::Running,
            info: ChildInfo::Workflow {
                name: "review-changes".into(),
                status: "running".into(),
                phase: Some(crate::children::Phase {
                    index: 3,
                    total: 5,
                    title: "Verify".into(),
                }),
                agents: Vec::new(),
                running: 7,
            },
            active_at: Some(NOW - 60),
            tool_changed_at: None,
            children: Vec::new(),
        };
        e.children = vec![child, workflow];
        e
    }

    fn done() -> AgentEntry {
        let mut e = entry("incr", agent("p2", "integrator", Status::Done, 1));
        e.active_at = Some(NOW - 120);
        e.crew = Some(crew::tests::item("B3", "assigned", "add godview pin"));
        e
    }

    fn view(model: &Model) -> View<'_> {
        View {
            model,
            now: NOW,
            peek: None,
            status: None,
            running_children: 2,
            frame: 0,
            instant: Instant::now(),
            motion: false,
            stream: &[],
        }
    }

    fn render(model: &Model, peek: Option<(&str, &str)>) -> String {
        screen(&draw_on(
            140,
            24,
            &View {
                peek,
                ..view(model)
            },
        ))
    }

    fn draw_on(width: u16, height: u16, view: &View) -> Terminal<TestBackend> {
        let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
        term.draw(|f| draw(f, view, &mut ListState::default()))
            .unwrap();
        term
    }

    fn screen(term: &Terminal<TestBackend>) -> String {
        let buf = term.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn line_with<'a>(screen: &'a str, needle: &str) -> &'a str {
        screen
            .lines()
            .find(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("no line with {needle:?} in\n{screen}"))
    }

    #[test]
    fn tree_shows_status_profile_context_title_and_current_tool() {
        let mut m = Model::default();
        m.update(vec![working(), done()]);
        let s = render(&m, None);
        assert!(line_with(&s, "▾ incr").contains("incr"));
        let row = line_with(&s, "◐ worker-1");
        assert!(row.contains('◐') && row.contains("alpha") && row.contains("31%"));
        assert!(row.contains("worker-1 title"));
        let tool = line_with(&s, "cargo nextest run -p sched");
        assert!(tool.contains("Bash") && tool.contains("12s"));
        let child = line_with(&s, "c1 desc");
        assert!(child.contains("├") && child.contains('●') && child.contains("Explore"));
        assert!(child.contains("Grep"));
        let wf = line_with(&s, "review-changes");
        assert!(wf.contains("└") && wf.contains('◆') && wf.contains("Verify 3/5"));
        let integ = line_with(&s, "integrator");
        assert!(integ.contains('✳') && integ.contains("B3 assigned · add godview pin"));
        assert!(integ.contains("2m"));
    }

    #[test]
    fn header_counts_and_shows_the_active_filters() {
        let mut m = Model::default();
        m.update(vec![working(), done()]);
        let s = render(&m, None);
        let header = s.lines().next().unwrap();
        assert!(header.contains("2 agents · 1 working · 2 subagents running"));
        m.update(vec![working()]);
        let one = View {
            running_children: 1,
            ..view(&m)
        };
        let top = screen(&draw_on(140, 5, &one));
        assert!(
            top.contains("1 agent · 1 working · 1 subagent running"),
            "{top}"
        );
        assert!(header.contains("w working only"));
        m.set_working_only(true);
        let s = render(&m, None);
        assert!(!s.contains("integrator"));
    }

    #[test]
    fn detail_panel_follows_the_selection() {
        let mut m = Model::default();
        m.update(vec![working(), done()]);
        let s = render(&m, None);
        assert!(line_with(&s, "context 31% (310k)").contains("cache hit 99.7%"));
        assert!(s.contains("recent tools"));
        assert!(line_with(&s, "src/lib.rs").contains("Read"));
        assert!(s.contains("Audit subagent is still running"));
    }

    #[test]
    fn peek_replaces_the_detail_with_pane_output() {
        let mut m = Model::default();
        m.update(vec![working()]);
        m.peek = true;
        let s = render(&m, Some(("p1", "$ cargo test\nrunning 3 tests\n")));
        assert!(s.contains("running 3 tests"));
        assert!(!s.contains("recent tools"));
    }

    #[test]
    fn folded_done_agents_show_a_more_row() {
        let mut m = Model::default();
        let many = (0..5)
            .map(|i| {
                entry(
                    "incr",
                    agent(&format!("p{i}"), &format!("w{i}"), Status::Done, i),
                )
            })
            .collect();
        m.update(many);
        assert!(render(&m, None).contains("2 more done"));
    }

    fn at_frame(m: &Model, frame: u64, motion: bool) -> Terminal<TestBackend> {
        draw_on(
            140,
            24,
            &View {
                frame,
                motion,
                ..view(m)
            },
        )
    }

    fn glyph_of(term: &Terminal<TestBackend>, label: &str) -> char {
        let s = screen(term);
        let row = line_with(&s, &pad(label, NAME_W));
        let i = row.find(label).unwrap();
        row[..i].trim_end().chars().last().unwrap()
    }

    #[test]
    fn an_active_agent_spins_and_a_stale_one_holds_still() {
        let mut m = Model::default();
        let mut active = working();
        active.active = true;
        let mut stale = entry("incr", agent("p3", "stalled", Status::Working, 3));
        stale.active = false;
        m.update(vec![active, stale]);
        let (f0, f2) = (at_frame(&m, 0, true), at_frame(&m, 2, true));
        assert_ne!(glyph_of(&f0, "worker-1"), glyph_of(&f2, "worker-1"));
        assert_eq!(glyph_of(&f0, "stalled"), '◐');
        assert_eq!(glyph_of(&f2, "stalled"), '◐');
    }

    fn pulse_cells(term: &Terminal<TestBackend>) -> Vec<(u16, String)> {
        let buf = term.backend().buffer();
        let mut cells = Vec::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if buf[(x, y)].fg == PULSE {
                    cells.push((y, buf[(x, y)].symbol().to_string()));
                }
            }
        }
        cells
    }

    #[test]
    fn one_bright_cell_runs_down_the_wire() {
        let mut m = Model::default();
        let mut e = entry("incr", agent("p1", "w1", Status::Working, 1));
        e.active = true;
        e.children = vec![
            subagent("c1", Liveness::Running, vec![]),
            subagent("c2", Liveness::Done, vec![]),
            subagent("c3", Liveness::Running, vec![]),
        ];
        m.update(vec![e]);
        let row_of = |term: &Terminal<TestBackend>, needle: &str| {
            screen(term)
                .lines()
                .position(|l| l.contains(needle))
                .unwrap() as u16
        };
        for (frame, child, glyph) in [
            (0, "c1", "┣"),
            (2, "c2", "┣"),
            (4, "c3", "┗"),
            (6, "c1", "┣"),
        ] {
            let term = at_frame(&m, frame, true);
            assert_eq!(
                pulse_cells(&term),
                vec![(row_of(&term, &format!("{child} desc")), glyph.to_string())],
                "frame {frame}"
            );
        }
        let off = at_frame(&m, 2, false);
        assert!(pulse_cells(&off).is_empty());
        assert_eq!(screen(&off), screen(&at_frame(&m, 0, false)));
    }

    #[test]
    fn a_blocked_agent_has_no_lit_wire() {
        let mut m = Model::default();
        let mut e = entry("incr", agent("p1", "w1", Status::Blocked, 1));
        e.children = vec![subagent("c1", Liveness::Running, vec![])];
        m.update(vec![e]);
        let buf_has = |term: &Terminal<TestBackend>, color| {
            let buf = term.backend().buffer();
            buf.content().iter().any(|c| c.fg == color)
        };
        let term = at_frame(&m, 0, true);
        assert!(!buf_has(&term, PULSE) && !buf_has(&term, WIRE));
    }

    #[test]
    fn flow_marks_only_a_pending_current_call() {
        let mut m = Model::default();
        let mut e = working();
        e.active = true;
        m.update(vec![e.clone()]);
        let s = screen(&at_frame(&m, 0, true));
        assert!(line_with(&s, "cargo nextest").contains("•··"));
        let act = e.activity.as_mut().unwrap();
        act.pending.clear();
        act.pending.insert("Read40".into());
        m.update(vec![e]);
        let s = screen(&at_frame(&m, 0, true));
        assert!(!line_with(&s, "cargo nextest").contains('•'));
    }

    #[test]
    fn a_new_tool_flashes_its_row_for_a_moment() {
        let mut m = Model::default();
        let mut e = working();
        let at = Instant::now();
        e.tool_changed_at = Some(at);
        m.update(vec![e]);
        let flashed = |instant| {
            let term = draw_on(
                140,
                24,
                &View {
                    instant,
                    ..view(&m)
                },
            );
            let y = screen(&term)
                .lines()
                .position(|l| l.contains("cargo nextest"))
                .unwrap() as u16;
            term.backend().buffer()[(40, y)].bg == FLASH_BG
        };
        assert!(flashed(at + Duration::from_millis(100)));
        assert!(!flashed(at + FLASH));
    }

    #[test]
    fn the_sparkline_needs_a_wide_tree() {
        let mut m = Model::default();
        let mut e = working();
        e.rate[RATE_BUCKETS - 1] = 6;
        e.rate[RATE_BUCKETS - 2] = 3;
        m.update(vec![e]);
        let wide = screen(&draw_on(170, 24, &view(&m)));
        assert!(line_with(&wide, &pad("worker-1", NAME_W)).contains("▁▁▁▁▁▁▁▁▅█"));
        let narrow = screen(&draw_on(140, 24, &view(&m)));
        assert!(!line_with(&narrow, &pad("worker-1", NAME_W)).contains('█'));
    }

    fn stream_item(id: &str, ago: i64, child: Option<&str>) -> StreamItem {
        StreamItem {
            id: id.into(),
            at: NOW - ago,
            who: "worker-1".into(),
            child: child.map(String::from),
            tool: "Grep".into(),
            summary: format!("{id} summary"),
            highlighted_at: None,
        }
    }

    #[test]
    fn the_stream_lists_newest_first_and_needs_a_tall_terminal() {
        let mut m = Model::default();
        m.update(vec![working()]);
        let items = [
            stream_item("new", 1, Some("Explore")),
            stream_item("old", 9, None),
        ];
        let v = View {
            stream: &items,
            ..view(&m)
        };
        let s = screen(&draw_on(140, 30, &v));
        assert!(s.contains(" live "));
        let new = s.lines().position(|l| l.contains("new summary")).unwrap();
        let old = s.lines().position(|l| l.contains("old summary")).unwrap();
        assert!(new < old);
        assert!(line_with(&s, "new summary").contains("worker-1 › Explore"));
        assert!(line_with(&s, "old summary").contains("9s"));
        let short = screen(&draw_on(140, 29, &v));
        assert!(!short.contains(" live ") && !short.contains("new summary"));
    }
}
