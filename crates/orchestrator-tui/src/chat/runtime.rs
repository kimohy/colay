use std::{io, time::Duration, time::Instant};

use crossterm::event::{self, Event, KeyEventKind};
use ratatui::{Terminal, backend::Backend, backend::CrosstermBackend, layout::Rect};
use thiserror::Error;

use crate::chat::{
    ActionFeedback, UiEffect, WorkspaceAction, WorkspaceCursor, WorkspaceSnapshot, WorkspaceState,
    compute_layout, render_workspace,
};
use crate::{CrosstermTerminalControl, TerminalGuard, TuiError};

const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(50);
const SNAPSHOT_REFRESH_INTERVAL: Duration = Duration::from_millis(200);

#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("{message}")]
pub struct DriverError {
    message: String,
}

impl DriverError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

pub trait WorkspaceDriver {
    /// Reads a presentation-safe durable projection after the supplied cursor.
    ///
    /// # Errors
    ///
    /// Returns [`DriverError`] when the backing projection cannot be read safely.
    fn refresh(&mut self, cursor: &WorkspaceCursor) -> Result<WorkspaceSnapshot, DriverError>;

    /// Applies one typed workspace action.
    ///
    /// # Errors
    ///
    /// Returns [`DriverError`] when the action cannot be durably accepted.
    fn dispatch(&mut self, action: WorkspaceAction) -> Result<ActionFeedback, DriverError>;
}

trait EventSource {
    fn poll(&mut self, timeout: Duration) -> io::Result<bool>;
    fn read(&mut self) -> io::Result<Event>;
}

struct CrosstermEvents;

impl EventSource for CrosstermEvents {
    fn poll(&mut self, timeout: Duration) -> io::Result<bool> {
        event::poll(timeout)
    }

    fn read(&mut self) -> io::Result<Event> {
        event::read()
    }
}

/// Runs the persistent chat workspace until the user requests quit.
///
/// # Errors
///
/// Returns [`TuiError`] when terminal setup/input fails or the initial durable
/// workspace snapshot cannot be loaded or validated.
pub fn run_workspace<D: WorkspaceDriver>(driver: &mut D) -> Result<(), TuiError> {
    let _guard = TerminalGuard::enter(CrosstermTerminalControl)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    run_loop(driver, &mut terminal, &mut CrosstermEvents)
}

