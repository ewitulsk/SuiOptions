//! ratatui rendering.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Tabs, Wrap},
    Frame,
};

use crate::app::{status_label, AppState, Focus, Tab};
use crate::flags::FlagValue;
use crate::process::Stream;

pub fn draw(f: &mut Frame, app: &AppState) {
    let area = f.area();
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(2)])
        .split(area);
    let main = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(28), Constraint::Min(20)])
        .split(outer[0]);

    draw_program_list(f, app, main[0]);
    draw_content(f, app, main[1]);
    draw_footer(f, app, outer[1]);

    if app.focus == Focus::PickingSubcommand {
        draw_subcommand_picker(f, app, area);
    }
    if app.focus == Focus::EditingFlag {
        draw_flag_editor(f, app, area);
    }
    if matches!(app.focus, Focus::EditingEnvKey | Focus::EditingEnvValue) {
        draw_env_editor(f, app, area);
    }
}

fn draw_program_list(f: &mut Frame, app: &AppState, area: Rect) {
    let items: Vec<ListItem> = app
        .programs
        .iter()
        .enumerate()
        .map(|(i, spec)| {
            let proc = &app.processes[i];
            let (label, color) = status_label(&proc.status);
            let title = Span::styled(format!(" {:<16}", spec.id), Style::default());
            let status = Span::styled(label, Style::default().fg(color));
            ListItem::new(Line::from(vec![title, status]))
        })
        .collect();

    let title = if app.focus == Focus::ProgramList {
        " Programs ◀ "
    } else {
        " Programs "
    };
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(focus_border_style(
                    app.focus == Focus::ProgramList,
                )),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    let mut state = ListState::default();
    state.select(Some(app.selected_program));
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_content(f: &mut Frame, app: &AppState, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Length(3),
            Constraint::Min(3),
        ])
        .split(area);
    draw_header(f, app, chunks[0]);
    draw_tabs(f, app, chunks[1]);
    match app.tab {
        Tab::Flags => draw_flags(f, app, chunks[2]),
        Tab::Env => draw_env(f, app, chunks[2]),
        Tab::Logs => draw_logs(f, app, chunks[2]),
        Tab::Docs => draw_docs(f, app, chunks[2]),
    }
}

