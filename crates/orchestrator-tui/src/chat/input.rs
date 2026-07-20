use crate::chat::ComposerTarget;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaletteCommand {
    Tasks,
    Plan,
    Approve,
    Pause,
    Resume,
    Cancel,
    Handover,
    Retry,
    Checkpoint,
    Provider,
}

#[must_use]
pub fn parse_palette_command(value: &str) -> Option<PaletteCommand> {
    match value.trim() {
        "/tasks" => Some(PaletteCommand::Tasks),
        "/plan" => Some(PaletteCommand::Plan),
        "/approve" => Some(PaletteCommand::Approve),
        "/pause" => Some(PaletteCommand::Pause),
        "/resume" => Some(PaletteCommand::Resume),
        "/cancel" => Some(PaletteCommand::Cancel),
        "/handover" => Some(PaletteCommand::Handover),
        "/retry" => Some(PaletteCommand::Retry),
        "/checkpoint" => Some(PaletteCommand::Checkpoint),
        "/provider" => Some(PaletteCommand::Provider),
        _ => None,
    }
}

#[must_use]
pub fn parse_submission(
    value: &str,
    persistent_target: &ComposerTarget,
) -> Option<(ComposerTarget, String)> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let Some((prefix, content)) = value.split_once(char::is_whitespace) else {
        return Some((persistent_target.clone(), value.to_owned()));
    };
    let content = content.trim();
    if content.is_empty() {
        return None;
    }
    match prefix {
        "@all" => Some((ComposerTarget::AllRunning, content.to_owned())),
        task if task.starts_with("@task-") && task.len() > 6 => Some((
            ComposerTarget::Task(task.trim_start_matches('@').to_owned()),
            content.to_owned(),
        )),
        _ => Some((persistent_target.clone(), value.to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::{PaletteCommand, parse_palette_command, parse_submission};
    use crate::chat::ComposerTarget;

    #[test]
    fn mention_prefixes_override_only_one_submission() {
        assert_eq!(
            parse_submission(
                "@task-03 fix the failing test",
                &ComposerTarget::Orchestrator
            ),
            Some((
                ComposerTarget::Task("task-03".to_owned()),
                "fix the failing test".to_owned()
            ))
        );
        assert_eq!(
            parse_submission("@all report status", &ComposerTarget::Orchestrator),
            Some((ComposerTarget::AllRunning, "report status".to_owned()))
        );
        assert_eq!(parse_submission("  ", &ComposerTarget::Orchestrator), None);
    }

    #[test]
    fn command_palette_parses_every_approved_command() {
        for (value, expected) in [
            ("/tasks", PaletteCommand::Tasks),
            ("/plan", PaletteCommand::Plan),
            ("/approve", PaletteCommand::Approve),
            ("/pause", PaletteCommand::Pause),
            ("/resume", PaletteCommand::Resume),
            ("/cancel", PaletteCommand::Cancel),
            ("/handover", PaletteCommand::Handover),
            ("/retry", PaletteCommand::Retry),
            ("/checkpoint", PaletteCommand::Checkpoint),
            ("/provider", PaletteCommand::Provider),
        ] {
            assert_eq!(parse_palette_command(value), Some(expected));
        }
        assert_eq!(parse_palette_command("/unknown"), None);
    }
}
