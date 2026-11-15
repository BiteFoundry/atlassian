//! Rendered screen region -> clickable target mapping (`HitMap`).

use ratatui::layout::{Position, Rect};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonId {
    SyncAll,
    CommandPalette,
    Help,
    Transition,
    AssignToMe,
    Assignee,
    Comment,
    Edit,
    NewItem,
    UploadAttachment,
    DownloadAttachment,
    ImagePreview,
    CrossLinks,
    OpenBrowser,
    ToggleToc,
    FocusJql,
    BackHistory,
    CheckUpdate,
    ToggleInlineImages,
    ToggleDrawer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitTarget {
    /// Top workspace tab (`0..=5`).
    Tab(usize),
    /// Board item in Board view left nav.
    BoardItem(usize),
    /// Sprint item in Board view left nav.
    SprintItem(usize),
    /// Card in Kanban column.
    KanbanCard { col: usize, row: usize },
    /// Saved filter row in Issues tab.
    FilterItem(usize),
    /// Issue list row in Issues tab.
    IssueRow(usize),
    /// Issue row in My Work tab.
    MyWorkIssue(usize),
    /// Page row in My Work tab.
    MyWorkPage(usize),
    /// Space row in Confluence tab.
    SpaceItem(usize),
    /// Flattened tree row in Confluence tab.
    TreeRow(usize),
    /// TOC heading item in Confluence TOC drawer.
    TocItem(usize),
    /// Unified search hit row.
    SearchHit(usize),
    /// Project row in Directory tab.
    DirectoryProject(usize),
    /// Space row in Directory tab.
    DirectorySpace(usize),
    /// Attachment chip/row.
    Attachment(usize),
    /// Overlay list item.
    OverlayItem(usize),
    /// Toolbar / action hint button.
    Button(ButtonId),
    /// JQL / search input bar.
    JqlBar,
    /// Unified search query bar.
    SearchBar,
    /// Detail / reader pane (focus).
    DetailPane,
    /// Nav pane (focus).
    NavPane,
}

#[derive(Debug, Clone, Default)]
pub struct HitMap {
    regions: Vec<(Rect, HitTarget)>,
}

impl HitMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.regions.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }

    pub fn len(&self) -> usize {
        self.regions.len()
    }

    pub fn add(&mut self, area: Rect, target: HitTarget) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        self.regions.push((area, target));
    }

    pub fn add_line(&mut self, x: u16, row: u16, width: u16, target: HitTarget) {
        self.add(Rect::new(x, row, width, 1), target);
    }

    pub fn hit(&self, row: u16, col: u16) -> Option<HitTarget> {
        let pos = Position::new(col, row);
        self.regions
            .iter()
            .rev()
            .find(|(area, _)| area.contains(pos))
            .map(|(_, target)| *target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn later_region_wins_overlap() {
        let mut map = HitMap::new();
        map.add(Rect::new(0, 0, 20, 5), HitTarget::DetailPane);
        map.add_line(2, 2, 10, HitTarget::Button(ButtonId::Transition));
        assert_eq!(map.hit(2, 5), Some(HitTarget::Button(ButtonId::Transition)));
        assert_eq!(map.hit(1, 5), Some(HitTarget::DetailPane));
        assert_eq!(map.hit(6, 5), None);
    }
}
