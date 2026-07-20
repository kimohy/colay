//! Terminal dashboard for orchestrator status.

pub mod chat;

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Stable UI input model. It deliberately contains presentation-safe strings only.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct DashboardSnapshot {
    pub task: TaskPanel,
    pub providers: Vec<ProviderRow>,
    pub routing: RoutingPanel,
    pub handover: HandoverPanel,
    pub verification: VerificationPanel,
    pub controls: ControlContext,
    /// Every provider present in configuration, including disabled providers.
    /// Control pickers deliberately use this list instead of inferring a default
    /// from usage or routing rows.
    #[serde(default)]
    pub provider_controls: Vec<ProviderControlOption>,
    /// Provider-specific drafts. Values from one quota scope are never silently
    /// reused for a different provider.
    #[serde(default)]
    pub usage_override_drafts: Vec<UsageOverrideDraft>,
    /// Effective provider/profile mappings with preset/customized classification.
    #[serde(default)]
    pub model_profiles: Vec<ModelProfileRow>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ModelProfileRow {
    pub provider: String,
    pub profile: String,
    pub model: String,
    pub effort: String,
    pub description: String,
    pub customized: bool,
}

/// A configured provider and its current administrative enablement state.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ProviderControlOption {
    pub provider: String,
    pub enabled: bool,
}

