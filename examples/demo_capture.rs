//! Captures 8 real `ratatui::TestBackend` frames (120x34) from `atlassian_cli::ui::draw`
//! with 100% fictional English demo data and invokes the shared studio-grade GIF renderer.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use atlassian_cli::{
    atlassian::AtlassianClient,
    auth::{AuthManager, StoredCredentials},
    config::{
        AppConfig, DensityMode, DeploymentKind, ImageProtocolPreference, Persona, SiteProfile,
        ThemePreset,
    },
    models::{
        AttachmentItem, BoardColumn, BoardSummary, ConfluenceComment, DocBlock, InlineNode,
        InlineStyle, IssueComment, IssueDetail, IssuePriority, IssueStatus, IssueSummary,
        IssueType, LinkedConfluencePage, PageAncestor, PageDetail, PageSummary, PanelKind,
        ProjectSummary, SiteScope, SpaceSummary, SprintSummary, StatusCategory, SubtaskSummary,
        TaskItem, TocEntry, UserSummary,
    },
    storage::{CacheCounts, Storage},
    sync,
    ui::{
        self,
        app::{AppState, PaneFocus, SiteKey, UiState, WorkspaceTab},
        overlays::{
            command_palette::CommandPalette,
            file_picker::{FilePickerEntry, FilePickerMode, FilePickerOverlay},
            links::LinksOverlay,
            path_input::UploadTarget,
            settings::SettingsOverlay,
            site_switcher::{SiteSwitcherEntry, SiteSwitcherOverlay},
            Overlay,
        },
        theme::Theme,
    },
};
use ratatui::{
    backend::TestBackend,
    buffer::Buffer,
    style::{Color, Modifier},
    Terminal,
};
use serde_json::{json, Value};
use tokio::sync::mpsc;

const WIDTH: u16 = 120;
const HEIGHT: u16 = 34;

/// GIF renderer script: `DEMO_RENDERER_SCRIPT`, else `<repo>/scripts/render_tui_gif.py`.
///
/// The renderer is not part of this repository; the fallback documents where it would go.
fn renderer_script() -> PathBuf {
    std::env::var_os("DEMO_RENDERER_SCRIPT")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("scripts")
                .join("render_tui_gif.py")
        })
}

/// Per-frame PNG preview directory: `DEMO_PREVIEW_DIR`, else `<repo>/target/demo-previews`.
fn preview_dir() -> PathBuf {
    std::env::var_os("DEMO_PREVIEW_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join("demo-previews")
        })
}

fn color_to_json(c: Color) -> Value {
    match c {
        Color::Reset => Value::Null,
        Color::Rgb(r, g, b) => json!([r, g, b]),
        Color::Black => json!("Black"),
        Color::Red => json!("Red"),
        Color::Green => json!("Green"),
        Color::Yellow => json!("Yellow"),
        Color::Blue => json!("Blue"),
        Color::Magenta => json!("Magenta"),
        Color::Cyan => json!("Cyan"),
        Color::Gray => json!("Gray"),
        Color::DarkGray => json!("DarkGray"),
        Color::LightRed => json!("LightRed"),
        Color::LightGreen => json!("LightGreen"),
        Color::LightYellow => json!("LightYellow"),
        Color::LightBlue => json!("LightBlue"),
        Color::LightMagenta => json!("LightMagenta"),
        Color::LightCyan => json!("LightCyan"),
        Color::White => json!("White"),
        Color::Indexed(idx) => {
            let (r, g, b) = xterm256_to_rgb(idx);
            json!([r, g, b])
        }
    }
}

fn xterm256_to_rgb(idx: u8) -> (u8, u8, u8) {
    if idx < 16 {
        const ANSI16: [(u8, u8, u8); 16] = [
            (24, 26, 33),
            (247, 118, 142),
            (158, 206, 106),
            (224, 175, 104),
            (122, 162, 247),
            (187, 154, 247),
            (125, 207, 255),
            (169, 177, 214),
            (86, 95, 137),
            (255, 140, 160),
            (175, 224, 128),
            (240, 200, 130),
            (148, 184, 255),
            (205, 176, 255),
            (155, 222, 255),
            (240, 244, 252),
        ];
        return ANSI16[idx as usize];
    }
    if idx >= 232 {
        let v = 8 + (idx - 232) * 10;
        return (v, v, v);
    }
    let n = idx - 16;
    let r = n / 36;
    let g = (n % 36) / 6;
    let b = n % 6;
    let map = |c: u8| if c == 0 { 0 } else { 55 + c * 40 };
    (map(r), map(g), map(b))
}

fn modifier_flags(m: Modifier) -> u8 {
    let mut flags = 0u8;
    if m.contains(Modifier::BOLD) {
        flags |= 1;
    }
    if m.contains(Modifier::ITALIC) {
        flags |= 2;
    }
    if m.contains(Modifier::DIM) {
        flags |= 4;
    }
    if m.contains(Modifier::UNDERLINED) {
        flags |= 8;
    }
    if m.contains(Modifier::REVERSED) {
        flags |= 16;
    }
    flags
}

