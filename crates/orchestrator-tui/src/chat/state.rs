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
    ApprovalConfirmation {
        revision_id: String,
        proposal_hash: String,
    },
    IntegrationApprovalConfirmation {
        batch_id: String,
        preview_hash: String,
    },
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
        if let Some(Overlay::ApprovalConfirmation {
            revision_id,
            proposal_hash,
        }) = self.overlay.as_ref()
            && !snapshot.plan_approval.as_ref().is_some_and(|plan| {
                plan.revision_id == *revision_id && plan.proposal_hash == *proposal_hash
            })
        {
            self.overlay = None;
            self.feedback = Some(ActionFeedback::warning(
                "plan changed; reopen /approve to review the current revision",
            ));
        }
        if let Some(Overlay::IntegrationApprovalConfirmation {
            batch_id,
            preview_hash,
        }) = self.overlay.as_ref()
            && !snapshot.integration_approval.as_ref().is_some_and(|card| {
                card.approvable && card.batch_id == *batch_id && card.preview_hash == *preview_hash
            })
        {
            self.overlay = None;
            self.feedback = Some(ActionFeedback::warning(
                "integration preview changed; reopen /approve to review the current preview",
            ));
        }
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
            Some(Overlay::ApprovalConfirmation {
                ref revision_id,
                ref proposal_hash,
            }) => match key.code {
                KeyCode::Char('y' | 'Y') => {
                    let action = WorkspaceAction::ApproveGraph {
                        revision_id: revision_id.clone(),
                        proposal_hash: proposal_hash.clone(),
                        approved_by: "local-tui".to_owned(),
                    };
                    self.overlay = None;
                    UiEffect::Dispatch(action)
                }
                KeyCode::Char('n' | 'N') => {
                    self.overlay = None;
                    UiEffect::Redraw
                }
                _ => UiEffect::None,
            },
            Some(Overlay::IntegrationApprovalConfirmation {
                ref batch_id,
                ref preview_hash,
            }) => match key.code {
                KeyCode::Char('y' | 'Y') => {
                    let action = WorkspaceAction::ApproveIntegration {
                        batch_id: batch_id.clone(),
                        preview_hash: preview_hash.clone(),
                        approved_by: "local-tui".to_owned(),
                    };
                    self.overlay = None;
                    UiEffect::Dispatch(action)
                }
                KeyCode::Char('n' | 'N') => {
                    self.overlay = None;
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
            PaletteCommand::Plan => Self::request_plan(snapshot),
            PaletteCommand::Integrate => Self::request_integration(snapshot),
            PaletteCommand::Approve => self.request_approval(snapshot),
            PaletteCommand::Resolve => Self::request_resolution(snapshot),
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

    fn request_plan(snapshot: &WorkspaceSnapshot) -> UiEffect {
        if let Some(reason) = snapshot.read_only_reason.as_deref() {
            return UiEffect::Feedback(ActionFeedback::info(reason));
        }
        snapshot
            .messages
            .iter()
            .rev()
            .find(|message| {
                message.role == "user"
                    && message.kind == "user_message"
                    && message.state == "final"
                    && message.task_id.is_none()
            })
            .map_or_else(
                || {
                    UiEffect::Feedback(ActionFeedback::info(
                        "send a session-level user goal before requesting a plan",
                    ))
                },
                |message| {
                    UiEffect::Dispatch(WorkspaceAction::RequestPlan {
                        goal_message_id: message.message_id.clone(),
                    })
                },
            )
    }

    fn request_approval(&mut self, snapshot: &WorkspaceSnapshot) -> UiEffect {
        if let Some(reason) = snapshot.read_only_reason.as_deref() {
            return UiEffect::Feedback(ActionFeedback::info(reason));
        }
        if let Some(plan) = snapshot.plan_approval.as_ref() {
            self.overlay = Some(Overlay::ApprovalConfirmation {
                revision_id: plan.revision_id.clone(),
                proposal_hash: plan.proposal_hash.clone(),
            });
            return UiEffect::Redraw;
        }
        if let Some(card) = snapshot.integration_approval.as_ref() {
            if !card.approvable {
                return UiEffect::Feedback(ActionFeedback::info(
                    "integration preview is blocked; use /resolve after reviewing blockers",
                ));
            }
            self.overlay = Some(Overlay::IntegrationApprovalConfirmation {
                batch_id: card.batch_id.clone(),
                preview_hash: card.preview_hash.clone(),
            });
            return UiEffect::Redraw;
        }
        UiEffect::Feedback(ActionFeedback::info(
            "no exact task graph or integration preview is awaiting approval",
        ))
    }

    fn request_integration(snapshot: &WorkspaceSnapshot) -> UiEffect {
        if let Some(reason) = snapshot.read_only_reason.as_deref() {
            return UiEffect::Feedback(ActionFeedback::info(reason));
        }
        UiEffect::Dispatch(WorkspaceAction::RequestIntegration)
    }

    fn request_resolution(snapshot: &WorkspaceSnapshot) -> UiEffect {
        if let Some(reason) = snapshot.read_only_reason.as_deref() {
            return UiEffect::Feedback(ActionFeedback::info(reason));
        }
        snapshot.integration_approval.as_ref().map_or_else(
            || UiEffect::Feedback(ActionFeedback::info("no integration preview is available")),
            |card| {
                if card.blockers.is_empty() {
                    UiEffect::Feedback(ActionFeedback::info(
                        "the current integration preview has no blockers",
                    ))
                } else {
                    UiEffect::Dispatch(WorkspaceAction::CreateResolutionTask {
                        batch_id: card.batch_id.clone(),
                    })
                }
            },
        )
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
        ComposerTarget, DaemonConnectivity, IntegrationApprovalCard, IntegrationSourceSummary,
        LayoutMode, PlanApprovalCard, PlanNodeSummary, TaskControlIntent, TaskSummary,
        TimelineEntry, WorkspaceAction, WorkspaceSnapshot,
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

    fn planning_snapshot() -> WorkspaceSnapshot {
        let mut snapshot = snapshot();
        snapshot.daemon = DaemonConnectivity::Online;
        snapshot.messages = vec![
            TimelineEntry {
                ordinal: 1,
                message_id: "old-goal".to_owned(),
                role: "user".to_owned(),
                kind: "user_message".to_owned(),
                state: "final".to_owned(),
                content: "old".to_owned(),
                ..TimelineEntry::default()
            },
            TimelineEntry {
                ordinal: 2,
                message_id: "task-message".to_owned(),
                task_id: Some("task-01".to_owned()),
                role: "user".to_owned(),
                kind: "user_message".to_owned(),
                state: "final".to_owned(),
                content: "task instruction".to_owned(),
                ..TimelineEntry::default()
            },
            TimelineEntry {
                ordinal: 3,
                message_id: "new-goal".to_owned(),
                role: "user".to_owned(),
                kind: "user_message".to_owned(),
                state: "final".to_owned(),
                content: "new".to_owned(),
                ..TimelineEntry::default()
            },
        ];
        snapshot.plan_approval = Some(PlanApprovalCard {
            revision_id: "revision-01".to_owned(),
            proposal_hash: "a".repeat(64),
            nodes: vec![PlanNodeSummary {
                key: "domain".to_owned(),
                title: "Domain".to_owned(),
                objective: "Implement domain".to_owned(),
                provider: "codex".to_owned(),
                profile: "standard".to_owned(),
                write_scopes: vec!["src/domain".to_owned()],
                parallel_safety: "isolated".to_owned(),
                ..PlanNodeSummary::default()
            }],
            proposed_parallelism: 1,
            risks: vec!["concurrency".to_owned()],
        });
        snapshot
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
    fn command_palette_maps_current_controls_and_planning() {
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
        assert_eq!(
            state.handle_key(key(KeyCode::Enter), &snapshot, LayoutMode::Wide),
            UiEffect::Feedback(crate::chat::ActionFeedback::info(
                "send a session-level user goal before requesting a plan"
            ))
        );
    }

    #[test]
    fn plan_uses_newest_session_goal_and_approval_requires_exact_yes() {
        let snapshot = planning_snapshot();
        let mut state = WorkspaceState::default();
        state.set_focus(FocusPane::Composer);
        state.set_composer("/plan");
        assert_eq!(
            state.handle_key(key(KeyCode::Enter), &snapshot, LayoutMode::Wide),
            UiEffect::Dispatch(WorkspaceAction::RequestPlan {
                goal_message_id: "new-goal".to_owned(),
            })
        );

        state.set_composer("/approve");
        assert_eq!(
            state.handle_key(key(KeyCode::Enter), &snapshot, LayoutMode::Wide),
            UiEffect::Redraw
        );
        assert_eq!(
            state.overlay(),
            Some(&Overlay::ApprovalConfirmation {
                revision_id: "revision-01".to_owned(),
                proposal_hash: "a".repeat(64),
            })
        );
        assert_eq!(
            state.handle_key(key(KeyCode::Char('x')), &snapshot, LayoutMode::Wide),
            UiEffect::None
        );
        assert_eq!(
            state.handle_key(key(KeyCode::Char('y')), &snapshot, LayoutMode::Wide),
            UiEffect::Dispatch(WorkspaceAction::ApproveGraph {
                revision_id: "revision-01".to_owned(),
                proposal_hash: "a".repeat(64),
                approved_by: "local-tui".to_owned(),
            })
        );
        assert_eq!(state.overlay(), None);
    }

    #[test]
    fn approval_cancels_and_stale_or_read_only_cards_fail_closed() {
        let snapshot = planning_snapshot();
        let mut state = WorkspaceState::default();
        state.set_focus(FocusPane::Composer);
        state.set_composer("/approve");
        state.handle_key(key(KeyCode::Enter), &snapshot, LayoutMode::Wide);
        assert_eq!(
            state.handle_key(key(KeyCode::Char('n')), &snapshot, LayoutMode::Wide),
            UiEffect::Redraw
        );
        assert_eq!(state.overlay(), None);

        state.set_composer("/approve");
        state.handle_key(key(KeyCode::Enter), &snapshot, LayoutMode::Wide);
        let mut changed = snapshot.clone();
        if let Some(plan) = changed.plan_approval.as_mut() {
            plan.proposal_hash = "b".repeat(64);
        }
        state.reconcile_snapshot(&changed);
        assert_eq!(state.overlay(), None);
        assert!(
            state
                .feedback()
                .is_some_and(|feedback| feedback.message.contains("plan changed"))
        );

        let mut offline = snapshot.clone();
        offline.read_only_reason = Some("daemon offline".to_owned());
        state.set_composer("/approve");
        assert!(matches!(
            state.handle_key(key(KeyCode::Enter), &offline, LayoutMode::Wide),
            UiEffect::Feedback(_)
        ));
        let mut invalid = snapshot;
        invalid.plan_approval = None;
        state.set_composer("/approve");
        assert!(matches!(
            state.handle_key(key(KeyCode::Enter), &invalid, LayoutMode::Wide),
            UiEffect::Feedback(_)
        ));
    }

    #[test]
    fn integration_preview_approval_and_resolution_are_explicit() {
        let mut snapshot = snapshot();
        snapshot.daemon = DaemonConnectivity::Online;
        snapshot.integration_approval = Some(IntegrationApprovalCard {
            batch_id: "batch-01".to_owned(),
            preview_hash: "c".repeat(64),
            base_revision: "d".repeat(40),
            destination: ".colay/integration/batch-01".to_owned(),
            sources: vec![IntegrationSourceSummary {
                task_id: "task-01".to_owned(),
                checkpoint_id: "checkpoint-01".to_owned(),
                verification_id: "verification-01".to_owned(),
                diff_sha256: "e".repeat(64),
                changed_files: vec!["src/lib.rs".to_owned()],
            }],
            blockers: Vec::new(),
            approvable: true,
        });
        let mut state = WorkspaceState::default();
        state.set_focus(FocusPane::Composer);

        state.set_composer("/integrate");
        assert_eq!(
            state.handle_key(key(KeyCode::Enter), &snapshot, LayoutMode::Wide),
            UiEffect::Dispatch(WorkspaceAction::RequestIntegration)
        );
        state.set_composer("/approve");
        assert_eq!(
            state.handle_key(key(KeyCode::Enter), &snapshot, LayoutMode::Wide),
            UiEffect::Redraw
        );
        assert_eq!(
            state.overlay(),
            Some(&Overlay::IntegrationApprovalConfirmation {
                batch_id: "batch-01".to_owned(),
                preview_hash: "c".repeat(64),
            })
        );
        assert_eq!(
            state.handle_key(key(KeyCode::Char('y')), &snapshot, LayoutMode::Wide),
            UiEffect::Dispatch(WorkspaceAction::ApproveIntegration {
                batch_id: "batch-01".to_owned(),
                preview_hash: "c".repeat(64),
                approved_by: "local-tui".to_owned(),
            })
        );

        snapshot
            .integration_approval
            .as_mut()
            .expect("card")
            .blockers = vec!["path overlap: src/lib.rs".to_owned()];
        snapshot
            .integration_approval
            .as_mut()
            .expect("card")
            .approvable = false;
        state.set_composer("/resolve");
        assert_eq!(
            state.handle_key(key(KeyCode::Enter), &snapshot, LayoutMode::Wide),
            UiEffect::Dispatch(WorkspaceAction::CreateResolutionTask {
                batch_id: "batch-01".to_owned(),
            })
        );
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
