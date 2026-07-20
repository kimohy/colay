use ratatui::layout::{Constraint, Layout, Rect};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutMode {
    Wide,
    Medium,
    Narrow,
    TooSmall,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PrimaryView {
    Tasks,
    #[default]
    Conversation,
    Inspector,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkspaceLayout {
    pub mode: LayoutMode,
    pub header: Rect,
    pub task_graph: Option<Rect>,
    pub conversation: Option<Rect>,
    pub inspector: Option<Rect>,
    pub composer: Option<Rect>,
    pub compact_status: Option<Rect>,
}

impl WorkspaceLayout {
    #[must_use]
    pub fn visible_rectangles(self) -> Vec<Rect> {
        let mut rectangles = Vec::with_capacity(5);
        if self.header.width > 0 && self.header.height > 0 {
            rectangles.push(self.header);
        }
        rectangles.extend(
            [
                self.task_graph,
                self.conversation,
                self.inspector,
                self.composer,
                self.compact_status,
            ]
            .into_iter()
            .flatten(),
        );
        rectangles
    }
}

#[must_use]
pub fn compute_layout(area: Rect, primary_view: PrimaryView) -> WorkspaceLayout {
    if area.width < 60 || area.height < 8 {
        return WorkspaceLayout {
            mode: LayoutMode::TooSmall,
            header: Rect::default(),
            task_graph: None,
            conversation: None,
            inspector: None,
            composer: None,
            compact_status: Some(area),
        };
    }

    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(3),
    ])
    .split(area);
    let header = rows[0];
    let content = rows[1];
    let composer = Some(rows[2]);
    if area.width >= 110 {
        let columns = Layout::horizontal([
            Constraint::Percentage(26),
            Constraint::Percentage(49),
            Constraint::Percentage(25),
        ])
        .split(content);
        WorkspaceLayout {
            mode: LayoutMode::Wide,
            header,
            task_graph: Some(columns[0]),
            conversation: Some(columns[1]),
            inspector: Some(columns[2]),
            composer,
            compact_status: None,
        }
    } else if area.width >= 80 {
        let columns = Layout::horizontal([Constraint::Percentage(31), Constraint::Percentage(69)])
            .split(content);
        WorkspaceLayout {
            mode: LayoutMode::Medium,
            header,
            task_graph: Some(columns[0]),
            conversation: Some(columns[1]),
            inspector: None,
            composer,
            compact_status: None,
        }
    } else {
        let (task_graph, conversation, inspector) = match primary_view {
            PrimaryView::Tasks => (Some(content), None, None),
            PrimaryView::Conversation => (None, Some(content), None),
            PrimaryView::Inspector => (None, None, Some(content)),
        };
        WorkspaceLayout {
            mode: LayoutMode::Narrow,
            header,
            task_graph,
            conversation,
            inspector,
            composer,
            compact_status: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::layout::Rect;

    use super::{LayoutMode, PrimaryView, compute_layout};

    #[test]
    fn responsive_thresholds_are_exact() {
        assert_eq!(
            compute_layout(Rect::new(0, 0, 110, 30), PrimaryView::Conversation).mode,
            LayoutMode::Wide
        );
        assert_eq!(
            compute_layout(Rect::new(0, 0, 109, 30), PrimaryView::Conversation).mode,
            LayoutMode::Medium
        );
        assert_eq!(
            compute_layout(Rect::new(0, 0, 80, 30), PrimaryView::Conversation).mode,
            LayoutMode::Medium
        );
        assert_eq!(
            compute_layout(Rect::new(0, 0, 79, 30), PrimaryView::Conversation).mode,
            LayoutMode::Narrow
        );
        assert_eq!(
            compute_layout(Rect::new(0, 0, 60, 20), PrimaryView::Conversation).mode,
            LayoutMode::Narrow
        );
        assert_eq!(
            compute_layout(Rect::new(0, 0, 59, 20), PrimaryView::Conversation).mode,
            LayoutMode::TooSmall
        );
    }

    #[test]
    fn interactive_modes_reserve_header_and_composer() {
        for width in [60, 80, 110, 160] {
            let area = Rect::new(0, 0, width, 24);
            let layout = compute_layout(area, PrimaryView::Conversation);
            assert_eq!(layout.header.height, 1);
            assert_eq!(layout.composer.map(|value| value.height), Some(3));
            for rectangle in layout.visible_rectangles() {
                assert!(rectangle.x >= area.x);
                assert!(rectangle.y >= area.y);
                assert!(rectangle.right() <= area.right());
                assert!(rectangle.bottom() <= area.bottom());
            }
        }
    }

    #[test]
    fn too_small_mode_has_status_but_no_mutating_composer() {
        let layout = compute_layout(Rect::new(0, 0, 59, 20), PrimaryView::Conversation);
        assert!(layout.compact_status.is_some());
        assert!(layout.composer.is_none());
        assert!(layout.task_graph.is_none());
        assert!(layout.conversation.is_none());
        assert!(layout.inspector.is_none());
    }

    #[test]
    fn narrow_mode_uses_the_selected_primary_view() {
        let tasks = compute_layout(Rect::new(0, 0, 70, 24), PrimaryView::Tasks);
        assert!(tasks.task_graph.is_some());
        assert!(tasks.conversation.is_none());
        let inspector = compute_layout(Rect::new(0, 0, 70, 24), PrimaryView::Inspector);
        assert!(inspector.inspector.is_some());
        assert!(inspector.task_graph.is_none());
    }
}