fn draw_header(f: &mut Frame, app: &AppState, area: Rect) {
    let spec = app.current_program();
    let proc = app.current_process();
    let (label, color) = status_label(&proc.status);

    let cmd_preview = {
        let argv = app.current_flags().build_argv();
        let envs = app.current_flags().build_env();
        let mut s = String::new();
        for (k, v) in &envs {
            s.push_str(&format!("{k}={} ", shell_quote(v)));
        }
        s.push_str(&format!("cargo run -p {}", spec.cargo_pkg));
        if !argv.is_empty() {
            s.push_str(" -- ");
            s.push_str(&shell_join(&argv));
        }
        s
    };

    let pid_text = match proc.status {
        crate::process::Status::Running { pid: Some(p) } => format!(" pid={p}"),
        _ => String::new(),
    };

    let lines = vec![
        Line::from(vec![
            Span::styled(
                format!(" {}", spec.id),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw("   "),
            Span::styled(label, Style::default().fg(color)),
            Span::raw(pid_text),
        ]),
        Line::from(Span::styled(
            format!(" cwd: {}", spec.working_dir),
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(Span::styled(
            format!(" $ {cmd_preview}"),
            Style::default().fg(Color::Yellow),
        )),
    ];
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn draw_tabs(f: &mut Frame, app: &AppState, area: Rect) {
    let titles: Vec<Line> = Tab::all()
        .iter()
        .map(|t| Line::from(t.label()))
        .collect();
    let selected = Tab::all().iter().position(|t| *t == app.tab).unwrap_or(0);
    let tabs = Tabs::new(titles)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(focus_border_style(
                    matches!(app.focus, Focus::Content | Focus::EditingFlag),
                )),
        )
        .select(selected)
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .divider("│");
    f.render_widget(tabs, area);
}

fn draw_flags(f: &mut Frame, app: &AppState, area: Rect) {
    let flags = app.current_flags();
    let mut items: Vec<ListItem> = Vec::new();
    let mut data_indices: Vec<Option<usize>> = Vec::new();
    let mut data_idx = 0usize;

    for fl in &flags.global {
        items.push(render_flag(fl));
        data_indices.push(Some(data_idx));
        data_idx += 1;
    }
    if let Some(idx) = flags.selected_subcommand {
        if let Some(sub) = flags.subcommands.get(idx) {
            items.push(ListItem::new(Line::from(Span::styled(
                format!("── subcommand: {} ──", sub.name),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM),
            ))));
            data_indices.push(None);
            for fl in &sub.flags {
                items.push(render_flag(fl));
                data_indices.push(Some(data_idx));
                data_idx += 1;
            }
        }
    }

    let highlight = data_indices
        .iter()
        .position(|i| *i == Some(app.selected_flag));

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Flags  ([e] edit  [Tab] cycle  [r] revert  [c] pick subcommand) "),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );

    let mut state = ListState::default();
    state.select(highlight);
    f.render_stateful_widget(list, area, &mut state);
}

fn render_flag(fl: &crate::flags::FlagState) -> ListItem<'static> {
    let mut spans = Vec::new();
    let name_style = if fl.overridden {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };
    spans.push(Span::styled(
        format!(" {:<24}", fl.spec.display_name()),
        name_style,
    ));
    let value_style = if fl.overridden {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::Gray)
    };
    let value_text = match &fl.value {
        FlagValue::Bool(true) => "● on".to_string(),
        FlagValue::Bool(false) => "○ off".to_string(),
        FlagValue::Text(s) => {
            if s.is_empty() {
                "(empty)".to_string()
            } else {
                s.clone()
            }
        }
    };
    spans.push(Span::styled(value_text, value_style));

    if !fl.spec.possible_values.is_empty() {
        spans.push(Span::styled(
            format!("    [{}]", fl.spec.possible_values.join(" | ")),
            Style::default().fg(Color::DarkGray),
        ));
    }
    if let Some(help) = &fl.spec.help {
        let one_line = help.replace('\n', " ");
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            one_line,
            Style::default().fg(Color::DarkGray),
        ));
    }
    ListItem::new(Line::from(spans))
}

fn draw_env(f: &mut Frame, app: &AppState, area: Rect) {
    let flags = app.current_flags();
    let items: Vec<ListItem> = flags
        .env
        .iter()
        .map(|e| {
            let key_text = if e.key.is_empty() {
                "(unnamed)".to_string()
            } else {
                e.key.clone()
            };
            let value_text = if e.value.is_empty() {
                "(empty)".to_string()
            } else {
                e.value.clone()
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!(" {:<24}", key_text),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(value_text, Style::default().fg(Color::Gray)),
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Env  ([e] edit value  [K] rename key  [a] add  [d] delete) "),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    let mut state = ListState::default();
    if !flags.env.is_empty() {
        state.select(Some(app.selected_env.min(flags.env.len() - 1)));
    }
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_logs(f: &mut Frame, app: &AppState, area: Rect) {
    let proc = app.current_process();
    let lines: Vec<Line> = proc
        .snapshot_logs()
        .into_iter()
        .map(|l| {
            let style = match l.stream {
                Stream::Stdout => Style::default().fg(Color::White),
                Stream::Stderr => Style::default().fg(Color::Red),
                Stream::System => Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::DIM),
            };
            Line::from(Span::styled(l.text, style))
        })
        .collect();
    let total = lines.len();
    let body_height = area.height.saturating_sub(2) as usize;
    let max_scroll = total.saturating_sub(body_height);
    let scroll_from_top = max_scroll.saturating_sub(app.log_scroll);

    let last_cmd = proc
        .last_command
        .as_deref()
        .unwrap_or("(no command yet)");
    let title = format!(" Logs ({total} lines) — {last_cmd} ");
    let para = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(title))
        .scroll((scroll_from_top as u16, 0));
    f.render_widget(para, area);
}

fn draw_docs(f: &mut Frame, app: &AppState, area: Rect) {
    let spec = app.current_program();
    let mut lines = vec![
        Line::from(Span::styled(
            spec.id.to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(spec.description.to_string()),
    ];

    if let Some(about) = spec.about() {
        lines.push(Line::from(""));
        for l in about.lines() {
            lines.push(Line::from(l.to_string()));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Flags",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));
    for a in spec.arg_specs() {
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {:<22}", a.display_name()),
                Style::default().fg(Color::Yellow),
            ),
            Span::raw(a.help.unwrap_or_default()),
        ]));
    }
    let subs = spec.subcommand_specs();
    if !subs.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Subcommands",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        for s in &subs {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {:<22}", s.name),
                    Style::default().fg(Color::Yellow),
                ),
                Span::raw(s.about.clone().unwrap_or_default()),
            ]));
        }
    }

    let block = Block::default().borders(Borders::ALL).title(" Docs ");
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(block), area);
}