fn cell_single_char(symbol: &str, is_bold: bool) -> char {
    let ch = symbol
        .chars()
        .find(|c| *c != '\u{fe0e}' && *c != '\u{fe0f}' && *c != '\u{200d}')
        .unwrap_or(' ');
    match ch {
        'ℹ' => 'i',
        '⇈' => '▲',
        '★' => '*',
        '☐' => '▫',
        '☑' => '■',
        '🐞' | '🐛' => '●',
        '▎' | '▏' => '▌',
        '✓' | '✔' if is_bold => '▪',
        '◆' if is_bold => '▪',
        other => other,
    }
}

fn buffer_to_rows(buf: &Buffer, width: u16, height: u16) -> Vec<Value> {
    let mut rows = Vec::with_capacity(height as usize);
    for y in 0..height {
        let mut runs: Vec<Value> = Vec::new();
        let mut cur_text = String::new();
        let mut cur_style: Option<(Color, Color, u8)> = None;

        for x in 0..width {
            let cell = &buf[(x, y)];
            let flags = modifier_flags(cell.modifier);
            let ch = cell_single_char(cell.symbol(), (flags & 1) != 0);
            let style_key = (cell.fg, cell.bg, flags);
            if cur_style == Some(style_key) {
                cur_text.push(ch);
            } else {
                if let Some((fg, bg, flags)) = cur_style {
                    runs.push(json!([cur_text, color_to_json(fg), color_to_json(bg), flags]));
                    cur_text = String::new();
                }
                cur_text.push(ch);
                cur_style = Some(style_key);
            }
        }
        if let Some((fg, bg, flags)) = cur_style {
            runs.push(json!([cur_text, color_to_json(fg), color_to_json(bg), flags]));
        }
        rows.push(Value::Array(runs));
    }
    rows
}

fn capture_frame(
    app: &AppState,
    ui: &mut UiState,
    duration_ms: u64,
    caption: &str,
    key_badge: &str,
) -> Value {
    let backend = TestBackend::new(WIDTH, HEIGHT);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|f| {
            let _ = ui::draw(f, app, ui);
        })
        .expect("draw frame");
    let rows = buffer_to_rows(terminal.backend().buffer(), WIDTH, HEIGHT);
    json!({
        "duration_ms": duration_ms,
        "caption": caption,
        "key_badge": key_badge,
        "rows": rows,
    })
}

fn user(id: &str, name: &str, email: &str) -> UserSummary {
    UserSummary {
        id: id.into(),
        display_name: name.into(),
        email: Some(email.into()),
        avatar_url: None,
        active: true,
    }
}

fn plain(text: &str) -> InlineNode {
    InlineNode::Text {
        text: text.into(),
        style: InlineStyle::default(),
    }
}

fn bold(text: &str) -> InlineNode {
    InlineNode::Text {
        text: text.into(),
        style: InlineStyle {
            bold: true,
            ..Default::default()
        },
    }
}

fn code(text: &str) -> InlineNode {
    InlineNode::Text {
        text: text.into(),
        style: InlineStyle {
            code: true,
            ..Default::default()
        },
    }
}

fn link(text: &str, url: &str) -> InlineNode {
    InlineNode::Text {
        text: text.into(),
        style: InlineStyle {
            link_url: Some(url.into()),
            ..Default::default()
        },
    }
}

fn make_issue(
    key: &str,
    summary: &str,
    status_name: &str,
    cat: StatusCategory,
    priority: &str,
    issue_type: &str,
    assignee: UserSummary,
    points: f64,
    comments: usize,
    attachments: usize,
    labels: &[&str],
) -> IssueSummary {
    IssueSummary {
        id: format!("id-{key}"),
        key: key.into(),
        project_key: "ENG".into(),
        summary: summary.into(),
        status: IssueStatus {
            id: status_name.to_ascii_lowercase().replace(' ', "_"),
            name: status_name.into(),
            category: cat,
        },
        priority: Some(IssuePriority {
            id: priority.to_ascii_lowercase(),
            name: priority.into(),
        }),
        issue_type: IssueType {
            id: issue_type.to_ascii_lowercase(),
            name: issue_type.into(),
            subtask: false,
            epic: false,
        },
        assignee: Some(assignee),
        reporter: Some(user("u-alex", "Alex Rivera", "alex.rivera@acme.example")),
        parent_key: Some("ENG-90".into()),
        parent_summary: Some("Platform Reliability Q4".into()),
        labels: labels.iter().map(|s| (*s).to_string()).collect(),
        story_points: Some(points),
        sprint_id: Some(42),
        sprint_name: Some("Sprint 42 — Core Scale".into()),
        comment_count: comments,
        attachment_count: attachments,
        created_at: "2025-11-10T09:15:00Z".into(),
        updated_at: "2025-11-19T14:22:00Z".into(),
        web_url: format!("https://acme.atlassian.net/browse/{key}"),
    }
}

