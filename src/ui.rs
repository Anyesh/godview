use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, HighlightSpacing, List, ListItem, ListState, Paragraph, Wrap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::children::Liveness;
use crate::herdr::Status;
use crate::model::{AgentEntry, ChildEntry, ChildInfo, Model, Row};
use crate::transcript::Activity;

const NAME_W: usize = 12;
const PROFILE_W: usize = 8;
const TYPE_W: usize = 15;
const TOOL_W: usize = 8;
/// Where an agent's title starts: indent, glyph, name, profile and context columns.
const TITLE_COL: usize = 2 + 2 + NAME_W + 1 + PROFILE_W + 1 + 4 + 2;
const SPLIT_MIN_WIDTH: u16 = 80;

pub struct View<'a> {
    pub model: &'a Model,
    pub now: i64,
    pub peek: Option<&'a str>,
    pub status: Option<&'a str>,
    pub running_children: usize,
}

pub fn draw(frame: &mut Frame, view: &View, list: &mut ListState) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    frame.render_widget(header_line(view, header.width as usize), header);
    if body.width >= SPLIT_MIN_WIDTH {
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(62), Constraint::Percentage(38)])
                .areas(body);
        draw_tree(frame, view, left, list);
        draw_detail(frame, view, right);
    } else {
        draw_tree(frame, view, body, list);
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
            "   {total} agents · {working} working · {} subagents running",
            view.running_children
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
    let right = vec![
        toggle(view.model.working_only(), "w working only"),
        Span::styled(" · ", Style::new().fg(Color::DarkGray)),
        toggle(view.model.all_children, "a all"),
        Span::raw(" "),
    ];
    justify(left, right, width)
}

fn footer_line(view: &View, width: usize) -> Line<'static> {
    let keys = Span::styled(
        " j/k move · enter go to pane · p peek output · space fold/expand · q quit",
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

fn draw_tree(frame: &mut Frame, view: &View, area: Rect, list: &mut ListState) {
    let block = Block::bordered().title(" agents ");
    let rows = view.model.rows();
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
    let items: Vec<ListItem> = rows
        .iter()
        .map(|(_, row)| ListItem::new(row_text(row, width, view)))
        .collect();
    list.select(view.model.selected_index());
    let widget = List::new(items)
        .block(block)
        .highlight_symbol("▶ ")
        .highlight_spacing(HighlightSpacing::Always)
        .highlight_style(Style::new().bg(Color::Indexed(236)));
    frame.render_stateful_widget(widget, area, list);
}

fn row_text(row: &Row, width: usize, view: &View) -> Text<'static> {
    match row {
        Row::Group { group, collapsed } => {
            let arrow = if *collapsed { "▸" } else { "▾" };
            Text::from(Line::styled(
                format!("{arrow} {}", group.label),
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ))
        }
        Row::Agent { entry } => agent_text(entry, width, view.now),
        Row::Child {
            child,
            guides,
            last,
            ..
        } => Text::from(child_line(child, guides, *last, width, view.now)),
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

fn agent_text(e: &AgentEntry, width: usize, now: i64) -> Text<'static> {
    let a = &e.agent;
    let busy = matches!(a.status, Status::Working | Status::Blocked);
    let dim = Style::new().fg(Color::DarkGray);
    let context = a
        .context
        .as_ref()
        .map_or(String::new(), |c| format!("{}%", c.percent));
    let fixed = vec![
        Span::raw("  "),
        status_glyph(a.status),
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
    if busy {
        if let Some(tool) = e.activity.as_ref().and_then(Activity::current) {
            let indent = Span::raw(" ".repeat(TITLE_COL));
            let name = Span::styled(format!("{}  ", tool.name), Style::new().fg(Color::Magenta));
            let age = tool.at.map(|t| Span::styled(fmt_age(now - t), dim));
            lines.push(spread(
                width,
                vec![indent, name],
                Span::raw(tool.summary.clone()),
                age,
            ));
        }
    }
    Text::from(lines)
}

fn child_line(
    c: &ChildEntry,
    guides: &[bool],
    last: bool,
    width: usize,
    now: i64,
) -> Line<'static> {
    let dim = Style::new().fg(Color::DarkGray);
    let mut fixed = vec![Span::raw("    ")];
    for &more in guides {
        fixed.push(Span::styled(if more { "│ " } else { "  " }, dim));
    }
    fixed.push(Span::styled(if last { "└ " } else { "├ " }, dim));
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
            spread(
                width,
                fixed,
                Span::styled(description.clone(), text_style),
                Some(right),
            )
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

fn draw_detail(frame: &mut Frame, view: &View, area: Rect) {
    let selection = view.model.selection();
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
        let lines: Vec<&str> = view.peek.unwrap_or("").lines().collect();
        let shown = lines[lines.len().saturating_sub(height)..].join("\n");
        frame.render_widget(Paragraph::new(shown).block(block), area);
        return;
    }
    let lines = match selection {
        Some((entry, None)) => agent_detail(entry, view.now),
        Some((_, Some(child))) => child_detail(child, view.now),
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

fn agent_detail(e: &AgentEntry, now: i64) -> Vec<Line<'static>> {
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
        lines.extend(activity_lines(activity, now));
    }
    lines
}

fn activity_lines(activity: &Activity, now: i64) -> Vec<Line<'static>> {
    let dim = Style::new().fg(Color::DarkGray);
    let mut lines = Vec::new();
    if !activity.recent.is_empty() {
        lines.push(Line::raw(""));
        lines.push(bold("recent tools".into()));
        for t in &activity.recent {
            let age = t.at.map_or(String::new(), |at| fmt_age(now - at));
            lines.push(Line::from(vec![
                Span::styled(format!(" {age:>4}  "), dim),
                Span::styled(format!("{:<6} ", t.name), Style::new().fg(Color::Magenta)),
                Span::raw(t.summary.clone()),
            ]));
        }
    }
    if let Some(text) = &activity.last_text {
        lines.push(Line::raw(""));
        lines.push(bold("last text".into()));
        lines.push(Line::raw(format!(" {text}")));
    }
    lines
}

fn child_detail(c: &ChildEntry, now: i64) -> Vec<Line<'static>> {
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
                lines.extend(activity_lines(a, now));
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
                lines.push(Line::raw(format!(" {:<8} {}  {tool}", a.state, a.label)));
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

    fn render(model: &Model, peek: Option<&str>) -> String {
        let mut term = Terminal::new(TestBackend::new(140, 24)).unwrap();
        let mut list = ListState::default();
        let view = View {
            model,
            now: NOW,
            peek,
            status: None,
            running_children: 2,
        };
        term.draw(|f| draw(f, &view, &mut list)).unwrap();
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
        let row = line_with(&s, "worker-1");
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
        assert!(header.contains("w working only"));
        m.set_working_only(true);
        let s = render(&m, None);
        assert!(!s.contains("integrator"));
    }

    #[test]
    fn detail_panel_follows_the_selection() {
        let mut m = Model::default();
        m.update(vec![working(), done()]);
        m.move_selection(1);
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
        m.move_selection(1);
        m.peek = true;
        let s = render(&m, Some("$ cargo test\nrunning 3 tests\n"));
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
}