fn draw_footer(f: &mut Frame, app: &AppState, area: Rect) {
    let help = match app.focus {
        Focus::EditingFlag | Focus::EditingEnvKey | Focus::EditingEnvValue => {
            "[Enter] commit  [Esc] cancel".to_string()
        }
        Focus::PickingSubcommand => "[↑/↓] pick  [Enter] choose  [Esc] cancel".to_string(),
        _ => {
            let proc_action = if app.current_process().is_running() {
                "[s] stop"
            } else {
                "[r] run"
            };
            let tab_hint = match app.tab {
                Tab::Flags => "[j/k] flag  [e] edit",
                Tab::Env => "[j/k] var  [e] edit  [a] add  [d] delete",
                Tab::Logs => "[j/k] scroll  [g/G] top/bottom",
                Tab::Docs => "",
            };
            format!(
                "[↑/↓] program  [→/←] panel  [Tab] tab  {tab_hint}  {proc_action}  [q] quit"
            )
        }
    };
    let status = app
        .status_msg
        .clone()
        .unwrap_or_else(|| String::from(""));
    let lines = vec![
        Line::from(Span::styled(help, Style::default().fg(Color::DarkGray))),
        Line::from(Span::styled(status, Style::default().fg(Color::Green))),
    ];
    f.render_widget(Paragraph::new(lines), area);
}

fn draw_flag_editor(f: &mut Frame, app: &AppState, area: Rect) {
    let popup = centered_rect(60, 20, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Edit flag value (Enter = commit, Esc = cancel) ");
    let p = Paragraph::new(app.edit_buffer.as_str()).block(block);
    f.render_widget(ratatui::widgets::Clear, popup);
    f.render_widget(p, popup);
}

fn draw_env_editor(f: &mut Frame, app: &AppState, area: Rect) {
    let popup = centered_rect(60, 20, area);
    let title = match app.focus {
        Focus::EditingEnvKey => " Edit env var KEY (Enter = commit, Esc = cancel) ",
        Focus::EditingEnvValue => " Edit env var VALUE (Enter = commit, Esc = cancel) ",
        _ => " Edit env var ",
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let p = Paragraph::new(app.edit_buffer.as_str()).block(block);
    f.render_widget(ratatui::widgets::Clear, popup);
    f.render_widget(p, popup);
}

fn draw_subcommand_picker(f: &mut Frame, app: &AppState, area: Rect) {
    let popup = centered_rect(50, 60, area);
    let flags = app.current_flags();
    let items: Vec<ListItem> = flags
        .subcommands
        .iter()
        .enumerate()
        .map(|(i, sub)| {
            let marker = if Some(i) == flags.selected_subcommand {
                "● "
            } else {
                "  "
            };
            let about = sub.about.clone().unwrap_or_default();
            ListItem::new(Line::from(vec![
                Span::raw(marker),
                Span::styled(
                    sub.name.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(about, Style::default().fg(Color::DarkGray)),
            ]))
        })
        .collect();
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Pick subcommand "),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    let mut state = ListState::default();
    state.select(flags.selected_subcommand);
    f.render_widget(ratatui::widgets::Clear, popup);
    f.render_stateful_widget(list, popup, &mut state);
}

fn focus_border_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup[1])[1]
}

fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(s: &str) -> String {
    if s.chars().any(|c| c.is_whitespace() || c == '"') {
        format!("\"{}\"", s.replace('"', "\\\""))
    } else {
        s.to_string()
    }
}