fn build_demo_state() -> (AppState, UiState) {
    // Isolate config/download paths so no host user path ever appears in Settings
    std::env::set_var("ATLASSIAN_CONFIG_DIR", r"C:\acme\config\atlassian-cli");

    let cloud_site = SiteProfile {
        id: "acme-cloud".into(),
        name: "Acme Cloud".into(),
        site_url: "https://acme.atlassian.net".into(),
        confluence_url: "https://acme.atlassian.net/wiki".into(),
        deployment: DeploymentKind::Cloud,
        cloud_id: Some("a1b2c3d4-acme-cloud".into()),
        account_id: "acme-oauth".into(),
        enabled: true,
        scopes: vec!["read:jira-work".into(), "read:confluence-content.all".into()],
        confluence_account_id: None,
    };
    let dc_site = SiteProfile {
        id: "acme-dc".into(),
        name: "Acme DC".into(),
        site_url: "https://jira.acme.internal".into(),
        confluence_url: "https://wiki.acme.internal".into(),
        deployment: DeploymentKind::DataCenter,
        cloud_id: None,
        account_id: "acme-dc-pat".into(),
        enabled: true,
        scopes: vec![],
        confluence_account_id: None,
    };

    let cfg = AppConfig {
        site_url: "https://acme.atlassian.net".into(),
        confluence_url: "https://acme.atlassian.net/wiki".into(),
        deployment: DeploymentKind::Cloud,
        sites: vec![cloud_site, dc_site],
        active_site: None,
        default_site: Some("acme-cloud".into()),
        theme: ThemePreset::Dark,
        density: DensityMode::Cozy,
        image_protocol: ImageProtocolPreference::Auto,
        inline_images: true,
        notify: true,
        show_nav_rail: true,
        auto_detail_drawer: true,
        download_dir: Some(PathBuf::from(r"C:\acme\Downloads\atlassian")),
        ..Default::default()
    };

    let auth = AuthManager::with_credentials(
        "demo",
        StoredCredentials::ApiToken {
            site_url: "https://acme.atlassian.net".into(),
            email: "alex.rivera@acme.example".into(),
            api_token: "demo-placeholder-token".into(),
            deployment: DeploymentKind::Cloud,
        },
    );
    let client = AtlassianClient::new(cfg.clone(), auth.clone()).expect("client");
    let storage = Storage::open_in_memory().expect("in-memory sqlite");
    let (core_tx, _) = mpsc::unbounded_channel();
    let sync_ctrl = sync::spawn_sync_engine(cfg.clone(), client.clone(), storage.clone(), core_tx);
    let (ui_tx, _) = mpsc::unbounded_channel();

    let mut app = AppState::new_with_persona(
        cfg.clone(),
        Persona::Jira,
        auth,
        client,
        storage,
        sync_ctrl,
        ui_tx,
    );
    let mut ui = UiState::from_config_with_persona(&cfg, Persona::Jira);
    ui.theme = Theme::midnight_dark();
    app.site_scope = SiteScope::All;

    let alex = user("u-alex", "Alex Rivera", "alex.rivera@acme.example");
    let maya = user("u-maya", "Maya Chen", "maya.chen@acme.example");
    let liam = user("u-liam", "Liam Brooks", "liam.brooks@acme.example");
    let priya = user("u-priya", "Priya Nair", "priya.nair@acme.example");
    app.me = Some(alex.clone());

    app.projects = vec![
        ProjectSummary {
            id: "10001".into(),
            key: "ENG".into(),
            name: "Core Platform".into(),
            project_type: "software".into(),
            lead_name: Some("Alex Rivera".into()),
            avatar_url: None,
            pinned: true,
        },
        ProjectSummary {
            id: "10002".into(),
            key: "INFRA".into(),
            name: "Cloud Infrastructure".into(),
            project_type: "software".into(),
            lead_name: Some("Priya Nair".into()),
            avatar_url: None,
            pinned: false,
        },
    ];

    app.boards = vec![
        BoardSummary {
            id: 11,
            name: "Platform Core — Sprint 42".into(),
            board_type: "scrum".into(),
            project_key: Some("ENG".into()),
            project_name: Some("Core Platform".into()),
            columns: vec![
                BoardColumn {
                    name: "To Do".into(),
                    status_ids: vec!["to_do".into()],
                    status_names: vec!["To Do".into()],
                    category_hint: StatusCategory::Todo,
                },
                BoardColumn {
                    name: "In Progress".into(),
                    status_ids: vec!["in_progress".into()],
                    status_names: vec!["In Progress".into()],
                    category_hint: StatusCategory::InProgress,
                },
                BoardColumn {
                    name: "In Review".into(),
                    status_ids: vec!["in_review".into()],
                    status_names: vec!["In Review".into()],
                    category_hint: StatusCategory::InProgress,
                },
                BoardColumn {
                    name: "Done".into(),
                    status_ids: vec!["done".into()],
                    status_names: vec!["Done".into()],
                    category_hint: StatusCategory::Done,
                },
            ],
        },
        BoardSummary {
            id: 24,
            name: "DC Ops Kanban".into(),
            board_type: "kanban".into(),
            project_key: Some("INFRA".into()),
            project_name: Some("Cloud Infrastructure".into()),
            columns: vec![],
        },
    ];
    app.board_site_ids = vec!["acme-cloud".into(), "acme-dc".into()];

    app.sprints = vec![
        SprintSummary {
            id: 42,
            board_id: 11,
            origin_board_id: None,
            name: "Sprint 42 — Core Scale".into(),
            state: "active".into(),
            start_date: Some("2025-11-10".into()),
            end_date: Some("2025-11-24".into()),
            goal: Some("Zero-downtime multi-region failover & FTS5 query latency <15ms".into()),
        },
        SprintSummary {
            id: 43,
            board_id: 11,
            origin_board_id: None,
            name: "Sprint 43 — Hardening".into(),
            state: "future".into(),
            start_date: Some("2025-11-25".into()),
            end_date: Some("2025-12-09".into()),
            goal: None,
        },
    ];

    let issues = vec![
        make_issue(
            "ENG-101",
            "Blue/green EU gateway cutover",
            "In Progress",
            StatusCategory::InProgress,
            "Highest",
            "Story",
            alex.clone(),
            5.0,
            4,
            2,
            &["gateway", "reliability", "q4"],
        ),
        make_issue(
            "ENG-102",
            "Token-bucket GraphQL limiter",
            "To Do",
            StatusCategory::Todo,
            "High",
            "Story",
            maya.clone(),
            3.0,
            2,
            1,
            &["edge", "security"],
        ),
        make_issue(
            "ENG-103",
            "Fix FTS5 WAL burst sync stall",
            "To Do",
            StatusCategory::Todo,
            "High",
            "Bug",
            liam.clone(),
            2.0,
            3,
            0,
            &["sqlite", "perf"],
        ),
        make_issue(
            "ENG-104",
            "Cloud OAuth + DC health probe",
            "In Progress",
            StatusCategory::InProgress,
            "High",
            "Task",
            priya.clone(),
            3.0,
            5,
            1,
            &["auth", "federation"],
        ),
        make_issue(
            "ENG-105",
            "OTel spans for webhook retries",
            "In Review",
            StatusCategory::InProgress,
            "Medium",
            "Story",
            maya.clone(),
            3.0,
            6,
            1,
            &["observability"],
        ),
        make_issue(
            "ENG-106",
            "Rotate SAML certs on DC nodes",
            "In Review",
            StatusCategory::InProgress,
            "High",
            "Task",
            liam.clone(),
            2.0,
            2,
            0,
            &["security", "dc"],
        ),
        make_issue(
            "ENG-107",
            "Markdown export for Wiki v2",
            "Done",
            StatusCategory::Done,
            "Medium",
            "Story",
            alex.clone(),
            3.0,
            4,
            1,
            &["confluence", "cli"],
        ),
        make_issue(
            "ENG-108",
            "Trim npm binary cold-start",
            "Done",
            StatusCategory::Done,
            "Low",
            "Task",
            priya.clone(),
            2.0,
            1,
            0,
            &["release", "npm"],
        ),
        make_issue(
            "ENG-109",
            "Add retry jitter for JQL sync",
            "To Do",
            StatusCategory::Todo,
            "Medium",
            "Task",
            alex.clone(),
            2.0,
            1,
            0,
            &["sync"],
        ),
        make_issue(
            "ENG-110",
            "Cache Confluence page macros",
            "In Progress",
            StatusCategory::InProgress,
            "Medium",
            "Story",
            maya.clone(),
            3.0,
            2,
            0,
            &["confluence", "cache"],
        ),
    ];

    app.board_issues = issues.clone();
    app.issues = issues.clone();
    app.issue_site_ids = vec![
        "acme-cloud".into(),
        "acme-cloud".into(),
        "acme-cloud".into(),
        "acme-cloud".into(),
        "acme-cloud".into(),
        "acme-dc".into(),
        "acme-cloud".into(),
        "acme-dc".into(),
        "acme-cloud".into(),
        "acme-dc".into(),
    ];

    app.active_issue = Some(IssueDetail {
        summary: issues[0].clone(),
        description_raw: String::new(),
        description_markdown:
            "Roll out weighted DNS cutover across `eu-central-1` with automated rollback triggers."
                .into(),
        description_blocks: vec![
            DocBlock::Panel {
                kind: PanelKind::Info,
                title: Some("Cutover Guardrails".into()),
                blocks: vec![DocBlock::Paragraph {
                    spans: vec![
                        plain("Keep p99 latency below "),
                        bold("45ms"),
                        plain(" and error budget burn <0.1% during canary ramp. See "),
                        link(
                            "Migration Runbook",
                            "https://acme.atlassian.net/wiki/spaces/ENG/pages/40211",
                        ),
                        plain("."),
                    ],
                }],
            },
            DocBlock::Heading {
                level: 2,
                spans: vec![plain("Rollout Checklist")],
            },
            DocBlock::TaskList {
                items: vec![
                    TaskItem {
                        checked: true,
                        content: vec![plain("Pre-warm Envoy clusters & validate mTLS certs")],
                    },
                    TaskItem {
                        checked: true,
                        content: vec![
                            plain("Shift 25% canary traffic via "),
                            code("gatewayctl route --weight 25"),
                        ],
                    },
                    TaskItem {
                        checked: false,
                        content: vec![
                            plain("Promote to 100% and archive legacy DC pool "),
                            InlineNode::IssueRef {
                                key: "ENG-104".into(),
                            },
                        ],
                    },
                ],
            },
            DocBlock::Paragraph {
                spans: vec![
                    plain("Grafana dashboard: "),
                    link(
                        "EU Gateway SLOs",
                        "https://metrics.acme.example/d/eu-gw-slo",
                    ),
                    plain("  ·  PR: "),
                    link(
                        "acme/platform#1482",
                        "https://github.com/acme-corp/platform/pull/1482",
                    ),
                ],
            },
        ],
        fix_versions: vec!["v2.14.0".into()],
        components: vec!["Edge Gateway".into(), "Traffic Mesh".into()],
        subtasks: vec![
            SubtaskSummary {
                key: "ENG-109".into(),
                summary: "Verify synthetic health checks on eu-central-1b".into(),
                status: IssueStatus {
                    id: "done".into(),
                    name: "Done".into(),
                    category: StatusCategory::Done,
                },
                issue_type: "Sub-task".into(),
                assignee_name: Some("Maya Chen".into()),
            },
            SubtaskSummary {
                key: "ENG-110".into(),
                summary: "Automate DNS weight rollback on 5xx spike".into(),
                status: IssueStatus {
                    id: "in_progress".into(),
                    name: "In Progress".into(),
                    category: StatusCategory::InProgress,
                },
                issue_type: "Sub-task".into(),
                assignee_name: Some("Alex Rivera".into()),
            },
        ],
        linked_issues: vec![],
        linked_pages: vec![LinkedConfluencePage {
            page_id: "40211".into(),
            title: "Zero-Downtime Migration Runbook".into(),
            space_key: Some("ENG".into()),
            web_url: "https://acme.atlassian.net/wiki/spaces/ENG/pages/40211".into(),
        }],
        attachments: vec![
            AttachmentItem {
                id: "att-1".into(),
                filename: "eu-gateway-topology.png".into(),
                mime_type: "image/png".into(),
                size: 184_320,
                author_name: Some("Alex Rivera".into()),
                created_at: "2025-11-18T11:30:00Z".into(),
                content_url: "https://acme.atlassian.net/rest/api/3/attachment/content/att-1"
                    .into(),
                thumbnail_url: None,
            },
            AttachmentItem {
                id: "att-2".into(),
                filename: "canary-latency-p99.csv".into(),
                mime_type: "text/csv".into(),
                size: 42_110,
                author_name: Some("Maya Chen".into()),
                created_at: "2025-11-19T09:05:00Z".into(),
                content_url: "https://acme.atlassian.net/rest/api/3/attachment/content/att-2"
                    .into(),
                thumbnail_url: None,
            },
        ],
        comments: vec![
            IssueComment {
                id: "c-1".into(),
                issue_key: "ENG-101".into(),
                author: maya.clone(),
                body_raw: String::new(),
                body_markdown: "Canary at 25% held steady at 18ms p99 over the last 4 hours. Ready for 50% bump.".into(),
                blocks: vec![DocBlock::Paragraph {
                    spans: vec![plain(
                        "Canary at 25% held steady at 18ms p99 over the last 4 hours. Ready for 50% bump.",
                    )],
                }],
                created_at: "2025-11-19T12:10:00Z".into(),
                updated_at: "2025-11-19T12:10:00Z".into(),
            },
            IssueComment {
                id: "c-2".into(),
                issue_key: "ENG-101".into(),
                author: priya.clone(),
                body_raw: String::new(),
                body_markdown: "Confirmed Data Center mirror replication lag is <120ms across both regions.".into(),
                blocks: vec![DocBlock::Paragraph {
                    spans: vec![plain(
                        "Confirmed Data Center mirror replication lag is <120ms across both regions.",
                    )],
                }],
                created_at: "2025-11-19T14:05:00Z".into(),
                updated_at: "2025-11-19T14:05:00Z".into(),
            },
        ],
        incomplete: vec![],
    });

    // Confluence Spaces & Hierarchical Page Tree
    app.spaces = vec![
        SpaceSummary {
            id: "sp-eng".into(),
            key: "ENG".into(),
            name: "Engineering".into(),
            space_type: "global".into(),
            homepage_id: Some("40200".into()),
            description: Some("Architecture & Runbooks".into()),
            web_url: "https://acme.atlassian.net/wiki/spaces/ENG".into(),
            pinned: true,
        },
        SpaceSummary {
            id: "sp-ops".into(),
            key: "OPS".into(),
            name: "DC Operations".into(),
            space_type: "global".into(),
            homepage_id: Some("50100".into()),
            description: Some("On-prem Data Center playbooks".into()),
            web_url: "https://wiki.acme.internal/display/OPS".into(),
            pinned: false,
        },
    ];
    app.space_site_ids = vec!["acme-cloud".into(), "acme-dc".into()];

    let root_arch = PageSummary {
        id: "40200".into(),
        space_id: "sp-eng".into(),
        space_key: "ENG".into(),
        parent_id: None,
        title: "System Architecture".into(),
        status: "current".into(),
        version_number: 14,
        author_name: Some("Alex Rivera".into()),
        updated_at: "2025-11-19T13:45:00Z".into(),
        position: 0,
        has_children: Some(true),
        web_url: "https://acme.atlassian.net/wiki/spaces/ENG/pages/40200".into(),
    };
    let child_runbook = PageSummary {
        id: "40211".into(),
        space_id: "sp-eng".into(),
        space_key: "ENG".into(),
        parent_id: Some("40200".into()),
        title: "Zero-Downtime Migration Runbook".into(),
        status: "current".into(),
        version_number: 9,
        author_name: Some("Alex Rivera".into()),
        updated_at: "2025-11-19T14:20:00Z".into(),
        position: 0,
        has_children: Some(false),
        web_url: "https://acme.atlassian.net/wiki/spaces/ENG/pages/40211".into(),
    };
    let child_gateway = PageSummary {
        id: "40212".into(),
        space_id: "sp-eng".into(),
        space_key: "ENG".into(),
        parent_id: Some("40200".into()),
        title: "API Gateway & Rate Limits".into(),
        status: "current".into(),
        version_number: 6,
        author_name: Some("Maya Chen".into()),
        updated_at: "2025-11-18T16:10:00Z".into(),
        position: 1,
        has_children: Some(false),
        web_url: "https://acme.atlassian.net/wiki/spaces/ENG/pages/40212".into(),
    };
    let root_oncall = PageSummary {
        id: "40230".into(),
        space_id: "sp-eng".into(),
        space_key: "ENG".into(),
        parent_id: None,
        title: "Incident Response Playbook".into(),
        status: "current".into(),
        version_number: 11,
        author_name: Some("Priya Nair".into()),
        updated_at: "2025-11-17T08:00:00Z".into(),
        position: 1,
        has_children: Some(true),
        web_url: "https://acme.atlassian.net/wiki/spaces/ENG/pages/40230".into(),
    };

    let tree_site = app.tree_site(&ui);
    app.root_pages_by_space.insert(
        SiteKey::new(&tree_site, "ENG"),
        vec![root_arch.clone(), root_oncall],
    );
    app.page_children_map.insert(
        SiteKey::new(&tree_site, "40200"),
        vec![child_runbook.clone(), child_gateway],
    );
    app.expanded_pages.insert(SiteKey::new(&tree_site, "40200"));

    app.active_page = Some(PageDetail {
        summary: child_runbook,
        ancestors: vec![PageAncestor {
            id: "40200".into(),
            title: "System Architecture".into(),
        }],
        labels: vec!["runbook".into(), "gateway".into(), "sre".into()],
        body_raw: String::new(),
        body_representation: "atlas_doc_format".into(),
        body_markdown: String::new(),
        blocks: vec![
            DocBlock::Panel {
                kind: PanelKind::Success,
                title: Some("Production Verified — Sprint 42".into()),
                blocks: vec![DocBlock::Paragraph {
                    spans: vec![
                        plain("Validated against "),
                        InlineNode::IssueRef {
                            key: "ENG-101".into(),
                        },
                        plain(" and "),
                        InlineNode::IssueRef {
                            key: "ENG-104".into(),
                        },
                        plain(" across Acme Cloud & Acme DC clusters."),
                    ],
                }],
            },
            DocBlock::Heading {
                level: 1,
                spans: vec![plain("1. Traffic Shift Sequence")],
            },
            DocBlock::Table {
                headers: vec![
                    vec![bold("Stage")],
                    vec![bold("Weight")],
                    vec![bold("SLO Gate")],
                    vec![bold("Owner")],
                ],
                rows: vec![
                    vec![
                        vec![plain("Canary")],
                        vec![code("25%")],
                        vec![plain("p99 <40ms")],
                        vec![plain("Maya C.")],
                    ],
                    vec![
                        vec![plain("Half Ramp")],
                        vec![code("50%")],
                        vec![plain("WAL <150ms")],
                        vec![plain("Alex R.")],
                    ],
                    vec![
                        vec![plain("Cutover")],
                        vec![code("100%")],
                        vec![plain("Zero 5xx")],
                        vec![plain("Priya N.")],
                    ],
                ],
            },
            DocBlock::Heading {
                level: 2,
                spans: vec![plain("2. Verification Command")],
            },
            DocBlock::CodeBlock {
                language: Some("bash".into()),
                code: "jira transition ENG-101 \"In Review\"\nconf view 40211 --json | jq '.version'".into(),
            },
            DocBlock::Heading {
                level: 2,
                spans: vec![plain("3. Quick Links")],
            },
            DocBlock::Paragraph {
                spans: vec![
                    link(
                        "EU Gateway SLO Dashboard",
                        "https://metrics.acme.example/d/eu-gw-slo",
                    ),
                    plain(" · "),
                    link(
                        "Status Page (Internal)",
                        "https://status.acme.internal/regions/eu",
                    ),
                ],
            },
        ],
        toc: vec![
            TocEntry {
                level: 1,
                text: "1. Traffic Shift Sequence".into(),
                line_hint: 5,
            },
            TocEntry {
                level: 2,
                text: "2. Verification Command".into(),
                line_hint: 14,
            },
            TocEntry {
                level: 2,
                text: "3. Quick Links".into(),
                line_hint: 20,
            },
            TocEntry {
                level: 2,
                text: "4. Rollback Triggers".into(),
                line_hint: 24,
            },
        ],
        attachments: vec![],
        comments: vec![ConfluenceComment {
            id: "cc-1".into(),
            page_id: "40211".into(),
            author_name: "Liam Brooks".into(),
            body_raw: String::new(),
            body_markdown: "Updated rollback command for secondary Data Center pool.".into(),
            blocks: vec![DocBlock::Paragraph {
                spans: vec![plain(
                    "Updated rollback command for secondary Data Center pool.",
                )],
            }],
            created_at: "2025-11-19T13:55:00Z".into(),
        }],
        linked_issue_keys: vec!["ENG-101".into(), "ENG-104".into()],
        incomplete: vec![],
    });

    app.cache_counts = CacheCounts {
        projects: 6,
        boards: 4,
        issues: 142,
        spaces: 5,
        pages: 89,
    };

    (app, ui)
}

