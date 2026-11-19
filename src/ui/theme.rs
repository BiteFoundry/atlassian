//! Semantic multi-depth color & style tokens for the Atlassian Jira + Confluence TUI.
//!
//! Supports Truecolor (`Midnight Dark` / `Daylight Light`), `Ansi256`, `Ansi16`, and `NO_COLOR`.

use ratatui::{
    style::{Color, Modifier, Style},
    text::Span,
};

use crate::{
    config::{Persona, ThemePreset},
    models::{IssuePriority, IssueType, PanelKind, StatusCategory},
};

/// Atlassian brand palette & TUI semantic tokens.
#[derive(Debug, Clone)]
pub struct Theme {
    pub no_color: bool,
    pub ansi16_only: bool,
    // Surfaces
    pub bg_canvas: Color,
    pub bg_sidebar: Color,
    pub bg_header: Color,
    pub bg_composer: Color,
    pub bg_elevated: Color,
    pub bg_selected: Color,
    pub bg_code: Color,
    pub bg_card: Color,
    // Foregrounds
    pub fg_primary: Color,
    pub fg_secondary: Color,
    pub fg_muted: Color,
    pub fg_subtle: Color,
    // Atlassian brand & semantic tokens
    pub jira_cobalt: Color,
    pub jira_pill_fg: Color,
    pub confluence_teal: Color,
    pub confluence_pill_fg: Color,
    pub accent: Color,
    pub accent_soft: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
    pub info: Color,
    pub note: Color,
    pub purple: Color,
    pub orange: Color,
    pub amber: Color,
    pub slate: Color,
    pub chip_bg: Color,
    pub chip_fg: Color,
    pub wiki_chip_bg: Color,
    pub mention_bg: Color,
    pub mention_fg: Color,
    // Status lozenge tokens adapted per palette
    pub lozenge_todo_bg: Color,
    pub lozenge_todo_fg: Color,
    pub lozenge_progress_bg: Color,
    pub lozenge_progress_fg: Color,
    pub lozenge_done_bg: Color,
    pub lozenge_done_fg: Color,
    // Legacy-compatible aliases used across TUI views & overlays
    pub fg: Color,
    pub muted: Color,
    pub border: Color,
    pub border_focus: Color,
    pub selection_bg: Color,
    pub selection_fg: Color,
    pub card_bg: Color,
    pub card_selected_bg: Color,
    pub code_fg: Color,
    pub code_bg: Color,
    pub divider: Color,
    pub toast: Color,
    pub author_palette: [Color; 8],
}

impl Default for Theme {
    fn default() -> Self {
        Self::midnight_dark()
    }
}

impl Theme {
    /// Detects terminal color capabilities when `preset == ThemePreset::Auto`, or applies explicit preset.
    pub fn detect(preset: ThemePreset) -> Self {
        if preset == ThemePreset::NoColor
            || std::env::var("NO_COLOR").is_ok_and(|v| !v.trim().is_empty())
        {
            return Self::no_color();
        }
        match preset {
            ThemePreset::Dark => Self::midnight_dark(),
            ThemePreset::Light => Self::daylight_light(),
            ThemePreset::Ansi256 => Self::ansi256(),
            ThemePreset::Ansi16 => Self::ansi16(),
            ThemePreset::NoColor => Self::no_color(),
            ThemePreset::Auto => {
                let colorterm = std::env::var("COLORTERM")
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if colorterm.contains("truecolor") || colorterm.contains("24bit") {
                    return Self::midnight_dark();
                }
                let term = std::env::var("TERM")
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if term == "dumb" {
                    return Self::no_color();
                }
                if term.contains("256color") {
                    return Self::ansi256();
                }
                if cfg!(windows) || std::env::var("WT_SESSION").is_ok() {
                    return Self::midnight_dark();
                }
                if term.is_empty() {
                    Self::ansi256()
                } else {
                    Self::ansi16()
                }
            }
        }
    }

