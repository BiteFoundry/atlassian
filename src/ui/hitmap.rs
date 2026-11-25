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
    HtmlPreview,
    CrossLinks,
    OpenBrowser,
    ToggleToc,
    FocusJql,
    BackHistory,
    CheckUpdate,
    ToggleInlineImages,
    ToggleDrawer,
    SiteSwitcher,
    // Global / workspace chrome controls
    ToggleNavRail,
    ToggleDensity,
    CycleTheme,
    TogglePin,
    Quit,
    RunJql,
    FocusLocalFilter,
    CloseLocalFilter,
    RemoteSearch,
    BoardColPrev,
    BoardColNext,
    // Overlay buttons
    OverlaySubmit,
    OverlayCancel,
    OverlayExternalEditor,
    OverlayDiscoverSites,
    OverlayCopyUrl,
    OverlayFileParent,
    OverlayFileZoxide,
    OverlayImagePrev,
    OverlayImageNext,
    OverlayImageZoomIn,
    OverlayImageZoomOut,
    OverlayImageFit,
    OverlayImageProtocol,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitTarget {
    /// Top workspace tab (`0..=5`).
    Tab(usize),
    /// Container for Board left-nav Boards section.
    BoardNavBoards,
    /// Container for Board left-nav Sprints section.
    BoardNavSprints,
    /// Board item in Board view left nav.
    BoardItem(usize),
    /// Sprint item in Board view left nav.
    SprintItem(usize),
    /// Entire Kanban column (header, empty space, card gaps).
    KanbanColumn(usize),
    /// Card in Kanban column.
    KanbanCard { col: usize, row: usize },
    /// Container for Issues left-nav Saved Filters section.
    IssueNavFilters,
    /// Container for Issues left-nav Issue List section.
    IssueNavIssues,
    /// Saved filter row in Issues tab.
    FilterItem(usize),
    /// Issue list row in Issues tab.
    IssueRow(usize),
    /// Container for My Work left column (Issues).
    MyWorkIssuesPane,
    /// Container for My Work right column (Pages).
    MyWorkPagesPane,
    /// Issue row in My Work tab.
    MyWorkIssue(usize),
    /// Page row in My Work tab.
    MyWorkPage(usize),
    /// Container for Confluence left-nav Spaces section.
    WikiNavSpaces,
    /// Container for Confluence left-nav Page Tree section.
    WikiNavTree,
    /// Space row in Confluence tab.
    SpaceItem(usize),
    /// Expand/collapse chevron (`▸`/`▾`) on a parent Confluence tree row.
    TreeChevron(usize),
    /// Flattened tree row in Confluence tab.
    TreeRow(usize),
    /// Container for Confluence TOC drawer.
    TocPane,
    /// TOC heading item in Confluence TOC drawer.
    TocItem(usize),
    /// Container for Unified Search results pane.
    SearchResultsPane,
    /// Unified search hit row.
    SearchHit(usize),
    /// Container for Directory Projects section.
    DirectoryProjectsPane,
    /// Container for Directory Spaces section.
    DirectorySpacesPane,
    /// Container for Directory Connected Sites table.
    DirectorySitesPane,
    /// Connected site row in Directory tab.
    DirectorySiteRow(usize),
    /// Project row in Directory tab.
    DirectoryProject(usize),
    /// Space row in Directory tab.
    DirectorySpace(usize),
    /// Attachment chip/row in Issue or Wiki detail.
    Attachment(usize),
    /// Subtask row in Issue detail.
    DetailSubtask(usize),
    /// Linked Jira issue row in Issue detail.
    DetailIssueLink(usize),
    /// Linked Confluence page row in Issue detail.
    DetailConfluenceLink(usize),
    /// Ancestor breadcrumb item in Wiki reader.
    WikiBreadcrumb(usize),
    /// Overlay list item.
    OverlayItem(usize),
    /// Overlay item secondary toggle (e.g. SiteSwitcher enabled toggle).
    OverlayItemToggle(usize),
    /// Clickable form field in an overlay (`IssueForm`, `PageForm`, `Comment`, `PathInput`).
    OverlayField(usize),
    /// Scrollable text area inside an overlay (`Comment`, `IssueForm`, `PageForm`, `Help`).
    OverlayScrollArea,
    /// Interior body of an active modal overlay (used to detect click-outside-to-close).
    OverlayBody,
    /// Clickable close pill (`[✕ Esc]`) on an active modal overlay.
    OverlayClose,
    /// Collapsible site group header in left sidebar (`index` into `UiState::site_group_keys`).
    SiteGroupHeader(usize),
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

    pub fn region_of(&self, target: HitTarget) -> Option<Rect> {
        self.regions
            .iter()
            .rev()
            .find(|(_, t)| *t == target)
            .map(|(area, _)| *area)
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