/// Control values supplied by the caller.
///
/// The optional selection fields are retained for serialized snapshot compatibility,
/// but interactive provider actions require a fresh explicit picker selection.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct ControlContext {
    pub automatic_routing_enabled: bool,
    pub manual_provider: Option<String>,
    pub handover_target: Option<String>,
    pub usage_override: Option<UsageOverrideDraft>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct UsageOverrideDraft {
    pub provider: String,
    pub used: Option<f64>,
    pub limit: Option<f64>,
    pub remaining: Option<f64>,
    pub entered_by: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct TaskPanel {
    pub id: String,
    pub objective: String,
    pub state: String,
    pub difficulty: String,
    pub risks: Vec<String>,
    pub phase: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ProviderRow {
    pub provider: String,
    pub usage: String,
    pub reset: String,
    pub source: String,
    pub confidence: String,
    pub health: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct RoutingPanel {
    pub selected: String,
    pub profile: String,
    pub effort: String,
    pub rationale: Vec<String>,
    pub alternatives: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct HandoverPanel {
    pub previous_provider: String,
    pub next_provider: String,
    pub reason: String,
    pub checkpoint: String,
    pub count: u32,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct VerificationPanel {
    pub changed_files: usize,
    pub tests: Vec<String>,
    pub failures: Vec<String>,
    pub approval_required: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ControlAction {
    SetAutomaticRouting {
        enabled: bool,
    },
    SelectProvider {
        task_id: String,
        provider: String,
    },
    SetProviderEnabled {
        provider: String,
        enabled: bool,
    },
    Pause {
        task_id: String,
    },
    Resume {
        task_id: String,
    },
    Cancel {
        task_id: String,
    },
    Handover {
        task_id: String,
        to_provider: String,
    },
    UsageOverride {
        provider: String,
        used: Option<f64>,
        limit: Option<f64>,
        remaining: Option<f64>,
        entered_by: String,
    },
    SetModelProfile {
        provider: String,
        profile: String,
        model: String,
        effort: String,
    },
    ResetModelProfile {
        provider: String,
        profile: String,
    },
    Quit,
}

#[derive(Debug, Error)]
pub enum TuiError {
    #[error("terminal I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("control input is incomplete or invalid: {0}")]
    InvalidControlInput(String),
}

trait TerminalControl {
    fn enable_raw_mode(&mut self) -> Result<(), io::Error>;
    fn disable_raw_mode(&mut self) -> Result<(), io::Error>;
    fn enter_alternate_screen(&mut self) -> Result<(), io::Error>;
    fn leave_alternate_screen(&mut self) -> Result<(), io::Error>;
}

struct CrosstermTerminalControl;

impl TerminalControl for CrosstermTerminalControl {
    fn enable_raw_mode(&mut self) -> Result<(), io::Error> {
        enable_raw_mode()
    }

    fn disable_raw_mode(&mut self) -> Result<(), io::Error> {
        disable_raw_mode()
    }

    fn enter_alternate_screen(&mut self) -> Result<(), io::Error> {
        execute!(io::stdout(), EnterAlternateScreen)
    }

    fn leave_alternate_screen(&mut self) -> Result<(), io::Error> {
        execute!(io::stdout(), LeaveAlternateScreen)
    }
}

struct TerminalGuard<C: TerminalControl> {
    control: C,
    raw_mode_enabled: bool,
    alternate_screen_entered: bool,
}

impl<C: TerminalControl> TerminalGuard<C> {
    fn enter(control: C) -> Result<Self, io::Error> {
        let mut guard = Self {
            control,
            raw_mode_enabled: false,
            alternate_screen_entered: false,
        };
        guard.control.enable_raw_mode()?;
        guard.raw_mode_enabled = true;
        guard.control.enter_alternate_screen()?;
        guard.alternate_screen_entered = true;
        Ok(guard)
    }
}

impl<C: TerminalControl> Drop for TerminalGuard<C> {
    fn drop(&mut self) {
        if self.alternate_screen_entered {
            let _ = self.control.leave_alternate_screen();
        }
        if self.raw_mode_enabled {
            let _ = self.control.disable_raw_mode();
        }
    }
}

/// Runs a read-mostly dashboard and returns the first requested control action.
///
/// # Errors
///
/// Returns [`TuiError::Io`] when terminal setup, drawing, or event input fails, and
/// [`TuiError::InvalidControlInput`] when a requested action has no complete payload.
pub fn run(snapshot: &DashboardSnapshot) -> Result<ControlAction, TuiError> {
    let _guard = TerminalGuard::enter(CrosstermTerminalControl)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let mut state = InteractionState::default();
    loop {
        terminal.draw(|frame| render_interactive(frame, snapshot, &state))?;
        if event::poll(Duration::from_millis(250))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && let Some(action) = action_for_key(key.code, snapshot, &mut state)?
        {
            return Ok(action);
        }
    }
}

fn action_for_key(
    code: KeyCode,
    snapshot: &DashboardSnapshot,
    state: &mut InteractionState,
) -> Result<Option<ControlAction>, TuiError> {
    if state.profile_reset_confirmation.is_some() {
        return Ok(profile_reset_confirmation_action(code, state));
    }
    if state.profile_editor.is_some() {
        return Ok(profile_editor_action(code, state));
    }
    if state.profile_list.is_some() {
        return profile_list_action(code, snapshot, state);
    }
    if state.usage_editor.is_some() {
        return Ok(usage_editor_action(code, state));
    }
    if state.picker.is_some() {
        return picker_action(code, snapshot, state);
    }
    let action = match code {
        KeyCode::Char('a') => Some(ControlAction::SetAutomaticRouting {
            enabled: !snapshot.controls.automatic_routing_enabled,
        }),
        KeyCode::Char('m') => {
            open_picker(state, PickerPurpose::ManualProvider, snapshot);
            None
        }
        KeyCode::Char('e') => {
            open_picker(state, PickerPurpose::ToggleEnabled, snapshot);
            None
        }
        KeyCode::Char('p') => Some(ControlAction::Pause {
            task_id: required_task_id(snapshot)?,
        }),
        KeyCode::Char('r') => Some(ControlAction::Resume {
            task_id: required_task_id(snapshot)?,
        }),
        KeyCode::Char('c') => Some(ControlAction::Cancel {
            task_id: required_task_id(snapshot)?,
        }),
        KeyCode::Char('h') => {
            open_picker(state, PickerPurpose::Handover, snapshot);
            None
        }
        KeyCode::Char('u') => {
            open_picker(state, PickerPurpose::UsageOverride, snapshot);
            None
        }
        KeyCode::Char('f') => {
            if snapshot.model_profiles.is_empty() {
                state.feedback = Some("no configured model profiles".to_owned());
            } else {
                state.profile_list = Some(ProfileListState::default());
                state.feedback = Some("select a provider profile explicitly".to_owned());
            }
            None
        }
        KeyCode::Char('q') | KeyCode::Esc => Some(ControlAction::Quit),
        _ => None,
    };
    Ok(action)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PickerPurpose {
    ManualProvider,
    ToggleEnabled,
    Handover,
    UsageOverride,
}

#[derive(Clone, Debug)]
struct PickerState {
    purpose: PickerPurpose,
    selected: Option<usize>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum UsageEditorField {
    #[default]
    Used,
    Limit,
    Remaining,
    Confirm,
}

#[derive(Clone, Debug)]
struct UsageEditorState {
    provider: String,
    used: String,
    limit: String,
    remaining: String,
    entered_by: String,
    field: UsageEditorField,
}

#[derive(Clone, Debug, Default)]
struct ProfileListState {
    selected: Option<usize>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ProfileEditorField {
    #[default]
    Model,
    Effort,
    Confirm,
}

#[derive(Clone, Debug)]
struct ProfileEditorState {
    provider: String,
    profile: String,
    model: String,
    effort: String,
    field: ProfileEditorField,
}

#[derive(Clone, Debug)]
struct ProfileResetConfirmation {
    provider: String,
    profile: String,
}

impl UsageEditorState {
    fn from_draft(draft: &UsageOverrideDraft) -> Self {
        Self {
            provider: draft.provider.clone(),
            used: display_optional_number(draft.used),
            limit: display_optional_number(draft.limit),
            remaining: display_optional_number(draft.remaining),
            entered_by: draft.entered_by.clone(),
            field: UsageEditorField::default(),
        }
    }

    fn selected_value_mut(&mut self) -> Option<&mut String> {
        match self.field {
            UsageEditorField::Used => Some(&mut self.used),
            UsageEditorField::Limit => Some(&mut self.limit),
            UsageEditorField::Remaining => Some(&mut self.remaining),
            UsageEditorField::Confirm => None,
        }
    }

    fn draft(&self) -> Result<UsageOverrideDraft, TuiError> {
        Ok(UsageOverrideDraft {
            provider: self.provider.clone(),
            used: parse_optional_number(&self.used, "used")?,
            limit: parse_optional_number(&self.limit, "limit")?,
            remaining: parse_optional_number(&self.remaining, "remaining")?,
            entered_by: self.entered_by.clone(),
        })
    }
}

#[derive(Clone, Debug, Default)]
struct InteractionState {
    picker: Option<PickerState>,
    usage_editor: Option<UsageEditorState>,
    profile_list: Option<ProfileListState>,
    profile_editor: Option<ProfileEditorState>,
    profile_reset_confirmation: Option<ProfileResetConfirmation>,
    feedback: Option<String>,
}

fn profile_list_action(
    code: KeyCode,
    snapshot: &DashboardSnapshot,
    state: &mut InteractionState,
) -> Result<Option<ControlAction>, TuiError> {
    let Some(list) = state.profile_list.as_mut() else {
        return Ok(None);
    };
    match code {
        KeyCode::Esc => {
            state.profile_list = None;
            state.feedback = None;
        }
        KeyCode::Down | KeyCode::Char('j') => {
            list.selected = Some(next_index(
                list.selected,
                snapshot.model_profiles.len(),
                true,
            ));
        }
        KeyCode::Up | KeyCode::Char('k') => {
            list.selected = Some(next_index(
                list.selected,
                snapshot.model_profiles.len(),
                false,
            ));
        }
        KeyCode::Char(digit) if digit.is_ascii_digit() && digit != '0' => {
            let index = usize::try_from(digit.to_digit(10).unwrap_or_default())
                .unwrap_or_default()
                .saturating_sub(1);
            if index < snapshot.model_profiles.len() {
                list.selected = Some(index);
            }
        }
        KeyCode::Enter => {
            let Some(index) = list.selected else {
                state.feedback = Some("choose a profile before editing".to_owned());
                return Ok(None);
            };
            let row = snapshot
                .model_profiles
                .get(index)
                .ok_or_else(|| invalid_control("selected model profile is no longer available"))?;
            state.profile_editor = Some(ProfileEditorState {
                provider: row.provider.clone(),
                profile: row.profile.clone(),
                model: row.model.clone(),
                effort: row.effort.clone(),
                field: ProfileEditorField::default(),
            });
            state.feedback = Some("edit Model/Effort, then confirm".to_owned());
        }
        KeyCode::Delete => {
            let Some(index) = list.selected else {
                state.feedback = Some("choose a profile before resetting".to_owned());
                return Ok(None);
            };
            let row = snapshot
                .model_profiles
                .get(index)
                .ok_or_else(|| invalid_control("selected model profile is no longer available"))?;
            if row.customized {
                state.profile_reset_confirmation = Some(ProfileResetConfirmation {
                    provider: row.provider.clone(),
                    profile: row.profile.clone(),
                });
                state.feedback = Some("Reset override? y/N".to_owned());
            } else {
                state.feedback =
                    Some("selected profile already uses the built-in preset".to_owned());
            }
        }
        _ => {}
    }
    Ok(None)
}

fn profile_reset_confirmation_action(
    code: KeyCode,
    state: &mut InteractionState,
) -> Option<ControlAction> {
    match code {
        KeyCode::Char('y' | 'Y') => {
            let target = state.profile_reset_confirmation.take()?;
            state.profile_list = None;
            state.feedback = None;
            Some(ControlAction::ResetModelProfile {
                provider: target.provider,
                profile: target.profile,
            })
        }
        KeyCode::Char('n' | 'N') | KeyCode::Esc | KeyCode::Enter => {
            state.profile_reset_confirmation = None;
            state.feedback = Some("profile reset cancelled".to_owned());
            None
        }
        _ => None,
    }
}

fn profile_editor_action(code: KeyCode, state: &mut InteractionState) -> Option<ControlAction> {
    let mut submitted = None;
    let editor = state.profile_editor.as_mut()?;
    match code {
        KeyCode::Esc => {
            state.profile_editor = None;
            state.feedback = Some("profile edit cancelled".to_owned());
        }
        KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
            editor.field = next_profile_field(editor.field, true);
        }
        KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => {
            editor.field = next_profile_field(editor.field, false);
        }
        KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
            if editor.field == ProfileEditorField::Effort =>
        {
            editor.effort = cycle_effort(&editor.effort, code != KeyCode::Left).to_owned();
        }
        KeyCode::Backspace if editor.field == ProfileEditorField::Model => {
            editor.model.pop();
        }
        KeyCode::Delete if editor.field == ProfileEditorField::Model => {
            editor.model.clear();
        }
        KeyCode::Home | KeyCode::End if editor.field == ProfileEditorField::Model => {}
        KeyCode::Char(character)
            if editor.field == ProfileEditorField::Model
                && !character.is_control()
                && editor.model.len() + character.len_utf8() <= 256 =>
        {
            editor.model.push(character);
        }
        KeyCode::Enter if editor.field == ProfileEditorField::Confirm => {
            let model = editor.model.trim();
            if model.is_empty() {
                state.feedback = Some("model must not be blank".to_owned());
                return None;
            }
            if !matches!(editor.effort.as_str(), "low" | "medium" | "high") {
                state.feedback = Some("effort must be low, medium, or high".to_owned());
                return None;
            }
            submitted = Some(ControlAction::SetModelProfile {
                provider: editor.provider.clone(),
                profile: editor.profile.clone(),
                model: model.to_owned(),
                effort: editor.effort.clone(),
            });
        }
        KeyCode::Enter => {
            editor.field = next_profile_field(editor.field, true);
        }
        _ => {}
    }
    if submitted.is_some() {
        state.profile_editor = None;
        state.profile_list = None;
        state.feedback = None;
    }
    submitted
}

const fn next_profile_field(field: ProfileEditorField, forward: bool) -> ProfileEditorField {
    match (field, forward) {
        (ProfileEditorField::Model, true) | (ProfileEditorField::Confirm, false) => {
            ProfileEditorField::Effort
        }
        (ProfileEditorField::Effort, true) | (ProfileEditorField::Model, false) => {
            ProfileEditorField::Confirm
        }
        (ProfileEditorField::Confirm, true) | (ProfileEditorField::Effort, false) => {
            ProfileEditorField::Model
        }
    }
}

fn cycle_effort(current: &str, forward: bool) -> &'static str {
    match (current, forward) {
        ("medium", true) | ("low", false) => "high",
        ("high", true) | ("medium", false) => "low",
        (_, _) => "medium",
    }
}

fn open_picker(state: &mut InteractionState, purpose: PickerPurpose, snapshot: &DashboardSnapshot) {
    let candidates = picker_candidates(snapshot, purpose);
    if candidates.is_empty() {
        state.feedback = Some("no eligible configured provider".to_owned());
    } else {
        state.picker = Some(PickerState {
            purpose,
            selected: None,
        });
        state.feedback = Some("select a provider explicitly, then press Enter".to_owned());
    }
}

fn picker_action(
    code: KeyCode,
    snapshot: &DashboardSnapshot,
    state: &mut InteractionState,
) -> Result<Option<ControlAction>, TuiError> {
    let Some(picker) = state.picker.as_mut() else {
        return Ok(None);
    };
    let candidates = picker_candidates(snapshot, picker.purpose);
    match code {
        KeyCode::Esc => {
            state.picker = None;
            state.feedback = None;
        }
        KeyCode::Down | KeyCode::Char('j') => {
            picker.selected = Some(next_index(picker.selected, candidates.len(), true));
        }
        KeyCode::Up | KeyCode::Char('k') => {
            picker.selected = Some(next_index(picker.selected, candidates.len(), false));
        }
        KeyCode::Char(digit) if digit.is_ascii_digit() && digit != '0' => {
            let index = usize::try_from(digit.to_digit(10).unwrap_or_default())
                .unwrap_or_default()
                .saturating_sub(1);
            if index < candidates.len() {
                picker.selected = Some(index);
            }
        }
        KeyCode::Enter => {
            let Some(index) = picker.selected else {
                state.feedback = Some("choose a provider before confirming".to_owned());
                return Ok(None);
            };
            let provider = candidates
                .get(index)
                .ok_or_else(|| invalid_control("selected provider is no longer available"))?
                .clone();
            let purpose = picker.purpose;
            state.picker = None;
            state.feedback = None;
            if purpose == PickerPurpose::UsageOverride {
                open_usage_editor(snapshot, state, &provider)?;
                return Ok(None);
            }
            return action_for_provider(snapshot, purpose, &provider).map(Some);
        }
        KeyCode::Char('q') => return Ok(Some(ControlAction::Quit)),
        _ => {}
    }
    Ok(None)
}

fn open_usage_editor(
    snapshot: &DashboardSnapshot,
    state: &mut InteractionState,
    provider: &str,
) -> Result<(), TuiError> {
    let draft = snapshot
        .usage_override_drafts
        .iter()
        .find(|draft| draft.provider == provider)
        .ok_or_else(|| invalid_control("usage override values have not been entered"))?;
    state.usage_editor = Some(UsageEditorState::from_draft(draft));
    state.feedback =
        Some("edit values; Delete clears a field; select Confirm and press Enter".to_owned());
    Ok(())
}

fn usage_editor_action(code: KeyCode, state: &mut InteractionState) -> Option<ControlAction> {
    let editor = state.usage_editor.as_mut()?;
    match code {
        KeyCode::Esc => {
            state.usage_editor = None;
            state.feedback = None;
        }
        KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
            editor.field = next_usage_field(editor.field, true);
        }
        KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => {
            editor.field = next_usage_field(editor.field, false);
        }
        KeyCode::Delete => {
            if let Some(value) = editor.selected_value_mut() {
                value.clear();
            }
        }
        KeyCode::Backspace => {
            if let Some(value) = editor.selected_value_mut() {
                value.pop();
            }
        }
        KeyCode::Char(character) if is_number_character(character) => {
            if let Some(value) = editor.selected_value_mut()
                && value.len() < 64
            {
                value.push(character);
            }
        }
        KeyCode::Enter if editor.field == UsageEditorField::Confirm => {
            let result = editor
                .draft()
                .and_then(|draft| usage_override_from_draft(&draft));
            match result {
                Ok(action) => {
                    state.usage_editor = None;
                    state.feedback = None;
                    return Some(action);
                }
                Err(error) => {
                    state.feedback = Some(error.to_string());
                }
            }
        }
        KeyCode::Enter => {
            editor.field = next_usage_field(editor.field, true);
        }
        KeyCode::Char('q') => return Some(ControlAction::Quit),
        _ => {}
    }
    None
}

fn next_usage_field(field: UsageEditorField, forward: bool) -> UsageEditorField {
    match (field, forward) {
        (UsageEditorField::Used, true) | (UsageEditorField::Remaining, false) => {
            UsageEditorField::Limit
        }
        (UsageEditorField::Limit, true) | (UsageEditorField::Confirm, false) => {
            UsageEditorField::Remaining
        }
        (UsageEditorField::Remaining, true) | (UsageEditorField::Used, false) => {
            UsageEditorField::Confirm
        }
        (UsageEditorField::Confirm, true) | (UsageEditorField::Limit, false) => {
            UsageEditorField::Used
        }
    }
}

fn is_number_character(character: char) -> bool {
    character.is_ascii_digit() || matches!(character, '.' | 'e' | 'E' | '+' | '-')
}

fn display_optional_number(value: Option<f64>) -> String {
    value.map_or_else(String::new, |value| value.to_string())
}

fn parse_optional_number(value: &str, field: &str) -> Result<Option<f64>, TuiError> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    value
        .parse::<f64>()
        .map(Some)
        .map_err(|_| invalid_control(format!("usage override {field} is not a valid number")))
}

fn next_index(current: Option<usize>, len: usize, forward: bool) -> usize {
    match (current, forward) {
        (None, true) => 0,
        (None | Some(0), false) => len.saturating_sub(1),
        (Some(index), true) => (index + 1) % len,
        (Some(index), false) => index - 1,
    }
}

fn picker_candidates(snapshot: &DashboardSnapshot, purpose: PickerPurpose) -> Vec<String> {
    snapshot
        .provider_controls
        .iter()
        .filter(|option| match purpose {
            PickerPurpose::ManualProvider | PickerPurpose::Handover => option.enabled,
            PickerPurpose::ToggleEnabled => true,
            PickerPurpose::UsageOverride => snapshot
                .usage_override_drafts
                .iter()
                .any(|draft| draft.provider == option.provider),
        })
        .map(|option| option.provider.clone())
        .collect()
}

fn action_for_provider(
    snapshot: &DashboardSnapshot,
    purpose: PickerPurpose,
    provider: &str,
) -> Result<ControlAction, TuiError> {
    match purpose {
        PickerPurpose::ManualProvider => Ok(ControlAction::SelectProvider {
            task_id: required_task_id(snapshot)?,
            provider: provider.to_owned(),
        }),
        PickerPurpose::ToggleEnabled => {
            let option = snapshot
                .provider_controls
                .iter()
                .find(|option| option.provider == provider)
                .ok_or_else(|| invalid_control("provider is not configured"))?;
            Ok(ControlAction::SetProviderEnabled {
                provider: provider.to_owned(),
                enabled: !option.enabled,
            })
        }
        PickerPurpose::Handover => Ok(ControlAction::Handover {
            task_id: required_task_id(snapshot)?,
            to_provider: provider.to_owned(),
        }),
        PickerPurpose::UsageOverride => Err(invalid_control(
            "usage override must be confirmed through the value editor",
        )),
    }
}

fn required_task_id(snapshot: &DashboardSnapshot) -> Result<String, TuiError> {
    nonempty(&snapshot.task.id, "task id")
}

fn nonempty(value: &str, field: &str) -> Result<String, TuiError> {
    let value = value.trim();
    if value.is_empty() {
        Err(invalid_control(format!("{field} is empty")))
    } else {
        Ok(value.to_owned())
    }
}

fn usage_override_from_draft(draft: &UsageOverrideDraft) -> Result<ControlAction, TuiError> {
    if draft.used.is_none() && draft.remaining.is_none() {
        return Err(invalid_control("usage override requires used or remaining"));
    }
    for (name, value) in [
        ("used", draft.used),
        ("limit", draft.limit),
        ("remaining", draft.remaining),
    ] {
        if value.is_some_and(|value| !value.is_finite() || value < 0.0) {
            return Err(invalid_control(format!(
                "usage override {name} must be finite and non-negative"
            )));
        }
    }
    if draft.limit.is_some_and(|limit| limit <= 0.0) {
        return Err(invalid_control(
            "usage override limit must be greater than zero",
        ));
    }
    if let Some(limit) = draft.limit
        && (draft.used.is_some_and(|used| used > limit)
            || draft.remaining.is_some_and(|remaining| remaining > limit))
    {
        return Err(invalid_control(
            "usage override used/remaining cannot exceed limit",
        ));
    }
    if let (Some(used), Some(limit), Some(remaining)) = (draft.used, draft.limit, draft.remaining)
        && (used + remaining - limit).abs() > f64::EPSILON * limit.max(1.0)
    {
        return Err(invalid_control(
            "usage override used plus remaining must equal limit",
        ));
    }
    Ok(ControlAction::UsageOverride {
        provider: nonempty(&draft.provider, "usage provider")?,
        used: draft.used,
        limit: draft.limit,
        remaining: draft.remaining,
        entered_by: nonempty(&draft.entered_by, "usage audit identity")?,
    })
}

fn invalid_control(message: impl Into<String>) -> TuiError {
    TuiError::InvalidControlInput(message.into())
}

pub fn render(frame: &mut Frame<'_>, snapshot: &DashboardSnapshot) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(28),
            Constraint::Percentage(32),
            Constraint::Percentage(32),
            Constraint::Length(2),
        ])
        .split(frame.area());

    let top = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(rows[0]);
    render_task(frame, top[0], &snapshot.task);
    render_providers(frame, top[1], &snapshot.providers);

    let middle = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(rows[1]);
    render_routing(frame, middle[0], &snapshot.routing);
    render_handover(frame, middle[1], &snapshot.handover);
    render_verification(frame, rows[2], &snapshot.verification);

    let help = Line::from(vec![
        Span::styled(
            "a",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(":auto  "),
        Span::styled("m", Style::default().fg(Color::Cyan)),
        Span::raw(":provider  "),
        Span::styled("e", Style::default().fg(Color::Cyan)),
        Span::raw(":enable/disable  "),
        Span::styled("p/r", Style::default().fg(Color::Cyan)),
        Span::raw(":pause/resume  "),
        Span::styled("h", Style::default().fg(Color::Cyan)),
        Span::raw(":handover  "),
        Span::styled("u", Style::default().fg(Color::Cyan)),
        Span::raw(":usage override  "),
        Span::styled("f", Style::default().fg(Color::Cyan)),
        Span::raw(":profiles  "),
        Span::styled("c/q", Style::default().fg(Color::Cyan)),
        Span::raw(":cancel/quit"),
    ]);
    frame.render_widget(Paragraph::new(help), rows[3]);
}

fn render_interactive(
    frame: &mut Frame<'_>,
    snapshot: &DashboardSnapshot,
    state: &InteractionState,
) {
    render(frame, snapshot);
    if let Some(confirmation) = &state.profile_reset_confirmation {
        render_profile_reset_confirmation(frame, confirmation, state.feedback.as_deref());
    } else if let Some(editor) = &state.profile_editor {
        render_profile_editor(frame, editor, state.feedback.as_deref());
    } else if let Some(list) = &state.profile_list {
        render_profile_list(frame, snapshot, list, state.feedback.as_deref());
    } else if let Some(editor) = &state.usage_editor {
        render_usage_editor(frame, editor, state.feedback.as_deref());
    } else if let Some(picker) = &state.picker {
        render_picker(frame, snapshot, picker, state.feedback.as_deref());
    } else if let Some(feedback) = &state.feedback {
        let area = Rect::new(2, frame.area().height.saturating_sub(3), 60, 3);
        frame.render_widget(
            Paragraph::new(feedback.as_str())
                .style(Style::default().fg(Color::Yellow))
                .block(panel(" Control ")),
            area,
        );
    }
}

fn render_profile_list(
    frame: &mut Frame<'_>,
    snapshot: &DashboardSnapshot,
    list: &ProfileListState,
    feedback: Option<&str>,
) {
    let width = frame.area().width.min(68);
    let height = u16::try_from(snapshot.model_profiles.len())
        .unwrap_or(u16::MAX)
        .saturating_add(9)
        .min(frame.area().height);
    let area = centered_rect(width, height, frame.area());
    let mut lines = vec![Line::from(
        "  Provider Profile  Model                        Effort Source",
    )];
    lines.extend(
        snapshot
            .model_profiles
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let marker = if list.selected == Some(index) {
                    ">"
                } else {
                    " "
                };
                let source = if row.customized { "custom" } else { "preset" };
                Line::from(format!(
                    "{marker} {:<7} {:<8} {:<28} {:<6} [{source}]",
                    row.provider, row.profile, row.model, row.effort,
                ))
            }),
    );
    lines.push(Line::from("economy: fast and cost-efficient simple work"));
    lines.push(Line::from("standard: everyday development work"));
    lines.push(Line::from(
        "premium: complex work requiring the highest quality",
    ));
    lines.push(Line::from(feedback.unwrap_or(
        "Up/Down or number: select  Enter: edit  Delete: reset  Esc: close",
    )));
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel(" Model profiles "))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn render_profile_editor(
    frame: &mut Frame<'_>,
    editor: &ProfileEditorState,
    feedback: Option<&str>,
) {
    let area = centered_rect(frame.area().width.min(68), 9, frame.area());
    let lines = vec![
        Line::from(format!("{}.{}", editor.provider, editor.profile)),
        Line::from(format!(
            "{} Model: {}",
            if editor.field == ProfileEditorField::Model {
                ">"
            } else {
                " "
            },
            editor.model,
        )),
        Line::from(format!(
            "{} Effort: {}",
            if editor.field == ProfileEditorField::Effort {
                ">"
            } else {
                " "
            },
            editor.effort,
        )),
        Line::from(format!(
            "{} Confirm override",
            if editor.field == ProfileEditorField::Confirm {
                ">"
            } else {
                " "
            },
        )),
        Line::from("Up/Down: field  Left/Right/Space: effort"),
        Line::from(feedback.unwrap_or("Type model; Enter advances/submits; Esc cancels")),
    ];
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel(" Model profile editor "))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn render_profile_reset_confirmation(
    frame: &mut Frame<'_>,
    confirmation: &ProfileResetConfirmation,
    feedback: Option<&str>,
) {
    let area = centered_rect(frame.area().width.min(52), 6, frame.area());
    let lines = vec![
        Line::from(format!(
            "{}.{}",
            confirmation.provider, confirmation.profile
        )),
        Line::from(feedback.unwrap_or("Reset override? y/N")),
        Line::from("y: reset  n/Esc: cancel"),
    ];
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(panel(" Confirm profile reset ")),
        area,
    );
}

fn render_usage_editor(frame: &mut Frame<'_>, editor: &UsageEditorState, feedback: Option<&str>) {
    let area = centered_rect(frame.area().width.min(72), 11, frame.area());
    let rows = [
        (UsageEditorField::Used, "Used", editor.used.as_str()),
        (UsageEditorField::Limit, "Limit", editor.limit.as_str()),
        (
            UsageEditorField::Remaining,
            "Remaining",
            editor.remaining.as_str(),
        ),
    ];
    let mut lines = vec![Line::from(format!("Provider: {}", editor.provider))];
    lines.extend(rows.into_iter().map(|(field, label, value)| {
        let marker = if editor.field == field { ">" } else { " " };
        let value = if value.is_empty() { "<unset>" } else { value };
        Line::from(format!("{marker} {label}: {value}"))
    }));
    lines.push(Line::from(format!(
        "{} Confirm override",
        if editor.field == UsageEditorField::Confirm {
            ">"
        } else {
            " "
        }
    )));
    lines.push(Line::from(
        "Up/Down: field  type: edit  Backspace: erase  Delete: unset",
    ));
    lines.push(Line::from(feedback.unwrap_or(
        "Enter advances; Enter on Confirm submits; Esc cancels",
    )));
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel(" Usage override editor "))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn render_picker(
    frame: &mut Frame<'_>,
    snapshot: &DashboardSnapshot,
    picker: &PickerState,
    feedback: Option<&str>,
) {
    let candidates = picker_candidates(snapshot, picker.purpose);
    let width = frame.area().width.min(64);
    let height = u16::try_from(candidates.len())
        .unwrap_or(u16::MAX)
        .saturating_add(5)
        .min(frame.area().height);
    let area = centered_rect(width, height, frame.area());
    let title = match picker.purpose {
        PickerPurpose::ManualProvider => " Select worker provider ",
        PickerPurpose::ToggleEnabled => " Enable / disable provider ",
        PickerPurpose::Handover => " Select handover target ",
        PickerPurpose::UsageOverride => " Select usage override provider ",
    };
    let mut lines = candidates
        .iter()
        .enumerate()
        .map(|(index, provider)| {
            let marker = if picker.selected == Some(index) {
                ">"
            } else {
                " "
            };
            let enabled = snapshot
                .provider_controls
                .iter()
                .find(|option| option.provider == *provider)
                .is_some_and(|option| option.enabled);
            Line::from(format!(
                "{marker} {}. {provider} [{}]",
                index + 1,
                if enabled { "enabled" } else { "disabled" }
            ))
        })
        .collect::<Vec<_>>();
    lines.push(Line::from(feedback.unwrap_or(
        "Up/Down or number selects; Enter confirms; Esc cancels",
    )));
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel(title))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width.min(area.width),
        height.min(area.height),
    )
}