    pub fn midnight_dark() -> Self {
        let bg_selected = Color::Rgb(42, 48, 72);
        let bg_card = Color::Rgb(30, 34, 50);
        let bg_code = Color::Rgb(29, 32, 48);
        let fg_primary = Color::Rgb(226, 230, 243);
        let fg_muted = Color::Rgb(110, 119, 153);
        let fg_subtle = Color::Rgb(70, 78, 107);
        let accent = Color::Rgb(122, 162, 247);
        let accent_soft = Color::Rgb(137, 221, 255);
        let code_fg = Color::Rgb(224, 175, 104);
        Self {
            no_color: false,
            ansi16_only: false,
            bg_canvas: Color::Reset,
            bg_sidebar: Color::Rgb(27, 30, 43),
            bg_header: Color::Rgb(31, 35, 51),
            bg_composer: Color::Rgb(27, 30, 43),
            bg_elevated: Color::Rgb(37, 42, 61),
            bg_selected,
            bg_code,
            bg_card,
            fg_primary,
            fg_secondary: Color::Rgb(169, 177, 208),
            fg_muted,
            fg_subtle,
            jira_cobalt: Color::Rgb(0, 82, 204),
            jira_pill_fg: Color::Rgb(240, 245, 255),
            confluence_teal: Color::Rgb(0, 184, 217),
            confluence_pill_fg: Color::Rgb(9, 30, 66),
            accent,
            accent_soft,
            success: Color::Rgb(115, 218, 202),
            warning: Color::Rgb(224, 175, 104),
            error: Color::Rgb(247, 118, 142),
            info: Color::Rgb(76, 154, 255),
            note: Color::Rgb(159, 107, 255),
            purple: Color::Rgb(187, 154, 247),
            orange: Color::Rgb(255, 158, 100),
            amber: Color::Rgb(224, 175, 104),
            slate: Color::Rgb(122, 134, 154),
            chip_bg: Color::Rgb(36, 52, 82),
            chip_fg: Color::Rgb(179, 212, 255),
            wiki_chip_bg: Color::Rgb(18, 56, 68),
            mention_bg: Color::Rgb(42, 60, 88),
            mention_fg: Color::Rgb(121, 226, 242),
            lozenge_todo_bg: Color::Rgb(66, 82, 110),
            lozenge_todo_fg: Color::Rgb(223, 225, 230),
            lozenge_progress_bg: Color::Rgb(0, 82, 204),
            lozenge_progress_fg: Color::Rgb(222, 235, 255),
            lozenge_done_bg: Color::Rgb(0, 135, 90),
            lozenge_done_fg: Color::Rgb(227, 252, 239),
            fg: fg_primary,
            muted: fg_muted,
            border: fg_subtle,
            border_focus: accent,
            selection_bg: bg_selected,
            selection_fg: fg_primary,
            card_bg: bg_card,
            card_selected_bg: bg_selected,
            code_fg,
            code_bg: bg_code,
            divider: fg_subtle,
            toast: accent_soft,
            author_palette: [
                Color::Rgb(158, 206, 106),
                Color::Rgb(224, 175, 104),
                Color::Rgb(187, 154, 247),
                Color::Rgb(125, 207, 255),
                Color::Rgb(247, 118, 142),
                Color::Rgb(115, 218, 202),
                Color::Rgb(255, 158, 100),
                Color::Rgb(180, 249, 248),
            ],
        }
    }

