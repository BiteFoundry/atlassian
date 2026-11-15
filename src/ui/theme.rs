//! Atlassian visual identity color tokens, status category lozenges, and issue badges.

use ratatui::{
    style::{Color, Modifier, Style},
    text::Span,
};

use crate::{
    config::Persona,
    models::{IssuePriority, IssueType, PanelKind, StatusCategory},
};

/// Atlassian brand palette & TUI semantic tokens.
#[derive(Debug, Clone)]
pub struct Theme {
    pub jira_cobalt: Color,
    pub confluence_teal: Color,
    pub fg: Color,
    pub muted: Color,
    pub accent: Color,
    pub border: Color,
    pub border_focus: Color,
    pub selection_bg: Color,
    pub selection_fg: Color,
    pub card_bg: Color,
    pub card_selected_bg: Color,
    pub code_fg: Color,
    pub code_bg: Color,
    pub error: Color,
    pub success: Color,
    pub warning: Color,
    pub info: Color,
    pub note: Color,
    pub purple: Color,
    pub orange: Color,
    pub amber: Color,
    pub slate: Color,
    pub chip_bg: Color,
    pub chip_fg: Color,
    pub mention_bg: Color,
    pub mention_fg: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            jira_cobalt: Color::Rgb(0, 82, 204),       // #0052CC
            confluence_teal: Color::Rgb(0, 184, 217),  // #00B8D9
            fg: Color::Reset,
            muted: Color::Rgb(122, 134, 154),
            accent: Color::Rgb(76, 154, 255),
            border: Color::Rgb(66, 82, 110),
            border_focus: Color::Rgb(0, 184, 217),
            selection_bg: Color::Rgb(30, 58, 110),
            selection_fg: Color::White,
            card_bg: Color::Rgb(24, 30, 44),
            card_selected_bg: Color::Rgb(32, 52, 92),
            code_fg: Color::Rgb(230, 219, 116),
            code_bg: Color::Rgb(28, 32, 42),
            error: Color::Rgb(222, 53, 11),
            success: Color::Rgb(0, 135, 90),
            warning: Color::Rgb(255, 171, 0),
            info: Color::Rgb(0, 101, 255),
            note: Color::Rgb(101, 84, 192),
            purple: Color::Rgb(159, 107, 255),
            orange: Color::Rgb(255, 139, 0),
            amber: Color::Rgb(255, 171, 0),
            slate: Color::Rgb(122, 134, 154),
            chip_bg: Color::Rgb(36, 52, 82),
            chip_fg: Color::Rgb(179, 212, 255),
            mention_bg: Color::Rgb(42, 60, 88),
            mention_fg: Color::Rgb(121, 226, 242),
        }
    }
}

impl Theme {
    pub fn persona_badge(&self, persona: Persona) -> Span<'static> {
        match persona {
            Persona::Jira => Span::styled(
                " 🔷 JIRA ",
                Style::default()
                    .bg(self.jira_cobalt)
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Persona::Confluence => Span::styled(
                " 📘 CONFLUENCE ",
                Style::default()
                    .bg(self.confluence_teal)
                    .fg(Color::Rgb(9, 30, 66))
                    .add_modifier(Modifier::BOLD),
            ),
        }
    }

    pub fn border_style(&self, focused: bool) -> Style {
        Style::default().fg(if focused {
            self.border_focus
        } else {
            self.border
        })
    }

    pub fn title(&self, focused: bool) -> Style {
        if focused {
            Style::default()
                .fg(self.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(self.muted)
        }
    }

    pub fn selection(&self) -> Style {
        Style::default()
            .bg(self.selection_bg)
            .fg(self.selection_fg)
            .add_modifier(Modifier::BOLD)
    }

    pub fn muted_style(&self) -> Style {
        Style::default().fg(self.muted)
    }

    pub fn accent_style(&self) -> Style {
        Style::default()
            .fg(self.accent)
            .add_modifier(Modifier::BOLD)
    }

    pub fn code_style(&self) -> Style {
        Style::default().fg(self.code_fg).bg(self.code_bg)
    }

    /// Native Jira status category lozenge colors.
    pub fn status_lozenge_style(&self, category: StatusCategory) -> Style {
        match category {
            StatusCategory::Todo => Style::default()
                .bg(Color::Rgb(66, 82, 110))
                .fg(Color::Rgb(223, 225, 230))
                .add_modifier(Modifier::BOLD),
            StatusCategory::InProgress => Style::default()
                .bg(Color::Rgb(0, 82, 204))
                .fg(Color::Rgb(222, 235, 255))
                .add_modifier(Modifier::BOLD),
            StatusCategory::Done => Style::default()
                .bg(Color::Rgb(0, 135, 90))
                .fg(Color::Rgb(227, 252, 239))
                .add_modifier(Modifier::BOLD),
        }
    }

    pub fn status_lozenge_span(&self, status_name: &str, category: StatusCategory) -> Span<'static> {
        let label = if status_name.trim().is_empty() {
            category.badge_label().to_string()
        } else {
            status_name.trim().to_ascii_uppercase()
        };
        Span::styled(format!(" {label} "), self.status_lozenge_style(category))
    }