fn run_loop<D, B, E>(
    driver: &mut D,
    terminal: &mut Terminal<B>,
    events: &mut E,
) -> Result<(), TuiError>
where
    D: WorkspaceDriver,
    B: Backend,
    E: EventSource,
{
    let mut snapshot = driver
        .refresh(&WorkspaceCursor::default())
        .map_err(|error| TuiError::Driver(error.to_string()))?;
    snapshot
        .validate()
        .map_err(|error| TuiError::InvalidControlInput(error.to_string()))?;
    let mut state = WorkspaceState::default();
    state.reconcile_snapshot(&snapshot);
    let mut last_refresh = Instant::now();

    loop {
        terminal.draw(|frame| render_workspace(frame, &snapshot, &state))?;
        if last_refresh.elapsed() >= SNAPSHOT_REFRESH_INTERVAL {
            match driver.refresh(&snapshot.cursor) {
                Ok(refreshed) => match refreshed.validate() {
                    Ok(()) => {
                        snapshot = refreshed;
                        state.reconcile_snapshot(&snapshot);
                    }
                    Err(error) => state.set_feedback(ActionFeedback::error(error.to_string())),
                },
                Err(error) => state.set_feedback(ActionFeedback::error(error.to_string())),
            }
            last_refresh = Instant::now();
        }
        if !events.poll(INPUT_POLL_INTERVAL)? {
            continue;
        }
        let Event::Key(key) = events.read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
            continue;
        }
        let terminal_size = terminal.size()?;
        let layout_mode = compute_layout(
            Rect::new(0, 0, terminal_size.width, terminal_size.height),
            state.primary_view(),
        )
        .mode;
        match state.handle_key(key, &snapshot, layout_mode) {
            UiEffect::None | UiEffect::Redraw => {}
            UiEffect::Feedback(feedback) => state.set_feedback(feedback),
            UiEffect::Dispatch(WorkspaceAction::Quit) => break,
            UiEffect::Dispatch(action) => match driver.dispatch(action) {
                Ok(feedback) => {
                    state.set_feedback(feedback);
                    match driver.refresh(&snapshot.cursor) {
                        Ok(refreshed) => {
                            refreshed.validate().map_err(|error| {
                                TuiError::InvalidControlInput(error.to_string())
                            })?;
                            snapshot = refreshed;
                            state.reconcile_snapshot(&snapshot);
                            last_refresh = Instant::now();
                        }
                        Err(error) => {
                            state.set_feedback(ActionFeedback::error(error.to_string()));
                        }
                    }
                }
                Err(error) => state.set_feedback(ActionFeedback::error(error.to_string())),
            },
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, io, time::Duration};

    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend, buffer::Cell};

    use super::{DriverError, EventSource, WorkspaceDriver, run_loop};
    use crate::chat::{
        ActionFeedback, ComposerTarget, DaemonConnectivity, WorkspaceAction, WorkspaceCursor,
        WorkspaceSnapshot,
    };

    struct FakeDriver {
        snapshot: WorkspaceSnapshot,
        refreshes: usize,
        actions: Vec<WorkspaceAction>,
        fail_dispatch: bool,
    }

    impl WorkspaceDriver for FakeDriver {
        fn refresh(&mut self, _cursor: &WorkspaceCursor) -> Result<WorkspaceSnapshot, DriverError> {
            self.refreshes += 1;
            Ok(self.snapshot.clone())
        }

        fn dispatch(&mut self, action: WorkspaceAction) -> Result<ActionFeedback, DriverError> {
            if self.fail_dispatch {
                self.fail_dispatch = false;
                return Err(DriverError::new("scripted dispatch failure"));
            }
            self.actions.push(action);
            Ok(ActionFeedback::info("accepted"))
        }
    }

    struct ScriptedEvents {
        events: VecDeque<Event>,
        initial_delay: Option<Duration>,
    }

    impl EventSource for ScriptedEvents {
        fn poll(&mut self, _timeout: Duration) -> io::Result<bool> {
            if let Some(delay) = self.initial_delay.take() {
                std::thread::sleep(delay);
                return Ok(false);
            }
            Ok(!self.events.is_empty())
        }

        fn read(&mut self) -> io::Result<Event> {
            self.events.pop_front().ok_or_else(|| {
                io::Error::new(io::ErrorKind::UnexpectedEof, "scripted events exhausted")
            })
        }
    }

    fn valid_snapshot() -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            repository: "colay".to_owned(),
            session_id: "session-01".to_owned(),
            session_title: "chat".to_owned(),
            session_state: "drafting".to_owned(),
            daemon: DaemonConnectivity::Online,
            ..WorkspaceSnapshot::default()
        }
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    fn rendered(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(Cell::symbol)
            .collect()
    }

    #[test]
    fn runtime_dispatches_message_and_keeps_loop_alive_until_quit()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut driver = FakeDriver {
            snapshot: valid_snapshot(),
            refreshes: 0,
            actions: Vec::new(),
            fail_dispatch: false,
        };
        let mut terminal = Terminal::new(TestBackend::new(110, 30))?;
        let mut events = ScriptedEvents {
            events: [
                key(KeyCode::Tab, KeyModifiers::NONE),
                key(KeyCode::Tab, KeyModifiers::NONE),
                key(KeyCode::Char('h'), KeyModifiers::NONE),
                key(KeyCode::Char('i'), KeyModifiers::NONE),
                key(KeyCode::Enter, KeyModifiers::NONE),
                key(KeyCode::Tab, KeyModifiers::NONE),
                key(KeyCode::Char('q'), KeyModifiers::NONE),
            ]
            .into_iter()
            .collect(),
            initial_delay: None,
        };
        run_loop(&mut driver, &mut terminal, &mut events)?;
        assert_eq!(
            driver.actions,
            vec![WorkspaceAction::SubmitMessage {
                target: ComposerTarget::Orchestrator,
                content: "hi".to_owned(),
            }]
        );
        assert!(driver.refreshes >= 2);
        Ok(())
    }

    #[test]
    fn runtime_refreshes_after_two_hundred_milliseconds_without_input()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut driver = FakeDriver {
            snapshot: valid_snapshot(),
            refreshes: 0,
            actions: Vec::new(),
            fail_dispatch: false,
        };
        let mut terminal = Terminal::new(TestBackend::new(110, 30))?;
        let mut events = ScriptedEvents {
            events: [key(KeyCode::Char('q'), KeyModifiers::NONE)]
                .into_iter()
                .collect(),
            initial_delay: Some(Duration::from_millis(210)),
        };
        run_loop(&mut driver, &mut terminal, &mut events)?;
        assert!(driver.refreshes >= 2);
        Ok(())
    }

    #[test]
    fn read_only_snapshot_suppresses_message_dispatch() -> Result<(), Box<dyn std::error::Error>> {
        let mut snapshot = valid_snapshot();
        snapshot.read_only_reason = Some("daemon offline".to_owned());
        let mut driver = FakeDriver {
            snapshot,
            refreshes: 0,
            actions: Vec::new(),
            fail_dispatch: false,
        };
        let mut terminal = Terminal::new(TestBackend::new(110, 30))?;
        let mut events = ScriptedEvents {
            events: [
                key(KeyCode::Tab, KeyModifiers::NONE),
                key(KeyCode::Tab, KeyModifiers::NONE),
                key(KeyCode::Char('x'), KeyModifiers::NONE),
                key(KeyCode::Enter, KeyModifiers::NONE),
                key(KeyCode::Tab, KeyModifiers::NONE),
                key(KeyCode::Char('q'), KeyModifiers::NONE),
            ]
            .into_iter()
            .collect(),
            initial_delay: None,
        };
        run_loop(&mut driver, &mut terminal, &mut events)?;
        assert!(driver.actions.is_empty());
        assert!(rendered(&terminal).contains("daemon offline"));
        Ok(())
    }

    #[test]
    fn dispatch_error_becomes_feedback_without_abandoning_terminal()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut driver = FakeDriver {
            snapshot: valid_snapshot(),
            refreshes: 0,
            actions: Vec::new(),
            fail_dispatch: true,
        };
        let mut terminal = Terminal::new(TestBackend::new(110, 30))?;
        let mut events = ScriptedEvents {
            events: [
                key(KeyCode::Tab, KeyModifiers::NONE),
                key(KeyCode::Tab, KeyModifiers::NONE),
                key(KeyCode::Char('x'), KeyModifiers::NONE),
                key(KeyCode::Enter, KeyModifiers::NONE),
                key(KeyCode::Tab, KeyModifiers::NONE),
                key(KeyCode::Char('q'), KeyModifiers::NONE),
            ]
            .into_iter()
            .collect(),
            initial_delay: None,
        };
        run_loop(&mut driver, &mut terminal, &mut events)?;
        assert!(driver.actions.is_empty());
        assert!(rendered(&terminal).contains("scripted dispatch failure"));
        Ok(())
    }
}
