use orchestrator_domain::{TaskState, TransitionGuards};

use crate::EngineResult;

#[derive(Clone, Debug, PartialEq)]
pub struct TaskLifecycle {
    state: TaskState,
    resume_state: Option<TaskState>,
}

impl TaskLifecycle {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: TaskState::Queued,
            resume_state: None,
        }
    }

    #[must_use]
    pub const fn state(&self) -> TaskState {
        self.state
    }

    #[must_use]
    pub const fn resume_state(&self) -> Option<TaskState> {
        self.resume_state
    }

    pub fn transition(
        &mut self,
        next: TaskState,
        mut guards: TransitionGuards,
    ) -> EngineResult<()> {
        if self.state == TaskState::Blocked {
            guards.resume_point = self.resume_state;
        }
        self.state.validate_transition(next, &guards)?;
        if next == TaskState::Blocked {
            self.resume_state = Some(self.state);
        } else if self.state == TaskState::Blocked {
            self.resume_state = None;
        }
        self.state = next;
        Ok(())
    }
}

impl Default for TaskLifecycle {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use orchestrator_domain::{TaskState, TransitionGuards};

    use super::TaskLifecycle;

    #[test]
    fn blocked_task_returns_only_to_recorded_state() -> Result<(), Box<dyn std::error::Error>> {
        let mut lifecycle = TaskLifecycle::new();
        lifecycle.transition(TaskState::Analyzing, TransitionGuards::default())?;
        lifecycle.transition(TaskState::Blocked, TransitionGuards::default())?;
        assert_eq!(lifecycle.resume_state(), Some(TaskState::Analyzing));
        lifecycle.transition(TaskState::Analyzing, TransitionGuards::default())?;
        assert_eq!(lifecycle.resume_state(), None);
        Ok(())
    }
}