    pub fn daylight_light() -> Self {
        let bg_selected = Color::Rgb(220, 228, 247);
        let bg_card = Color::Rgb(240, 243, 250);
        let bg_code = Color::Rgb(235, 238, 245);
        let fg_primary = Color::Rgb(30, 36, 51);
        let fg_muted = Color::Rgb(107, 114, 128);
        let fg_subtle = Color::Rgb(203, 213, 225);
        let accent = Color::Rgb(0, 82, 204);
        let accent_soft = Color::Rgb(0, 141, 166);
        let code_fg = Color::Rgb(156, 54, 181);
        Self {
            no_color: false,
            ansi16_only: false,
            bg_canvas: Color::Reset,
            bg_sidebar: Color::Rgb(238, 241, 247),
            bg_header: Color::Rgb(228, 232, 242),
            bg_composer: Color::Rgb(255, 255, 255),
            bg_elevated: Color::Rgb(224, 229, 240),
            bg_selected,
            bg_code,
            bg_card,
            fg_primary,
            fg_secondary: Color::Rgb(74, 85, 104),
            fg_muted,
            fg_subtle,
            jira_cobalt: Color::Rgb(0, 82, 204),
            jira_pill_fg: Color::Rgb(255, 255, 255),
            confluence_teal: Color::Rgb(0, 141, 166),
            confluence_pill_fg: Color::Rgb(255, 255, 255),
            accent,
            accent_soft,
            success: Color::Rgb(0, 135, 90),
            warning: Color::Rgb(180, 83, 9),
            error: Color::Rgb(201, 42, 42),
            info: Color::Rgb(0, 82, 204),
            note: Color::Rgb(101, 84, 192),
            purple: Color::Rgb(103, 65, 217),
            orange: Color::Rgb(217, 72, 15),
            amber: Color::Rgb(180, 83, 9),
            slate: Color::Rgb(100, 116, 139),
            chip_bg: Color::Rgb(222, 235, 255),
            chip_fg: Color::Rgb(0, 82, 204),
            wiki_chip_bg: Color::Rgb(224, 247, 250),
            mention_bg: Color::Rgb(220, 236, 255),
            mention_fg: Color::Rgb(11, 114, 133),
            lozenge_todo_bg: Color::Rgb(223, 225, 230),
            lozenge_todo_fg: Color::Rgb(66, 82, 110),
            lozenge_progress_bg: Color::Rgb(222, 235, 255),
            lozenge_progress_fg: Color::Rgb(7, 71, 166),
            lozenge_done_bg: Color::Rgb(227, 252, 239),
            lozenge_done_fg: Color::Rgb(0, 102, 68),
            fg: fg_primary,
            muted: fg_muted,
            border: fg_subtle,
            border_focus: accent,
            selection_bg: bg_selected,
            selection_fg: fg_primary,
            card_bg: bg_card,
            card_selected_bg: bg_selected,
            code_fg,
            code_bg: bg_code,
            divider: fg_subtle,
            toast: accent,
            author_palette: [
                Color::Rgb(43, 138, 62),
                Color::Rgb(180, 83, 9),
                Color::Rgb(103, 65, 217),
                Color::Rgb(25, 113, 194),
                Color::Rgb(201, 42, 42),
                Color::Rgb(12, 133, 153),
                Color::Rgb(217, 72, 15),
                Color::Rgb(95, 61, 196),
            ],
        }
    }

    pub fn ansi256() -> Self {
        let bg_selected = Color::Indexed(237);
        let bg_card = Color::Indexed(235);
        let bg_code = Color::Indexed(235);
        let fg_primary = Color::Indexed(254);
        let fg_muted = Color::Indexed(244);
        let fg_subtle = Color::Indexed(239);
        let accent = Color::Indexed(111);
        let accent_soft = Color::Indexed(117);
        let code_fg = Color::Indexed(180);
        Self {
            no_color: false,
            ansi16_only: false,
            bg_canvas: Color::Reset,
            bg_sidebar: Color::Indexed(234),
            bg_header: Color::Indexed(235),
            bg_composer: Color::Indexed(234),
            bg_elevated: Color::Indexed(236),
            bg_selected,
            bg_code,
            bg_card,
            fg_primary,
            fg_secondary: Color::Indexed(249),
            fg_muted,
            fg_subtle,
            jira_cobalt: Color::Indexed(26),
            jira_pill_fg: Color::Indexed(255),
            confluence_teal: Color::Indexed(38),
            confluence_pill_fg: Color::Indexed(17),
            accent,
            accent_soft,
            success: Color::Indexed(79),
            warning: Color::Indexed(179),
            error: Color::Indexed(204),
            info: Color::Indexed(75),
            note: Color::Indexed(141),
            purple: Color::Indexed(141),
            orange: Color::Indexed(209),
            amber: Color::Indexed(179),
            slate: Color::Indexed(245),
            chip_bg: Color::Indexed(24),
            chip_fg: Color::Indexed(153),
            wiki_chip_bg: Color::Indexed(23),
            mention_bg: Color::Indexed(24),
            mention_fg: Color::Indexed(117),
            lozenge_todo_bg: Color::Indexed(240),
            lozenge_todo_fg: Color::Indexed(254),
            lozenge_progress_bg: Color::Indexed(26),
            lozenge_progress_fg: Color::Indexed(195),
            lozenge_done_bg: Color::Indexed(29),
            lozenge_done_fg: Color::Indexed(194),
            fg: fg_primary,
            muted: fg_muted,
            border: fg_subtle,
            border_focus: accent,
            selection_bg: bg_selected,
            selection_fg: fg_primary,
            card_bg: bg_card,
            card_selected_bg: bg_selected,
            code_fg,
            code_bg: bg_code,
            divider: fg_subtle,
            toast: accent_soft,
            author_palette: [
                Color::Indexed(114),
                Color::Indexed(179),
                Color::Indexed(141),
                Color::Indexed(81),
                Color::Indexed(204),
                Color::Indexed(79),
                Color::Indexed(209),
                Color::Indexed(117),
            ],
        }
    }