    /// Issue Type icon & color: Epic `⚡`(Purple), Story `📗`(Green), Task `☑`(Blue), Bug `🐞`(Red), Subtask `↳`(Cyan).
    pub fn issue_type_badge(&self, issue_type: &IssueType) -> (&'static str, Color) {
        let lower = issue_type.name.trim().to_ascii_lowercase();
        if issue_type.epic || lower.contains("epic") || lower.contains("에픽") {
            ("⚡", self.purple)
        } else if issue_type.subtask || lower.contains("sub") || lower.contains("하위") {
            ("↳", self.confluence_teal)
        } else if lower.contains("bug") || lower.contains("버그") || lower.contains("defect") {
            ("🐞", self.error)
        } else if lower.contains("story") || lower.contains("스토리") {
            ("📗", self.success)
        } else {
            ("☑", self.accent)
        }
    }

    /// Priority icon & color: `⇈` Highest (Red), `↑` High (Orange), `=` Medium (Amber), `↓` Low (Blue), `⇊` Lowest (Slate).
    pub fn priority_badge(&self, priority: Option<&IssuePriority>) -> (&'static str, Color) {
        let Some(p) = priority else {
            return ("=", self.muted);
        };
        let lower = p.name.trim().to_ascii_lowercase();
        if lower.contains("highest") || lower.contains("blocker") || lower.contains("critical") || lower.contains("최고") {
            ("⇈", self.error)
        } else if lower.contains("high") || lower.contains("major") || lower.contains("높음") {
            ("↑", self.orange)
        } else if lower.contains("lowest") || lower.contains("trivial") || lower.contains("최저") {
            ("⇊", self.slate)
        } else if lower.contains("low") || lower.contains("minor") || lower.contains("낮음") {
            ("↓", self.accent)
        } else {
            ("=", self.amber)
        }
    }

    /// Panel accent color, header icon, and default title label.
    pub fn panel_meta(&self, kind: PanelKind) -> (Color, &'static str, &'static str) {
        match kind {
            PanelKind::Info => (self.info, "ℹ", "Info"),
            PanelKind::Note => (self.note, "📝", "Note"),
            PanelKind::Warning => (self.warning, "⚠", "Warning"),
            PanelKind::Success => (self.success, "✅", "Tip"),
            PanelKind::Error => (self.error, "⛔", "Error"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_lozenge_colors_match_atlassian_spec() {
        let theme = Theme::default();
        assert_eq!(
            theme.status_lozenge_style(StatusCategory::Todo).bg,
            Some(Color::Rgb(66, 82, 110))
        );
        assert_eq!(
            theme.status_lozenge_style(StatusCategory::InProgress).bg,
            Some(Color::Rgb(0, 82, 204))
        );
        assert_eq!(
            theme.status_lozenge_style(StatusCategory::Done).bg,
            Some(Color::Rgb(0, 135, 90))
        );

        let span = theme.status_lozenge_span("In Review", StatusCategory::InProgress);
        assert_eq!(span.content.as_ref(), " IN REVIEW ");
    }

    #[test]
    fn issue_type_and_priority_badges_map_correctly() {
        let theme = Theme::default();
        assert_eq!(
            theme.issue_type_badge(&IssueType {
                id: "1".into(),
                name: "Bug".into(),
                subtask: false,
                epic: false
            }).0,
            "🐞"
        );
        assert_eq!(
            theme.issue_type_badge(&IssueType {
                id: "2".into(),
                name: "Epic".into(),
                subtask: false,
                epic: true
            }).0,
            "⚡"
        );
        assert_eq!(
            theme.priority_badge(Some(&IssuePriority {
                id: "1".into(),
                name: "Highest".into()
            })).0,
            "⇈"
        );
        assert_eq!(
            theme.priority_badge(Some(&IssuePriority {
                id: "5".into(),
                name: "Lowest".into()
            })).0,
            "⇊"
        );
    }
}