#[tokio::main]
async fn main() {
    let (mut app, mut ui) = build_demo_state();
    let mut frames = Vec::new();

    // Frame 1: Jira Kanban Board view (4 columns: To Do / In Progress / In Review / Done)
    app.persona = Persona::Jira;
    ui.active_tab = WorkspaceTab::Board;
    ui.focus = PaneFocus::Main;
    ui.show_nav_rail = false; // Full 120-col width renders all 4 Kanban columns cleanly side-by-side
    ui.board_ui.selected_col = 1;
    ui.board_ui.selected_row = 0;
    ui.board_ui.drawer_open = false;
    ui.overlays.clear();
    frames.push(capture_frame(
        &app,
        &mut ui,
        2400,
        "Jira Kanban Board — Multi-column sprint board with live status lozenges",
        "1 Board",
    ));

    // Frame 2: Issues list + rich Issue detail pane
    ui.active_tab = WorkspaceTab::Issues;
    ui.show_nav_rail = true;
    ui.focus = PaneFocus::Main;
    ui.issue_ui.selected_issue = 0;
    ui.issue_ui.jql.text = "project = ENG AND sprint in openSprints() ORDER BY priority DESC".into();
    ui.overlays.clear();
    frames.push(capture_frame(
        &app,
        &mut ui,
        2400,
        "Issues & JQL Explorer — Rich ADF detail, subtasks, linked docs & comments",
        "2 Issues",
    ));

    // Frame 3: Confluence Wiki view (Space tree + rendered page + TOC drawer)
    app.persona = Persona::Confluence;
    ui.active_tab = WorkspaceTab::Confluence;
    ui.show_nav_rail = true;
    ui.confluence_ui.selected_space = 0;
    ui.confluence_ui.selected_tree_row = 1;
    ui.confluence_ui.toc_open = true;
    ui.focus = PaneFocus::Main;
    ui.overlays.clear();
    frames.push(capture_frame(
        &app,
        &mut ui,
        2400,
        "Confluence Wiki — Expandable page tree, tables, code blocks & live TOC",
        "4 Wiki",
    ));

    // Frame 4: Multi-Site Switcher overlay (Ctrl+S, showing Acme Cloud + Acme DC federation)
    app.persona = Persona::Jira;
    ui.active_tab = WorkspaceTab::Issues;
    ui.overlays = vec![Overlay::site_switcher({
        let mut ov = SiteSwitcherOverlay::new(vec![
            SiteSwitcherEntry {
                site_id: "acme-cloud".into(),
                name: "Acme Cloud".into(),
                site_url: "https://acme.atlassian.net".into(),
                deployment_label: "Cloud".into(),
                auth_label: "Cloud OAuth".into(),
                enabled: true,
                is_active: false,
                issue_count: 98,
                page_count: 64,
            },
            SiteSwitcherEntry {
                site_id: "acme-dc".into(),
                name: "Acme DC".into(),
                site_url: "https://jira.acme.internal".into(),
                deployment_label: "Data Center".into(),
                auth_label: "Data Center".into(),
                enabled: true,
                is_active: false,
                issue_count: 44,
                page_count: 25,
            },
        ]);
        ov.selected = 0;
        ov
    })];
    frames.push(capture_frame(
        &app,
        &mut ui,
        2200,
        "Multi-Site Federation — Unified Cloud OAuth 2.0 & Data Center PAT switcher",
        "Ctrl+S",
    ));

    // Frame 5: Link Extractor overlay (u)
    ui.overlays = vec![Overlay::Links(LinksOverlay::from_app(&app, &ui))];
    frames.push(capture_frame(
        &app,
        &mut ui,
        2000,
        "Link Extractor — Open or copy any URL from issues & Confluence pages",
        "u",
    ));

    // Frame 6: File Picker overlay (Ctrl+F) with clean fictional workspace directory
    let picker = FilePickerOverlay {
        target: UploadTarget::Issue("ENG-101".into()),
        site_id: None,
        current_dir: PathBuf::from("/workspace/acme-platform/docs"),
        entries: vec![
            FilePickerEntry {
                name: "..".into(),
                path: PathBuf::from("/workspace/acme-platform"),
                is_dir: true,
                size: 0,
                is_parent: true,
            },
            FilePickerEntry {
                name: "diagrams".into(),
                path: PathBuf::from("/workspace/acme-platform/docs/diagrams"),
                is_dir: true,
                size: 0,
                is_parent: false,
            },
            FilePickerEntry {
                name: "runbooks".into(),
                path: PathBuf::from("/workspace/acme-platform/docs/runbooks"),
                is_dir: true,
                size: 0,
                is_parent: false,
            },
            FilePickerEntry {
                name: "eu-gateway-topology.png".into(),
                path: PathBuf::from("/workspace/acme-platform/docs/eu-gateway-topology.png"),
                is_dir: false,
                size: 184_320,
                is_parent: false,
            },
            FilePickerEntry {
                name: "migration-checklist.md".into(),
                path: PathBuf::from("/workspace/acme-platform/docs/migration-checklist.md"),
                is_dir: false,
                size: 14_280,
                is_parent: false,
            },
            FilePickerEntry {
                name: "canary-latency-report.pdf".into(),
                path: PathBuf::from("/workspace/acme-platform/docs/canary-latency-report.pdf"),
                is_dir: false,
                size: 512_900,
                is_parent: false,
            },
            FilePickerEntry {
                name: "openapi-v2.yaml".into(),
                path: PathBuf::from("/workspace/acme-platform/docs/openapi-v2.yaml"),
                is_dir: false,
                size: 38_410,
                is_parent: false,
            },
        ],
        filter: String::new(),
        selected: 3,
        mode: FilePickerMode::Browse,
        zoxide_dirs: vec![],
        status_msg: None,
        selected_file: None,
    };
    ui.overlays = vec![Overlay::FilePicker(picker)];
    frames.push(capture_frame(
        &app,
        &mut ui,
        2000,
        "Interactive File Picker — Filter & attach files with zoxide jump",
        "Ctrl+F",
    ));

    // Frame 7: Command Palette (Ctrl+P)
    let mut palette = CommandPalette::new();
    palette.selected = 2;
    ui.overlays = vec![Overlay::CommandPalette(palette)];
    frames.push(capture_frame(
        &app,
        &mut ui,
        2000,
        "Command Palette — Fuzzy-search every Jira & Confluence action",
        "Ctrl+P",
    ));

    // Frame 8: Settings overlay (Ctrl+,)
    let mut settings = SettingsOverlay::from_config(&app, &ui);
    settings.image_protocol_label = Some("Auto (Kitty Graphics)".into());
    settings.selected = 3;
    ui.overlays = vec![Overlay::Settings(settings)];
    frames.push(capture_frame(
        &app,
        &mut ui,
        2200,
        "Live Settings — Theme, density, graphics protocol & deployment info",
        "Ctrl+,",
    ));

    let spec = json!({
        "title": "atlassian · jira · confluence",
        "badge": "@bitefoundry/atlassian v0.1.0",
        "width": WIDTH,
        "height": HEIGHT,
        "bg": [17, 19, 24],
        "fg": [226, 230, 243],
        "frames": frames,
    });

    let temp_json = std::env::temp_dir().join("atlassian_demo_frames.json");
    std::fs::write(
        &temp_json,
        serde_json::to_string(&spec).expect("serialize frames.json"),
    )
    .expect("write temp frames.json");

    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let output_gif = repo_root.join("assets").join("demo.gif");
    if let Some(parent) = output_gif.parent() {
        std::fs::create_dir_all(parent).expect("create assets dir");
    }

    let renderer = renderer_script();
    if !renderer.is_file() {
        eprintln!(
            "renderer not found at {}; set DEMO_RENDERER_SCRIPT=<path to render_tui_gif.py>. Frames kept at {}",
            renderer.display(),
            temp_json.display()
        );
        std::process::exit(2);
    }
    let previews = preview_dir();
    std::fs::create_dir_all(&previews).expect("create demo preview dir");
    let python = std::env::var("DEMO_PYTHON")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "python".to_string());

    let status = Command::new(&python)
        .arg(&renderer)
        .arg(&temp_json)
        .arg(&output_gif)
        .arg("--preview-png")
        .arg(&previews)
        .status()
        .unwrap_or_else(|err| panic!("invoke {python} {}: {err}", renderer.display()));

    assert!(
        status.success(),
        "render_tui_gif.py failed with status {status}"
    );
    println!(
        "Rendered {} ({} frames)",
        output_gif.display(),
        spec["frames"].as_array().map(|a| a.len()).unwrap_or(0)
    );
}