    pub fn ansi16() -> Self {
        Self {
            no_color: false,
            ansi16_only: true,
            bg_canvas: Color::Reset,
            bg_sidebar: Color::Reset,
            bg_header: Color::Reset,
            bg_composer: Color::Reset,
            bg_elevated: Color::Reset,
            bg_selected: Color::DarkGray,
            bg_code: Color::Black,
            bg_card: Color::Reset,
            fg_primary: Color::Reset,
            fg_secondary: Color::White,
            fg_muted: Color::DarkGray,
            fg_subtle: Color::DarkGray,
            jira_cobalt: Color::Blue,
            jira_pill_fg: Color::White,
            confluence_teal: Color::Cyan,
            confluence_pill_fg: Color::Black,
            accent: Color::Cyan,
            accent_soft: Color::LightCyan,
            success: Color::Green,
            warning: Color::Yellow,
            error: Color::Red,
            info: Color::Blue,
            note: Color::Magenta,
            purple: Color::LightMagenta,
            orange: Color::Yellow,
            amber: Color::Yellow,
            slate: Color::DarkGray,
            chip_bg: Color::Blue,
            chip_fg: Color::White,
            wiki_chip_bg: Color::Cyan,
            mention_bg: Color::Blue,
            mention_fg: Color::LightCyan,
            lozenge_todo_bg: Color::DarkGray,
            lozenge_todo_fg: Color::White,
            lozenge_progress_bg: Color::Blue,
            lozenge_progress_fg: Color::White,
            lozenge_done_bg: Color::Green,
            lozenge_done_fg: Color::Black,
            fg: Color::Reset,
            muted: Color::DarkGray,
            border: Color::DarkGray,
            border_focus: Color::Cyan,
            selection_bg: Color::DarkGray,
            selection_fg: Color::White,
            card_bg: Color::Reset,
            card_selected_bg: Color::DarkGray,
            code_fg: Color::Yellow,
            code_bg: Color::Black,
            divider: Color::DarkGray,
            toast: Color::LightCyan,
            author_palette: [
                Color::Green,
                Color::Yellow,
                Color::Magenta,
                Color::Cyan,
                Color::Red,
                Color::LightGreen,
                Color::LightRed,
                Color::LightBlue,
            ],
        }
    }

    pub fn no_color() -> Self {
        Self {
            no_color: true,
            ansi16_only: true,
            bg_canvas: Color::Reset,
            bg_sidebar: Color::Reset,
            bg_header: Color::Reset,
            bg_composer: Color::Reset,
            bg_elevated: Color::Reset,
            bg_selected: Color::Reset,
            bg_code: Color::Reset,
            bg_card: Color::Reset,
            fg_primary: Color::Reset,
            fg_secondary: Color::Reset,
            fg_muted: Color::Reset,
            fg_subtle: Color::Reset,
            jira_cobalt: Color::Reset,
            jira_pill_fg: Color::Reset,
            confluence_teal: Color::Reset,
            confluence_pill_fg: Color::Reset,
            accent: Color::Reset,
            accent_soft: Color::Reset,
            success: Color::Reset,
            warning: Color::Reset,
            error: Color::Reset,
            info: Color::Reset,
            note: Color::Reset,
            purple: Color::Reset,
            orange: Color::Reset,
            amber: Color::Reset,
            slate: Color::Reset,
            chip_bg: Color::Reset,
            chip_fg: Color::Reset,
            wiki_chip_bg: Color::Reset,
            mention_bg: Color::Reset,
            mention_fg: Color::Reset,
            lozenge_todo_bg: Color::Reset,
            lozenge_todo_fg: Color::Reset,
            lozenge_progress_bg: Color::Reset,
            lozenge_progress_fg: Color::Reset,
            lozenge_done_bg: Color::Reset,
            lozenge_done_fg: Color::Reset,
            fg: Color::Reset,
            muted: Color::Reset,
            border: Color::Reset,
            border_focus: Color::Reset,
            selection_bg: Color::Reset,
            selection_fg: Color::Reset,
            card_bg: Color::Reset,
            card_selected_bg: Color::Reset,
            code_fg: Color::Reset,
            code_bg: Color::Reset,
            divider: Color::Reset,
            toast: Color::Reset,
            author_palette: [Color::Reset; 8],
        }
    }