fn panel(title: &'static str) -> Block<'static> {
    Block::default().title(title).borders(Borders::ALL)
}

fn render_task(frame: &mut Frame<'_>, area: Rect, task: &TaskPanel) {
    let text = vec![
        Line::from(format!("ID: {}", task.id)),
        Line::from(format!("State: {} / {}", task.state, task.phase)),
        Line::from(format!("Difficulty: {}", task.difficulty)),
        Line::from(format!("Risks: {}", task.risks.join(", "))),
        Line::from(task.objective.clone()),
    ];
    frame.render_widget(
        Paragraph::new(text)
            .block(panel(" Task "))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn render_providers(frame: &mut Frame<'_>, area: Rect, providers: &[ProviderRow]) {
    let items = providers.iter().map(|provider| {
        ListItem::new(format!(
            "{} | {} | reset {} | {} / {} | {}",
            provider.provider,
            provider.usage,
            provider.reset,
            provider.source,
            provider.confidence,
            provider.health
        ))
    });
    frame.render_widget(List::new(items).block(panel(" Providers ")), area);
}

fn render_routing(frame: &mut Frame<'_>, area: Rect, routing: &RoutingPanel) {
    let mut lines = vec![
        Line::from(format!("Selected: {}", routing.selected)),
        Line::from(format!(
            "Profile: {} / effort {}",
            routing.profile, routing.effort
        )),
    ];
    lines.extend(
        routing
            .rationale
            .iter()
            .map(|line| Line::from(format!("- {line}"))),
    );
    if !routing.alternatives.is_empty() {
        lines.push(Line::from(format!(
            "Alternatives: {}",
            routing.alternatives.join(", ")
        )));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel(" Routing "))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn render_handover(frame: &mut Frame<'_>, area: Rect, handover: &HandoverPanel) {
    let lines = vec![
        Line::from(format!(
            "{} -> {}",
            handover.previous_provider, handover.next_provider
        )),
        Line::from(format!("Reason: {}", handover.reason)),
        Line::from(format!("Checkpoint: {}", handover.checkpoint)),
        Line::from(format!("Count: {}", handover.count)),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel(" Handover "))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn render_verification(frame: &mut Frame<'_>, area: Rect, verification: &VerificationPanel) {
    let mut lines = vec![
        Line::from(format!("Changed files: {}", verification.changed_files)),
        Line::from(format!(
            "Approval required: {}",
            verification.approval_required
        )),
    ];
    lines.extend(
        verification
            .tests
            .iter()
            .map(|test| Line::from(format!("PASS {test}"))),
    );
    lines.extend(
        verification
            .failures
            .iter()
            .map(|failure| Line::from(format!("FAIL {failure}"))),
    );
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel(" Verification "))
            .wrap(Wrap { trim: true }),
        area,
    );
}

/// Renders to any backend for snapshot and contract tests without entering raw mode.
///
/// # Errors
///
/// Returns an I/O error when the terminal backend cannot draw the frame.
pub fn draw_once<B: Backend>(
    terminal: &mut Terminal<B>,
    snapshot: &DashboardSnapshot,
) -> Result<(), io::Error> {
    terminal.draw(|frame| render(frame, snapshot)).map(|_| ())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use ratatui::backend::TestBackend;

    #[derive(Clone)]
    struct FakeTerminalControl {
        calls: Arc<Mutex<Vec<&'static str>>>,
        fail_alternate_screen: bool,
    }

    impl FakeTerminalControl {
        fn record(&self, operation: &'static str) {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(operation);
            }
        }
    }

    impl TerminalControl for FakeTerminalControl {
        fn enable_raw_mode(&mut self) -> Result<(), io::Error> {
            self.record("enable_raw");
            Ok(())
        }

        fn disable_raw_mode(&mut self) -> Result<(), io::Error> {
            self.record("disable_raw");
            Ok(())
        }

        fn enter_alternate_screen(&mut self) -> Result<(), io::Error> {
            self.record("enter_alternate");
            if self.fail_alternate_screen {
                Err(io::Error::other("injected alternate-screen failure"))
            } else {
                Ok(())
            }
        }

        fn leave_alternate_screen(&mut self) -> Result<(), io::Error> {
            self.record("leave_alternate");
            Ok(())
        }
    }

    #[test]
    fn restores_raw_mode_when_alternate_screen_setup_fails()
    -> Result<(), Box<dyn std::error::Error>> {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let result = TerminalGuard::enter(FakeTerminalControl {
            calls: Arc::clone(&calls),
            fail_alternate_screen: true,
        });
        assert!(result.is_err());
        let recorded = calls
            .lock()
            .map_err(|_| io::Error::other("fake terminal call log was poisoned"))?
            .clone();
        assert_eq!(
            recorded,
            vec!["enable_raw", "enter_alternate", "disable_raw"]
        );
        Ok(())
    }

    #[test]
    fn handover_action_contains_task_and_target() -> Result<(), Box<dyn std::error::Error>> {
        let snapshot = DashboardSnapshot {
            task: TaskPanel {
                id: "task-1".to_owned(),
                ..TaskPanel::default()
            },
            provider_controls: vec![ProviderControlOption {
                provider: "claude".to_owned(),
                enabled: true,
            }],
            ..DashboardSnapshot::default()
        };
        let mut state = InteractionState::default();
        assert_eq!(
            action_for_key(KeyCode::Char('h'), &snapshot, &mut state)?,
            None
        );
        assert_eq!(action_for_key(KeyCode::Enter, &snapshot, &mut state)?, None);
        assert_eq!(
            action_for_key(KeyCode::Char('1'), &snapshot, &mut state)?,
            None
        );
        assert_eq!(
            action_for_key(KeyCode::Enter, &snapshot, &mut state)?,
            Some(ControlAction::Handover {
                task_id: "task-1".to_owned(),
                to_provider: "claude".to_owned(),
            })
        );
        Ok(())
    }

    #[test]
    fn usage_override_editor_emits_only_edited_values_after_final_confirmation()
    -> Result<(), Box<dyn std::error::Error>> {
        let snapshot = DashboardSnapshot {
            provider_controls: vec![ProviderControlOption {
                provider: "codex".to_owned(),
                enabled: true,
            }],
            usage_override_drafts: vec![UsageOverrideDraft {
                provider: "codex".to_owned(),
                used: Some(25.0),
                limit: Some(100.0),
                remaining: Some(75.0),
                entered_by: "enterprise-admin".to_owned(),
            }],
            ..DashboardSnapshot::default()
        };
        let mut state = InteractionState::default();
        assert_eq!(
            action_for_key(KeyCode::Char('u'), &snapshot, &mut state)?,
            None
        );
        assert_eq!(action_for_key(KeyCode::Down, &snapshot, &mut state)?, None);
        assert_eq!(action_for_key(KeyCode::Enter, &snapshot, &mut state)?, None);
        assert!(state.picker.is_none());
        assert_eq!(
            state
                .usage_editor
                .as_ref()
                .map(|editor| editor.used.as_str()),
            Some("25")
        );

        assert_eq!(
            action_for_key(KeyCode::Delete, &snapshot, &mut state)?,
            None
        );
        assert_eq!(
            action_for_key(KeyCode::Char('4'), &snapshot, &mut state)?,
            None
        );
        assert_eq!(
            action_for_key(KeyCode::Char('0'), &snapshot, &mut state)?,
            None
        );
        assert_eq!(action_for_key(KeyCode::Down, &snapshot, &mut state)?, None);
        assert_eq!(action_for_key(KeyCode::Down, &snapshot, &mut state)?, None);
        assert_eq!(
            action_for_key(KeyCode::Delete, &snapshot, &mut state)?,
            None
        );
        assert_eq!(
            action_for_key(KeyCode::Char('6'), &snapshot, &mut state)?,
            None
        );
        assert_eq!(
            action_for_key(KeyCode::Char('0'), &snapshot, &mut state)?,
            None
        );
        assert_eq!(action_for_key(KeyCode::Down, &snapshot, &mut state)?, None);
        assert_eq!(
            action_for_key(KeyCode::Enter, &snapshot, &mut state)?,
            Some(ControlAction::UsageOverride {
                provider: "codex".to_owned(),
                used: Some(40.0),
                limit: Some(100.0),
                remaining: Some(60.0),
                entered_by: "enterprise-admin".to_owned(),
            })
        );
        assert!(state.usage_editor.is_none());
        Ok(())
    }

    #[test]
    fn usage_override_editor_keeps_invalid_input_open_and_escape_cancels()
    -> Result<(), Box<dyn std::error::Error>> {
        let snapshot = DashboardSnapshot {
            provider_controls: vec![ProviderControlOption {
                provider: "codex".to_owned(),
                enabled: true,
            }],
            usage_override_drafts: vec![UsageOverrideDraft {
                provider: "codex".to_owned(),
                used: None,
                limit: Some(100.0),
                remaining: None,
                entered_by: "enterprise-admin".to_owned(),
            }],
            ..DashboardSnapshot::default()
        };
        let mut state = InteractionState::default();
        action_for_key(KeyCode::Char('u'), &snapshot, &mut state)?;
        action_for_key(KeyCode::Char('1'), &snapshot, &mut state)?;
        action_for_key(KeyCode::Enter, &snapshot, &mut state)?;
        action_for_key(KeyCode::Up, &snapshot, &mut state)?;
        assert_eq!(action_for_key(KeyCode::Enter, &snapshot, &mut state)?, None);
        assert!(state.usage_editor.is_some());
        assert!(
            state
                .feedback
                .as_deref()
                .is_some_and(|message| message.contains("requires used or remaining"))
        );
        assert_eq!(action_for_key(KeyCode::Esc, &snapshot, &mut state)?, None);
        assert!(state.usage_editor.is_none());
        assert!(state.feedback.is_none());
        Ok(())
    }

    #[test]
    fn usage_override_editor_preserves_provider_specific_values_and_optional_limit()
    -> Result<(), Box<dyn std::error::Error>> {
        let snapshot = DashboardSnapshot {
            provider_controls: vec![
                ProviderControlOption {
                    provider: "codex".to_owned(),
                    enabled: true,
                },
                ProviderControlOption {
                    provider: "claude".to_owned(),
                    enabled: true,
                },
            ],
            usage_override_drafts: vec![
                UsageOverrideDraft {
                    provider: "codex".to_owned(),
                    used: Some(10.0),
                    limit: Some(100.0),
                    remaining: Some(90.0),
                    entered_by: "admin".to_owned(),
                },
                UsageOverrideDraft {
                    provider: "claude".to_owned(),
                    used: Some(7.0),
                    limit: Some(50.0),
                    remaining: Some(43.0),
                    entered_by: "admin".to_owned(),
                },
            ],
            ..DashboardSnapshot::default()
        };
        let mut state = InteractionState::default();
        action_for_key(KeyCode::Char('u'), &snapshot, &mut state)?;
        action_for_key(KeyCode::Char('2'), &snapshot, &mut state)?;
        action_for_key(KeyCode::Enter, &snapshot, &mut state)?;
        assert_eq!(
            state
                .usage_editor
                .as_ref()
                .map(|editor| (editor.provider.as_str(), editor.used.as_str())),
            Some(("claude", "7"))
        );
        action_for_key(KeyCode::Down, &snapshot, &mut state)?;
        action_for_key(KeyCode::Delete, &snapshot, &mut state)?;
        action_for_key(KeyCode::Down, &snapshot, &mut state)?;
        action_for_key(KeyCode::Down, &snapshot, &mut state)?;
        assert_eq!(
            action_for_key(KeyCode::Enter, &snapshot, &mut state)?,
            Some(ControlAction::UsageOverride {
                provider: "claude".to_owned(),
                used: Some(7.0),
                limit: None,
                remaining: Some(43.0),
                entered_by: "admin".to_owned(),
            })
        );
        Ok(())
    }

    #[test]
    fn usage_override_validation_rejects_non_finite_negative_and_inconsistent_values() {
        let invalid_values = [
            UsageOverrideDraft {
                provider: "codex".to_owned(),
                used: Some(f64::INFINITY),
                limit: None,
                remaining: None,
                entered_by: "admin".to_owned(),
            },
            UsageOverrideDraft {
                provider: "codex".to_owned(),
                used: Some(-1.0),
                limit: None,
                remaining: None,
                entered_by: "admin".to_owned(),
            },
            UsageOverrideDraft {
                provider: "codex".to_owned(),
                used: Some(80.0),
                limit: Some(100.0),
                remaining: Some(30.0),
                entered_by: "admin".to_owned(),
            },
        ];
        assert!(
            invalid_values
                .iter()
                .all(|draft| usage_override_from_draft(draft).is_err())
        );
    }

    #[test]
    fn provider_toggle_requires_explicit_selection() -> Result<(), Box<dyn std::error::Error>> {
        let snapshot = DashboardSnapshot {
            provider_controls: vec![
                ProviderControlOption {
                    provider: "codex".to_owned(),
                    enabled: true,
                },
                ProviderControlOption {
                    provider: "claude".to_owned(),
                    enabled: false,
                },
            ],
            ..DashboardSnapshot::default()
        };
        let mut state = InteractionState::default();
        assert_eq!(
            action_for_key(KeyCode::Char('e'), &snapshot, &mut state)?,
            None
        );
        assert_eq!(action_for_key(KeyCode::Enter, &snapshot, &mut state)?, None);
        assert_eq!(
            action_for_key(KeyCode::Char('2'), &snapshot, &mut state)?,
            None
        );
        assert_eq!(
            action_for_key(KeyCode::Enter, &snapshot, &mut state)?,
            Some(ControlAction::SetProviderEnabled {
                provider: "claude".to_owned(),
                enabled: true,
            })
        );
        Ok(())
    }

    #[test]
    fn profile_editor_emits_validated_model_and_effort() -> Result<(), Box<dyn std::error::Error>> {
        let snapshot = DashboardSnapshot {
            model_profiles: vec![ModelProfileRow {
                provider: "claude".to_owned(),
                profile: "premium".to_owned(),
                model: "claude-fable-5".to_owned(),
                effort: "high".to_owned(),
                description: "complex work requiring the highest quality".to_owned(),
                customized: false,
            }],
            ..DashboardSnapshot::default()
        };
        let mut state = InteractionState::default();
        action_for_key(KeyCode::Char('f'), &snapshot, &mut state)?;
        action_for_key(KeyCode::Down, &snapshot, &mut state)?;
        action_for_key(KeyCode::Enter, &snapshot, &mut state)?;
        action_for_key(KeyCode::End, &snapshot, &mut state)?;
        action_for_key(KeyCode::Char('x'), &snapshot, &mut state)?;
        action_for_key(KeyCode::Down, &snapshot, &mut state)?;
        action_for_key(KeyCode::Down, &snapshot, &mut state)?;
        assert_eq!(
            action_for_key(KeyCode::Enter, &snapshot, &mut state)?,
            Some(ControlAction::SetModelProfile {
                provider: "claude".to_owned(),
                profile: "premium".to_owned(),
                model: "claude-fable-5x".to_owned(),
                effort: "high".to_owned(),
            })
        );
        Ok(())
    }

    #[test]
    fn profile_reset_requires_explicit_confirmation() -> Result<(), Box<dyn std::error::Error>> {
        let snapshot = DashboardSnapshot {
            model_profiles: vec![ModelProfileRow {
                provider: "gemini".to_owned(),
                profile: "standard".to_owned(),
                model: "company-gemini".to_owned(),
                effort: "medium".to_owned(),
                description: "everyday development work".to_owned(),
                customized: true,
            }],
            ..DashboardSnapshot::default()
        };
        let mut state = InteractionState::default();
        action_for_key(KeyCode::Char('f'), &snapshot, &mut state)?;
        action_for_key(KeyCode::Down, &snapshot, &mut state)?;
        assert_eq!(
            action_for_key(KeyCode::Delete, &snapshot, &mut state)?,
            None
        );
        assert!(state.profile_reset_confirmation.is_some());
        assert_eq!(
            action_for_key(KeyCode::Char('y'), &snapshot, &mut state)?,
            Some(ControlAction::ResetModelProfile {
                provider: "gemini".to_owned(),
                profile: "standard".to_owned(),
            })
        );
        Ok(())
    }

    #[test]
    fn renders_all_five_panels() -> Result<(), Box<dyn std::error::Error>> {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend)?;
        let snapshot = DashboardSnapshot {
            task: TaskPanel {
                id: "task-1".into(),
                objective: "Implement safely".into(),
                state: "Running".into(),
                difficulty: "complex".into(),
                risks: vec!["security".into()],
                phase: "implementation".into(),
            },
            providers: vec![ProviderRow {
                provider: "codex".into(),
                usage: "42%".into(),
                reset: "tomorrow".into(),
                source: "official_protocol".into(),
                confidence: "confirmed".into(),
                health: "healthy".into(),
            }],
            ..DashboardSnapshot::default()
        };
        draw_once(&mut terminal, &snapshot)?;
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        for title in ["Task", "Providers", "Routing", "Handover", "Verification"] {
            assert!(rendered.contains(title));
        }
        Ok(())
    }
}
