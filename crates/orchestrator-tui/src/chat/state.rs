use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::input::{PaletteCommand, parse_palette_command, parse_submission};
use crate::chat::{
    ActionFeedback, ComposerTarget, LayoutMode, PrimaryView, TaskControlIntent, WorkspaceAction,
    WorkspaceSnapshot,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FocusPane {
    Tasks,
    #[default]
    Conversation,
    Inspector,
    Composer,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Overlay {
    TaskSwitcher,
    Overview,
    FullLog,
    TargetPicker,
    CommandPalette,
    Help,
    Inspector,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiEffect {
    None,
    Dispatch(WorkspaceAction),
    Feedback(ActionFeedback),
    Redraw,
}

#[derive(Clone, Debug)]
pub struct WorkspaceState {
    focus: FocusPane,
    primary_view: PrimaryView,
    selected_task: Option<String>,
    selected_task_index: Option<usize>,
    composer_target: ComposerTarget,
    composer: String,
    overlay: Option<Overlay>,
    conversation_scroll: usize,
    feedback: Option<ActionFeedback>,
}

impl Default for WorkspaceState {
    fn default() -> Self {
        Self {
            focus: FocusPane::Conversation,
            primary_view: PrimaryView::Conversation,
            selected_task: None,
            selected_task_index: None,
            composer_target: ComposerTarget::Orchestrator,
            composer: String::new(),
            overlay: None,
            conversation_scroll: 0,
            feedback: None,
        }
    }
}

impl WorkspaceState {
    #[must_use]
    pub const fn focus(&self) -> FocusPane {
        self.focus
    }

    pub const fn set_focus(&mut self, focus: FocusPane) {
        self.focus = focus;
        self.primary_view = match focus {
            FocusPane::Tasks => PrimaryView::Tasks,
            FocusPane::Inspector => PrimaryView::Inspector,
            FocusPane::Conversation | FocusPane::Composer => PrimaryView::Conversation,
        };
    }

    #[must_use]
    pub const fn primary_view(&self) -> PrimaryView {
        self.primary_view
    }

    #[must_use]
    pub fn selected_task(&self) -> Option<&str> {
        self.selected_task.as_deref()
    }

    pub fn select_task(&mut self, task_id: Option<String>) {
        self.selected_task = task_id;
    }

    #[must_use]
    pub const fn composer_target(&self) -> &ComposerTarget {
        &self.composer_target
    }

    #[must_use]
    pub fn composer(&self) -> &str {
        &self.composer
    }

    pub fn set_composer(&mut self, value: impl Into<String>) {
        self.composer = value.into();
    }

    #[must_use]
    pub fn overlay(&self) -> Option<&Overlay> {
        self.overlay.as_ref()
    }

    #[must_use]
    pub const fn conversation_scroll(&self) -> usize {
        self.conversation_scroll
    }

    #[must_use]
    pub const fn feedback(&self) -> Option<&ActionFeedback> {
        self.feedback.as_ref()
    }

    pub fn set_feedback(&mut self, feedback: ActionFeedback) {
        self.feedback = Some(feedback);
    }

    pub fn reconcile_snapshot(&mut self, snapshot: &WorkspaceSnapshot) {
        if self.selected_task.is_none()
            && let Some(inspector) = snapshot.inspector.as_ref()
        {
            self.selected_task = Some(inspector.task_id.clone());
        }
        if let Some(selected) = self.selected_task.as_deref() {
            self.selected_task_index = snapshot
                .tasks
                .iter()
                .position(|task| task.task_id == selected);
            if self.selected_task_index.is_none() {
                self.selected_task = None;
            }
        }
        self.conversation_scroll = self
            .conversation_scroll
            .min(snapshot.messages.len().saturating_sub(1));
    }

    pub fn handle_key(
        &mut self,
        key: KeyEvent,
        snapshot: &WorkspaceSnapshot,
        layout_mode: LayoutMode,
    ) -> UiEffect {
        if layout_mode == LayoutMode::TooSmall {
            return if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) {
                UiEffect::Dispatch(WorkspaceAction::Quit)
            } else {
                UiEffect::Feedback(ActionFeedback::info(
                    "terminal is too narrow for mutations; resize to at least 60 columns",
                ))
            };
        }
        if self.overlay.is_some() {
            return self.handle_overlay_key(key, snapshot);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return self.handle_control_key(key.code, snapshot);
        }
        match key.code {
            KeyCode::Tab => {
                self.set_focus(next_focus(self.focus, true));
                UiEffect::Redraw
            }
            KeyCode::BackTab => {
                self.set_focus(next_focus(self.focus, false));
                UiEffect::Redraw
            }
            KeyCode::Char('?') => {
                self.overlay = Some(Overlay::Help);
                UiEffect::Redraw
            }
            KeyCode::Char('/') => {
                self.overlay = Some(Overlay::CommandPalette);
                self.set_focus(FocusPane::Composer);
                "/".clone_into(&mut self.composer);
                UiEffect::Redraw
            }
            KeyCode::Esc => {
                self.overlay = None;
                UiEffect::Redraw
            }
            KeyCode::Char('q') if self.focus != FocusPane::Composer => {
                UiEffect::Dispatch(WorkspaceAction::Quit)
            }
            KeyCode::Down | KeyCode::Char('j') if self.focus == FocusPane::Tasks => {
                self.move_task_selection(snapshot, true);
                UiEffect::Redraw
            }
            KeyCode::Up | KeyCode::Char('k') if self.focus == FocusPane::Tasks => {
                self.move_task_selection(snapshot, false);
                UiEffect::Redraw
            }
            KeyCode::Down | KeyCode::Char('j') if self.focus == FocusPane::Conversation => {
                self.conversation_scroll = self.conversation_scroll.saturating_sub(1);
                UiEffect::Redraw
            }
            KeyCode::Up | KeyCode::Char('k') if self.focus == FocusPane::Conversation => {
                self.conversation_scroll = self.conversation_scroll.saturating_add(1);
                UiEffect::Redraw
            }
            KeyCode::Enter if self.focus == FocusPane::Composer => self.submit_composer(snapshot),
            KeyCode::Enter if self.focus == FocusPane::Tasks => {
                self.primary_view = PrimaryView::Inspector;
                UiEffect::Redraw
            }
            KeyCode::Backspace if self.focus == FocusPane::Composer => {
                self.composer.pop();
                UiEffect::Redraw
            }
            KeyCode::Char(character)
                if self.focus == FocusPane::Composer
                    && !character.is_control()
                    && self.composer.len() + character.len_utf8() <= 4_096 =>
            {
                self.composer.push(character);
                UiEffect::Redraw
            }
            _ => UiEffect::None,
        }
    }

    fn handle_control_key(&mut self, code: KeyCode, snapshot: &WorkspaceSnapshot) -> UiEffect {
        match code {
            KeyCode::Char('p' | 'P') => {
                self.overlay = Some(Overlay::TaskSwitcher);
                UiEffect::Redraw
            }
            KeyCode::Char('o' | 'O') => {
                self.overlay = Some(Overlay::Overview);
                UiEffect::Redraw
            }
            KeyCode::Char('l' | 'L') => {
                self.overlay = Some(Overlay::FullLog);
                UiEffect::Redraw
            }
            KeyCode::Char('t' | 'T') => {
                self.overlay = Some(Overlay::TargetPicker);
                UiEffect::Redraw
            }
            KeyCode::Char(' ') => self.task_control(snapshot, TaskControlIntent::Pause),
            _ => UiEffect::None,
        }
    }

    fn handle_overlay_key(&mut self, key: KeyEvent, snapshot: &WorkspaceSnapshot) -> UiEffect {
        if key.code == KeyCode::Esc {
            self.overlay = None;
            return UiEffect::Redraw;
        }
        match self.overlay {
            Some(Overlay::TargetPicker) => match key.code {
                KeyCode::Char('o' | 'O') => {
                    self.composer_target = ComposerTarget::Orchestrator;
                    self.overlay = None;
                    UiEffect::Redraw
                }
                KeyCode::Char('a' | 'A') => {
                    self.composer_target = ComposerTarget::AllRunning;
                    self.overlay = None;
                    UiEffect::Redraw
                }
                KeyCode::Char(digit) if digit.is_ascii_digit() && digit != '0' => {
                    let index = digit
                        .to_digit(10)
                        .and_then(|value| usize::try_from(value).ok())
                        .unwrap_or_default()
                        .saturating_sub(1);
                    if let Some(task) = snapshot.tasks.get(index) {
                        self.composer_target = ComposerTarget::Task(task.task_id.clone());
                        self.overlay = None;
                    }
                    UiEffect::Redraw
                }
                _ => UiEffect::None,
            },
            Some(Overlay::TaskSwitcher) => match key.code {
                KeyCode::Down | KeyCode::Char('j') => {
                    self.move_task_selection(snapshot, true);
                    UiEffect::Redraw
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.move_task_selection(snapshot, false);
                    UiEffect::Redraw
                }
                KeyCode::Enter => {
                    self.overlay = None;
                    UiEffect::Redraw
                }
                _ => UiEffect::None,
            },
            Some(Overlay::CommandPalette) => match key.code {
                KeyCode::Enter => {
                    self.overlay = None;
                    self.submit_composer(snapshot)
                }
                KeyCode::Backspace => {
                    self.composer.pop();
                    UiEffect::Redraw
                }
                KeyCode::Char(character)
                    if !character.is_control()
                        && self.composer.len() + character.len_utf8() <= 4_096 =>
                {
                    self.composer.push(character);
                    UiEffect::Redraw
                }
                _ => UiEffect::None,
            },
            Some(Overlay::Overview | Overlay::FullLog | Overlay::Help | Overlay::Inspector) => {
                UiEffect::None
            }
            None => UiEffect::None,
        }
    }

    fn move_task_selection(&mut self, snapshot: &WorkspaceSnapshot, forward: bool) {
        if snapshot.tasks.is_empty() {
            self.selected_task = None;
            self.selected_task_index = None;
            return;
        }
        let current = self.selected_task_index.unwrap_or(if forward {
            snapshot.tasks.len().saturating_sub(1)
        } else {
            0
        });
        let index = if forward {
            (current + 1) % snapshot.tasks.len()
        } else {
            current
                .checked_sub(1)
                .unwrap_or(snapshot.tasks.len().saturating_sub(1))
        };
        self.selected_task_index = Some(index);
        self.selected_task = snapshot.tasks.get(index).map(|task| task.task_id.clone());
    }

    fn submit_composer(&mut self, snapshot: &WorkspaceSnapshot) -> UiEffect {
        let value = self.composer.trim();
        if value.starts_with('/') {
            let effect = parse_palette_command(value).map_or_else(
                || UiEffect::Feedback(ActionFeedback::info("unknown command palette action")),
                |command| self.palette_effect(command, snapshot),
            );
            self.composer.clear();
            return effect;
        }
        if snapshot.read_only_reason.is_some() {
            return UiEffect::Feedback(ActionFeedback::info(
                snapshot
                    .read_only_reason
                    .as_deref()
                    .unwrap_or("workspace is read-only"),
            ));
        }
        let Some((target, content)) = parse_submission(value, &self.composer_target) else {
            return UiEffect::Feedback(ActionFeedback::info("message must not be blank"));
        };
        self.composer.clear();
        UiEffect::Dispatch(WorkspaceAction::SubmitMessage { target, content })
    }

    fn palette_effect(
        &mut self,
        command: PaletteCommand,
        snapshot: &WorkspaceSnapshot,
    ) -> UiEffect {
        match command {
            PaletteCommand::Tasks => {
                self.overlay = Some(Overlay::TaskSwitcher);
                UiEffect::Redraw
            }
            PaletteCommand::Plan => UiEffect::Feedback(ActionFeedback::unavailable("planning")),
            PaletteCommand::Approve => {
                UiEffect::Feedback(ActionFeedback::unavailable("graph approval"))
            }
            PaletteCommand::Pause => self.task_control(snapshot, TaskControlIntent::Pause),
            PaletteCommand::Resume => self.task_control(snapshot, TaskControlIntent::Resume),
            PaletteCommand::Cancel => self.task_control(snapshot, TaskControlIntent::Cancel),
            PaletteCommand::Handover | PaletteCommand::Provider | PaletteCommand::Admin => {
                UiEffect::Dispatch(WorkspaceAction::OpenAdministration)
            }
            PaletteCommand::Retry => self.task_control(snapshot, TaskControlIntent::Retry),
            PaletteCommand::Checkpoint => {
                self.task_control(snapshot, TaskControlIntent::Checkpoint)
            }
        }
    }

    fn task_control(&self, snapshot: &WorkspaceSnapshot, intent: TaskControlIntent) -> UiEffect {
        if let Some(reason) = snapshot.read_only_reason.as_deref() {
            return UiEffect::Feedback(ActionFeedback::info(reason));
        }
        self.selected_task.as_ref().map_or_else(
            || UiEffect::Feedback(ActionFeedback::info("select a task first")),
            |task_id| {
                UiEffect::Dispatch(WorkspaceAction::RequestTaskControl {
                    task_id: task_id.clone(),
                    intent,
                })
            },
        )
    }
}

const fn next_focus(focus: FocusPane, forward: bool) -> FocusPane {
    match (focus, forward) {
        (FocusPane::Tasks, true) | (FocusPane::Conversation, false) => FocusPane::Conversation,
        (FocusPane::Conversation, true) | (FocusPane::Inspector, false) => FocusPane::Inspector,
        (FocusPane::Inspector, true) | (FocusPane::Composer, false) => FocusPane::Composer,
        (FocusPane::Composer, true) | (FocusPane::Tasks, false) => FocusPane::Tasks,
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::{FocusPane, Overlay, UiEffect, WorkspaceState};
    use crate::chat::{
        ComposerTarget, LayoutMode, TaskControlIntent, TaskSummary, WorkspaceAction,
        WorkspaceSnapshot,
    };

    fn snapshot() -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            repository: "colay".to_owned(),
            session_id: "session-01".to_owned(),
            session_title: "Auth refactor".to_owned(),
            session_state: "running".to_owned(),
            tasks: ["task-01", "task-02", "task-03"]
                .into_iter()
                .map(|task_id| TaskSummary {
                    task_id: task_id.to_owned(),
                    title: task_id.to_owned(),
                    state: "running".to_owned(),
                    state_symbol: "*".to_owned(),
                    dependency_status: "ready".to_owned(),
                    needs_attention: false,
                })
                .collect(),
            ..WorkspaceSnapshot::default()
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn task_selection_never_retargets_composer() {
        let mut state = WorkspaceState::default();
        state.select_task(Some("task-03".to_owned()));
        assert_eq!(state.selected_task(), Some("task-03"));
        assert_eq!(state.composer_target(), &ComposerTarget::Orchestrator);
    }

    #[test]
    fn target_changes_only_after_explicit_target_picker_selection() {
        let mut state = WorkspaceState::default();
        let snapshot = snapshot();
        assert_eq!(
            state.handle_key(
                KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
                &snapshot,
                LayoutMode::Wide,
            ),
            UiEffect::Redraw
        );
        assert_eq!(state.overlay(), Some(&Overlay::TargetPicker));
        state.handle_key(key(KeyCode::Char('3')), &snapshot, LayoutMode::Wide);
        assert_eq!(
            state.composer_target(),
            &ComposerTarget::Task("task-03".to_owned())
        );
    }

    #[test]
    fn mention_submission_does_not_mutate_persistent_target() {
        let mut state = WorkspaceState::default();
        let snapshot = snapshot();
        state.set_focus(FocusPane::Composer);
        state.set_composer("@task-03 fix tests");
        assert_eq!(
            state.handle_key(key(KeyCode::Enter), &snapshot, LayoutMode::Wide),
            UiEffect::Dispatch(WorkspaceAction::SubmitMessage {
                target: ComposerTarget::Task("task-03".to_owned()),
                content: "fix tests".to_owned(),
            })
        );
        assert_eq!(state.composer_target(), &ComposerTarget::Orchestrator);
    }

    #[test]
    fn command_palette_accepts_text_until_enter() {
        let mut state = WorkspaceState::default();
        let snapshot = snapshot();
        assert_eq!(
            state.handle_key(key(KeyCode::Char('/')), &snapshot, LayoutMode::Wide),
            UiEffect::Redraw
        );
        for character in "admin".chars() {
            assert_eq!(
                state.handle_key(key(KeyCode::Char(character)), &snapshot, LayoutMode::Wide),
                UiEffect::Redraw
            );
        }
        assert_eq!(state.composer(), "/admin");
        assert_eq!(
            state.handle_key(key(KeyCode::Enter), &snapshot, LayoutMode::Wide),
            UiEffect::Dispatch(WorkspaceAction::OpenAdministration)
        );
    }

    #[test]
    fn compact_mode_blocks_mutation_but_allows_quit() {
        let mut state = WorkspaceState::default();
        let snapshot = snapshot();
        state.set_focus(FocusPane::Composer);
        state.set_composer("run it");
        assert!(matches!(
            state.handle_key(key(KeyCode::Enter), &snapshot, LayoutMode::TooSmall),
            UiEffect::Feedback(_)
        ));
        assert_eq!(
            state.handle_key(key(KeyCode::Char('q')), &snapshot, LayoutMode::TooSmall),
            UiEffect::Dispatch(WorkspaceAction::Quit)
        );
    }

    #[test]
    fn command_palette_maps_current_controls_and_marks_later_phases_unavailable() {
        let mut state = WorkspaceState::default();
        let snapshot = snapshot();
        state.select_task(Some("task-02".to_owned()));
        state.set_focus(FocusPane::Composer);
        state.set_composer("/pause");
        assert_eq!(
            state.handle_key(key(KeyCode::Enter), &snapshot, LayoutMode::Wide),
            UiEffect::Dispatch(WorkspaceAction::RequestTaskControl {
                task_id: "task-02".to_owned(),
                intent: TaskControlIntent::Pause,
            })
        );
        state.set_composer("/plan");
        assert!(matches!(
            state.handle_key(key(KeyCode::Enter), &snapshot, LayoutMode::Wide),
            UiEffect::Feedback(_)
        ));
    }

    #[test]
    fn focus_and_overlay_bindings_follow_workspace_contract() {
        let mut state = WorkspaceState::default();
        let snapshot = snapshot();
        state.handle_key(key(KeyCode::Tab), &snapshot, LayoutMode::Wide);
        assert_eq!(state.focus(), FocusPane::Inspector);
        state.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            &snapshot,
            LayoutMode::Wide,
        );
        assert_eq!(state.overlay(), Some(&Overlay::TaskSwitcher));
        state.handle_key(key(KeyCode::Esc), &snapshot, LayoutMode::Wide);
        assert_eq!(state.overlay(), None);
    }
}
