use ratatui::{
    Frame,
    layout::{Constraint, Flex, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
};

use crate::chat::{
    AttentionSeverity, FocusPane, LayoutMode, Overlay, WorkspaceSnapshot, WorkspaceState,
    compute_layout,
};

const MAX_RENDERED_MESSAGES: usize = 100;

pub fn render_workspace(
    frame: &mut Frame<'_>,
    snapshot: &WorkspaceSnapshot,
    state: &WorkspaceState,
) {
    let layout = compute_layout(frame.area(), state.primary_view());
    if layout.mode == LayoutMode::TooSmall {
        render_compact(
            frame,
            layout.compact_status.unwrap_or(frame.area()),
            snapshot,
        );
        return;
    }
    render_header(frame, layout.header, snapshot);
    if let Some(area) = layout.task_graph {
        render_tasks(frame, area, snapshot, state);
    }
    if let Some(area) = layout.conversation {
        render_conversation(frame, area, snapshot, state);
    }
    if let Some(area) = layout.inspector {
        render_inspector(frame, area, snapshot, state);
    }
    if let Some(area) = layout.composer {
        render_composer(frame, area, snapshot, state);
    }
    if let Some(overlay) = state.overlay() {
        render_overlay(frame, frame.area(), overlay, snapshot, state);
    }
}

fn render_header(frame: &mut Frame<'_>, area: Rect, snapshot: &WorkspaceSnapshot) {
    let connectivity = match snapshot.daemon {
        crate::chat::DaemonConnectivity::Online => "daemon online",
        crate::chat::DaemonConnectivity::Stale => "daemon stale - READ ONLY",
        crate::chat::DaemonConnectivity::Offline => "daemon offline - READ ONLY",
    };
    let text = format!(
        " COLAY - session: {} - {} running / {} blocked - {} ",
        snapshot.session_title, snapshot.running_count, snapshot.blocked_count, connectivity
    );
    frame.render_widget(
        Paragraph::new(text).style(Style::default().add_modifier(Modifier::BOLD)),
        area,
    );
}

fn render_tasks(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &WorkspaceSnapshot,
    state: &WorkspaceState,
) {
    let title = focused_title("TASK GRAPH", state.focus() == FocusPane::Tasks);
    let mut items = snapshot
        .tasks
        .iter()
        .map(|task| {
            let selected = if state.selected_task() == Some(task.task_id.as_str()) {
                "> "
            } else {
                "  "
            };
            ListItem::new(vec![
                Line::from(format!(
                    "{selected}{} {} {}",
                    task.state_symbol, task.state, task.title
                )),
                Line::from(format!("    {}", task.dependency_status)),
            ])
        })
        .collect::<Vec<_>>();
    if !snapshot.attention.is_empty() {
        items.push(ListItem::new(Line::from(Span::styled(
            "ATTENTION",
            Style::default().add_modifier(Modifier::BOLD),
        ))));
        items.extend(snapshot.attention.iter().map(|attention| {
            let symbol = match attention.severity {
                AttentionSeverity::Info => "i",
                AttentionSeverity::Warning => "!",
                AttentionSeverity::Critical => "!!",
            };
            ListItem::new(format!(" {symbol} {}", attention.label))
        }));
    }
    frame.render_widget(List::new(items).block(panel(title)), area);
}

fn render_conversation(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &WorkspaceSnapshot,
    state: &WorkspaceState,
) {
    let title = focused_title("CONVERSATION", state.focus() == FocusPane::Conversation);
    let end = snapshot
        .messages
        .len()
        .saturating_sub(state.conversation_scroll());
    let visible_message_capacity =
        usize::from(area.height.saturating_sub(3) / 3).clamp(1, MAX_RENDERED_MESSAGES);
    let start = end.saturating_sub(visible_message_capacity);
    let mut lines = Vec::new();
    if start > 0 || snapshot.has_older_messages {
        lines.push(Line::from("... more messages (scroll to load) ..."));
    }
    for message in &snapshot.messages[start..end] {
        let target = message
            .task_id
            .as_deref()
            .map(|task_id| format!(" [{task_id}]"))
            .unwrap_or_default();
        let state_suffix = if message.state == "final" {
            String::new()
        } else {
            format!(" ({})", message.state)
        };
        lines.push(Line::from(Span::styled(
            format!("{}{}{}", message.role, target, state_suffix),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        if message.folded {
            lines.push(Line::from(format!("[tool] {}", message.content)));
        } else {
            lines.push(Line::from(message.content.as_str()));
        }
        lines.push(Line::default());
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel(title))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_inspector(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &WorkspaceSnapshot,
    state: &WorkspaceState,
) {
    let title = focused_title("INSPECTOR", state.focus() == FocusPane::Inspector);
    let lines = snapshot.inspector.as_ref().map_or_else(
        || vec![Line::from("Select a task to inspect")],
        |inspector| {
            let mut lines = vec![
                Line::from(Span::styled(
                    inspector.task_id.as_str(),
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                Line::from(inspector.state.as_str()),
                Line::from(format!("provider  {}", inspector.provider)),
                Line::from(format!(
                    "profile   {}/{}",
                    inspector.profile, inspector.effort
                )),
                Line::from(format!("progress  {}", inspector.progress)),
                Line::from(format!("elapsed   {}", inspector.elapsed)),
                Line::default(),
                Line::from("DEPENDENCIES"),
            ];
            lines.extend(
                inspector
                    .dependencies
                    .iter()
                    .map(|value| Line::from(format!("- {value}"))),
            );
            lines.push(Line::default());
            lines.push(Line::from(format!("worktree {}", inspector.worktree)));
            lines.push(Line::from(format!(
                "files    {}",
                inspector.changed_files.len()
            )));
            lines.extend(
                inspector
                    .tests
                    .iter()
                    .map(|value| Line::from(format!("test     {value}"))),
            );
            lines
        },
    );
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel(title))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_composer(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &WorkspaceSnapshot,
    state: &WorkspaceState,
) {
    let read_only = snapshot
        .read_only_reason
        .as_deref()
        .map(|reason| format!("READ ONLY: {reason}"));
    let feedback = state
        .feedback()
        .map(|feedback| feedback.message.clone())
        .or(read_only)
        .unwrap_or_else(|| "Enter send | Ctrl+T target | / commands | ? help".to_owned());
    let input = if state.composer().is_empty() {
        "Message or /command..."
    } else {
        state.composer()
    };
    let lines = vec![
        Line::from(format!(
            "{}to: {}",
            if state.focus() == FocusPane::Composer {
                "> "
            } else {
                "  "
            },
            state.composer_target().label()
        )),
        Line::from(input),
        Line::from(feedback),
    ];
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_compact(frame: &mut Frame<'_>, area: Rect, snapshot: &WorkspaceSnapshot) {
    let text = vec![
        Line::from(Span::styled(
            "COLAY - safe compact status",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(format!("session: {}", snapshot.session_title)),
        Line::from(format!(
            "{} running / {} blocked",
            snapshot.running_count, snapshot.blocked_count
        )),
        Line::default(),
        Line::from("terminal too narrow; resize to at least 60 columns"),
        Line::from("execution continues in the repository daemon"),
        Line::from("q or Esc: exit"),
    ];
    frame.render_widget(
        Paragraph::new(text)
            .block(panel(" COMPACT "))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_overlay(
    frame: &mut Frame<'_>,
    area: Rect,
    overlay: &Overlay,
    snapshot: &WorkspaceSnapshot,
    state: &WorkspaceState,
) {
    let (title, lines) = match overlay {
        Overlay::TaskSwitcher => (
            " TASK SWITCHER ",
            snapshot
                .tasks
                .iter()
                .enumerate()
                .map(|(index, task)| {
                    Line::from(format!(
                        "{}: {} {}",
                        index + 1,
                        task.state_symbol,
                        task.title
                    ))
                })
                .collect(),
        ),
        Overlay::Overview => (
            " ORCHESTRATION OVERVIEW ",
            vec![
                Line::from(format!("session  {}", snapshot.session_title)),
                Line::from(format!("state    {}", snapshot.session_state)),
                Line::from(format!("tasks    {}", snapshot.tasks.len())),
                Line::from(format!("attention {}", snapshot.attention.len())),
            ],
        ),
        Overlay::FullLog => (
            " FULL LOG ",
            snapshot
                .messages
                .iter()
                .rev()
                .take(MAX_RENDERED_MESSAGES)
                .rev()
                .map(|message| Line::from(format!("{}: {}", message.role, message.content)))
                .collect(),
        ),
        Overlay::TargetPicker => {
            let mut lines = vec![Line::from("o: orchestrator"), Line::from("a: all running")];
            lines.extend(
                snapshot
                    .tasks
                    .iter()
                    .enumerate()
                    .map(|(index, task)| Line::from(format!("{}: {}", index + 1, task.task_id))),
            );
            (" COMPOSER TARGET ", lines)
        }
        Overlay::CommandPalette => (
            " COMMAND PALETTE ",
            vec![Line::from(
                "/tasks /plan /approve /pause /resume /cancel /handover /retry /checkpoint /provider",
            )],
        ),
        Overlay::Help => (
            " HELP ",
            vec![
                Line::from("Tab/Shift+Tab panes | j/k move | Enter select/send"),
                Line::from("Ctrl+P tasks | Ctrl+O overview | Ctrl+L log"),
                Line::from("Ctrl+T target | Ctrl+Space pause/resume | / commands"),
                Line::from("Esc close | q quit outside composer"),
            ],
        ),
        Overlay::Inspector => {
            let lines = snapshot.inspector.as_ref().map_or_else(
                || vec![Line::from("Select a task to inspect")],
                |inspector| {
                    vec![
                        Line::from(inspector.task_id.as_str()),
                        Line::from(inspector.state.as_str()),
                        Line::from(format!("{}/{}", inspector.profile, inspector.effort)),
                    ]
                },
            );
            (" INSPECTOR ", lines)
        }
    };
    let overlay_area = centered_rect(area, 72, 70);
    frame.render_widget(Clear, overlay_area);
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel(title))
            .wrap(Wrap { trim: false }),
        overlay_area,
    );
    let _ = state;
}

fn centered_rect(area: Rect, horizontal_percent: u16, vertical_percent: u16) -> Rect {
    let [area] = Layout::horizontal([Constraint::Percentage(horizontal_percent)])
        .flex(Flex::Center)
        .areas(area);
    let [area] = Layout::vertical([Constraint::Percentage(vertical_percent)])
        .flex(Flex::Center)
        .areas(area);
    area
}

fn focused_title(title: &str, focused: bool) -> String {
    if focused {
        format!("> {title} <")
    } else {
        format!(" {title} ")
    }
}

fn panel(title: impl Into<String>) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .title(title.into())
        .border_style(Style::default().fg(Color::White))
}

#[cfg(test)]
mod tests {
    use std::io;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend, buffer::Cell};

    use super::render_workspace;
    use crate::chat::{
        AttentionItem, AttentionSeverity, DaemonConnectivity, LayoutMode, TaskInspector,
        TaskSummary, TimelineEntry, WorkspaceSnapshot, WorkspaceState,
    };

    fn snapshot() -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            repository: "colay".to_owned(),
            session_id: "session-01".to_owned(),
            session_title: "auth-refactor".to_owned(),
            session_state: "running".to_owned(),
            daemon: DaemonConnectivity::Online,
            running_count: 1,
            blocked_count: 1,
            tasks: vec![TaskSummary {
                task_id: "task-03".to_owned(),
                title: "tests".to_owned(),
                state: "RUNNING".to_owned(),
                state_symbol: "*".to_owned(),
                dependency_status: "task-01 done".to_owned(),
                needs_attention: true,
            }],
            messages: vec![TimelineEntry {
                ordinal: 1,
                message_id: "message-01".to_owned(),
                task_id: Some("task-03".to_owned()),
                role: "agent".to_owned(),
                kind: "tool_summary".to_owned(),
                state: "final".to_owned(),
                content: "cargo test: 31 passed".to_owned(),
                created_at: "2026-07-21T00:00:00Z".to_owned(),
                folded: true,
            }],
            attention: vec![AttentionItem {
                key: "attention-01".to_owned(),
                task_id: Some("task-03".to_owned()),
                severity: AttentionSeverity::Warning,
                label: "1 approval".to_owned(),
            }],
            inspector: Some(TaskInspector {
                task_id: "task-03".to_owned(),
                state: "RUNNING".to_owned(),
                provider: "codex".to_owned(),
                profile: "premium".to_owned(),
                effort: "high".to_owned(),
                progress: "60%".to_owned(),
                elapsed: "4m".to_owned(),
                dependencies: vec!["task-01 done".to_owned()],
                worktree: ".worktrees/task-03".to_owned(),
                changed_files: vec!["tests/auth.rs".to_owned()],
                tests: vec!["31 passed".to_owned()],
            }),
            ..WorkspaceSnapshot::default()
        }
    }

    fn rendered_text(
        width: u16,
        height: u16,
        snapshot: &WorkspaceSnapshot,
        state: &WorkspaceState,
    ) -> Result<String, io::Error> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend)?;
        terminal.draw(|frame| render_workspace(frame, snapshot, state))?;
        Ok(terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(Cell::symbol)
            .collect())
    }

    #[test]
    fn wide_layout_renders_three_panes_attention_and_explicit_target() -> Result<(), io::Error> {
        for width in [110, 160] {
            let text = rendered_text(width, 40, &snapshot(), &WorkspaceState::default())?;
            for expected in [
                "COLAY",
                "TASK GRAPH",
                "CONVERSATION",
                "INSPECTOR",
                "ATTENTION",
                "to: orchestrator",
                "* RUNNING",
                "daemon online",
            ] {
                assert!(text.contains(expected), "missing `{expected}` at {width}");
            }
        }
        Ok(())
    }

    #[test]
    fn medium_layout_uses_inspector_overlay() -> Result<(), io::Error> {
        let snapshot = snapshot();
        let mut state = WorkspaceState::default();
        let initial = rendered_text(100, 30, &snapshot, &state)?;
        assert!(!initial.contains("INSPECTOR"));
        state.handle_key(
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL),
            &snapshot,
            LayoutMode::Medium,
        );
        let overview = rendered_text(100, 30, &snapshot, &state)?;
        assert!(overview.contains("ORCHESTRATION OVERVIEW"));
        Ok(())
    }

    #[test]
    fn narrow_and_compact_modes_degrade_safely() -> Result<(), io::Error> {
        let narrow = rendered_text(70, 24, &snapshot(), &WorkspaceState::default())?;
        assert!(narrow.contains("CONVERSATION"));
        assert!(!narrow.contains("TASK GRAPH"));

        let compact = rendered_text(59, 20, &snapshot(), &WorkspaceState::default())?;
        assert!(compact.contains("terminal too narrow"));
        assert!(!compact.contains("Message or /command"));
        Ok(())
    }

    #[test]
    fn thousand_message_snapshot_renders_a_bounded_tail() -> Result<(), io::Error> {
        let mut snapshot = snapshot();
        snapshot.messages = (1..=1_000)
            .map(|ordinal| TimelineEntry {
                ordinal,
                message_id: format!("message-{ordinal}"),
                role: "agent".to_owned(),
                kind: "agent_message".to_owned(),
                state: "final".to_owned(),
                content: format!("message content {ordinal}"),
                ..TimelineEntry::default()
            })
            .collect();
        snapshot.has_older_messages = true;
        let text = rendered_text(160, 40, &snapshot, &WorkspaceState::default())?;
        assert!(text.contains("more messages"));
        assert!(text.contains("message content 1000"));
        assert!(!text.contains("message content 1 "));
        Ok(())
    }
}