    /// Clean minimal-chrome persona pill (` JIRA ` / ` CONFLUENCE `), no emoji glyphs.
    pub fn persona_badge(&self, persona: Persona) -> Span<'static> {
        let label = match persona {
            Persona::Jira => " JIRA ",
            Persona::Confluence => " CONFLUENCE ",
        };
        if self.no_color {
            return Span::styled(
                label,
                Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD),
            );
        }
        match persona {
            Persona::Jira => Span::styled(
                label,
                Style::default()
                    .bg(self.jira_cobalt)
                    .fg(self.jira_pill_fg)
                    .add_modifier(Modifier::BOLD),
            ),
            Persona::Confluence => Span::styled(
                label,
                Style::default()
                    .bg(self.confluence_teal)
                    .fg(self.confluence_pill_fg)
                    .add_modifier(Modifier::BOLD),
            ),
        }
    }

    /// Returns a stable color for an author ID or display name.
    pub fn author(&self, key: &str) -> Color {
        if self.no_color {
            return Color::Reset;
        }
        let mut hash: u64 = 5381;
        for byte in key.bytes() {
            hash = hash.wrapping_mul(33).wrapping_add(byte as u64);
        }
        self.author_palette[(hash as usize) % self.author_palette.len()]
    }

    pub fn surface_style(&self, bg: Color) -> Style {
        if self.no_color {
            Style::default()
        } else {
            Style::default().bg(bg).fg(self.fg_primary)
        }
    }

    pub fn border_style(&self, focused: bool) -> Style {
        if self.no_color {
            return if focused {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default().add_modifier(Modifier::DIM)
            };
        }
        let color = if focused {
            self.border_focus
        } else {
            self.border
        };
        Style::default().fg(color)
    }

    pub fn title(&self, focused: bool) -> Style {
        if focused {
            if self.no_color {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(self.accent)
                    .add_modifier(Modifier::BOLD)
            }
        } else {
            self.muted_style()
        }
    }

    /// Non-destructive selection style: tints background without clobbering child span foreground colors.
    pub fn selection(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else if self.ansi16_only {
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::default().bg(self.bg_selected)
        }
    }

    /// Single-line list/navigation selection (uses `REVERSED` in `no_color` / `ansi16` modes).
    pub fn nav_selection(&self) -> Style {
        if self.no_color || self.ansi16_only {
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::default()
                .bg(self.bg_selected)
                .fg(self.fg_primary)
                .add_modifier(Modifier::BOLD)
        }
    }

    pub fn muted_style(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(self.fg_muted)
        }
    }

    pub fn subtle_style(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(self.fg_subtle)
        }
    }

    pub fn accent_style(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(self.accent)
                .add_modifier(Modifier::BOLD)
        }
    }

    pub fn code_style(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(self.code_fg).bg(self.bg_code)
        }
    }

    pub fn quote_style(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::ITALIC)
        } else {
            Style::default()
                .fg(self.fg_secondary)
                .add_modifier(Modifier::ITALIC)
        }
    }

    /// Native Jira status category lozenge colors adapted across all 5 palettes.
    pub fn status_lozenge_style(&self, category: StatusCategory) -> Style {
        if self.no_color {
            return match category {
                StatusCategory::Todo => Style::default().add_modifier(Modifier::BOLD),
                StatusCategory::InProgress | StatusCategory::Done => {
                    Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
                }
            };
        }
        match category {
            StatusCategory::Todo => Style::default()
                .bg(self.lozenge_todo_bg)
                .fg(self.lozenge_todo_fg)
                .add_modifier(Modifier::BOLD),
            StatusCategory::InProgress => Style::default()
                .bg(self.lozenge_progress_bg)
                .fg(self.lozenge_progress_fg)
                .add_modifier(Modifier::BOLD),
            StatusCategory::Done => Style::default()
                .bg(self.lozenge_done_bg)
                .fg(self.lozenge_done_fg)
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

    /// Issue Type icon & color: Epic `⚡`, Story `◆`, Task `☑`, Bug `●`, Subtask `↳`.
    pub fn issue_type_badge(&self, issue_type: &IssueType) -> (&'static str, Color) {
        let lower = issue_type.name.trim().to_ascii_lowercase();
        let (icon, color) = if issue_type.epic || lower.contains("epic") {
            ("⚡", self.purple)
        } else if issue_type.subtask || lower.contains("sub") {
            ("↳", self.confluence_teal)
        } else if lower.contains("bug") || lower.contains("defect") {
            ("🐞", self.error)
        } else if lower.contains("story") {
            ("◆", self.success)
        } else {
            ("☑", self.accent)
        };
        (icon, if self.no_color { Color::Reset } else { color })
    }

    /// Priority icon & color: `⇈` Highest, `↑` High, `=` Medium, `↓` Low, `⇊` Lowest.
    pub fn priority_badge(&self, priority: Option<&IssuePriority>) -> (&'static str, Color) {
        let Some(p) = priority else {
            return ("=", self.fg_muted);
        };
        let lower = p.name.trim().to_ascii_lowercase();
        let (icon, color) = if lower.contains("highest")
            || lower.contains("blocker")
            || lower.contains("critical")
        {
            ("⇈", self.error)
        } else if lower.contains("high") || lower.contains("major") {
            ("↑", self.orange)
        } else if lower.contains("lowest") || lower.contains("trivial") {
            ("⇊", self.slate)
        } else if lower.contains("low") || lower.contains("minor") {
            ("↓", self.accent)
        } else {
            ("=", self.amber)
        };
        (icon, if self.no_color { Color::Reset } else { color })
    }

    /// ADF callout panel accent color, header glyph, and default English label.
    pub fn panel_meta(&self, kind: PanelKind) -> (Color, &'static str, &'static str) {
        let (color, icon, label) = match kind {
            PanelKind::Info => (self.info, "ℹ", "Info"),
            PanelKind::Note => (self.note, "✎", "Note"),
            PanelKind::Warning => (self.warning, "⚠", "Warning"),
            PanelKind::Success => (self.success, "✓", "Tip"),
            PanelKind::Error => (self.error, "✕", "Error"),
        };
        (
            if self.no_color { Color::Reset } else { color },
            icon,
            label,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_lozenge_colors_match_atlassian_spec() {
        let theme = Theme::midnight_dark();
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
    fn persona_badges_are_clean_pills_without_emoji() {
        for preset in [
            ThemePreset::Dark,
            ThemePreset::Light,
            ThemePreset::Ansi256,
            ThemePreset::Ansi16,
            ThemePreset::NoColor,
        ] {
            let theme = Theme::detect(preset);
            assert_eq!(theme.persona_badge(Persona::Jira).content.as_ref(), " JIRA ");
            assert_eq!(
                theme.persona_badge(Persona::Confluence).content.as_ref(),
                " CONFLUENCE "
            );
        }
    }

    #[test]
    fn issue_type_and_priority_badges_map_correctly() {
        let theme = Theme::default();
        assert_eq!(
            theme
                .issue_type_badge(&IssueType {
                    id: "1".into(),
                    name: "Bug".into(),
                    subtask: false,
                    epic: false,
                })
                .0,
            "🐞"
        );
        assert_eq!(
            theme
                .issue_type_badge(&IssueType {
                    id: "2".into(),
                    name: "Epic".into(),
                    subtask: false,
                    epic: true,
                })
                .0,
            "⚡"
        );
        assert_eq!(
            theme
                .priority_badge(Some(&IssuePriority {
                    id: "1".into(),
                    name: "Highest".into(),
                }))
                .0,
            "⇈"
        );
        assert_eq!(
            theme
                .priority_badge(Some(&IssuePriority {
                    id: "5".into(),
                    name: "Lowest".into(),
                }))
                .0,
            "⇊"
        );
    }

    #[test]
    fn all_five_presets_construct_cleanly_with_distinct_palettes() {
        let dark = Theme::midnight_dark();
        let light = Theme::daylight_light();
        let a256 = Theme::ansi256();
        let a16 = Theme::ansi16();
        let nc = Theme::no_color();

        assert!(!dark.no_color && !dark.ansi16_only);
        assert!(!light.no_color && !light.ansi16_only);
        assert!(!a256.no_color && !a256.ansi16_only);
        assert!(!a16.no_color && a16.ansi16_only);
        assert!(nc.no_color && nc.ansi16_only);

        assert_ne!(dark.bg_sidebar, light.bg_sidebar);
        assert_eq!(dark.selection().fg, None);
        assert_eq!(dark.selection().bg, Some(dark.bg_selected));
        assert_eq!(nc.author("alice"), Color::Reset);
        assert_eq!(dark.author("alice"), dark.author("alice"));
    }
}
