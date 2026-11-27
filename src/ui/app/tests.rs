use super::*;
use crate::{
    atlassian::AtlassianClient,
    auth::{AuthManager, StoredCredentials},
    config::{
        AppConfig, DensityMode, DeploymentKind, ImageProtocolPreference as ConfigImageProtocol,
        Persona, SiteProfile,
    },
    img_preview,
    models::{
        AttachmentItem, BoardColumn, BoardSummary, CoreEvent, CrossRefTarget, IssueDetail,
        IssueSummary, IssueTransition, PageDetail, PageSummary, SiteScope, SpaceSummary,
        StatusCategory, UserSummary,
    },
    storage::Storage,
    sync,
    ui::{
        overlays::{
            comment::{CommentOverlay, CommentTarget},
            issue_form::IssueFormOverlay,
            page_form::PageFormOverlay,
            site_switcher::SiteSwitcherCommand,
            Overlay,
        },
        theme::Theme,
    },
};
use std::collections::{HashMap, HashSet};
use tokio::sync::mpsc;
use crate::models::IssueStatus;

fn sample_issue(key: &str, status_name: &str, cat: StatusCategory) -> IssueSummary {
    IssueSummary {
        key: key.to_string(),
        summary: format!("Summary for {key}"),
        status: IssueStatus {
            id: status_name.to_lowercase(),
            name: status_name.to_string(),
            category: cat,
        },
        ..Default::default()
    }
}

fn two_site_app() -> (AppState, UiState) {
    let cfg = AppConfig {
        sites: ["site-a", "site-b"]
            .iter()
            .map(|id| SiteProfile {
                id: (*id).into(),
                name: id.to_uppercase(),
                site_url: format!("https://{id}.example.com"),
                deployment: DeploymentKind::Cloud,
                account_id: "default".into(),
                enabled: true,
                ..Default::default()
            })
            .collect(),
        default_site: Some("site-a".into()),
        active_site: Some("site-a".into()),
        ..Default::default()
    };
    let auth = AuthManager::new("w8-tasks");
    let client = AtlassianClient::new(cfg.clone(), auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let (core_tx, _core_rx) = mpsc::unbounded_channel();
    let sync_ctrl =
        sync::spawn_sync_engine(cfg.clone(), client.clone(), storage.clone(), core_tx);
    let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
    let app = AppState::new_with_persona(
        cfg.clone(),
        Persona::Jira,
        auth,
        client,
        storage,
        sync_ctrl,
        ui_tx,
    );
    let ui = UiState::from_config_with_persona(&cfg, Persona::Jira);
    (app, ui)
}

fn page(id: &str, space: &str, title: &str) -> PageSummary {
    PageSummary {
        id: id.into(),
        space_key: space.into(),
        title: title.into(),
        ..Default::default()
    }
}

fn space(key: &str) -> SpaceSummary {
    SpaceSummary {
        id: format!("sp-{key}"),
        key: key.into(),
        name: key.into(),
        space_type: "global".into(),
        ..Default::default()
    }
}

/// Two rows with the same key from two sites; the opened issue is site A's.
fn shared_key_rows(app: &mut AppState, ui: &mut UiState) {
    app.site_scope = SiteScope::All;
    let row = || sample_issue("SHARED-1", "To Do", StatusCategory::Todo);
    app.issues = vec![row(), row()];
    app.issue_site_ids = vec!["site-a".into(), "site-b".into()];
    app.active_issue = Some(IssueDetail {
        summary: row(),
        ..Default::default()
    });
    app.active_issue_site_id = Some("site-a".into());
    ui.active_tab = WorkspaceTab::Issues;
    ui.issue_ui.selected_issue = 1;
}

#[tokio::test]
async fn write_targets_site_of_selected_row_not_opened_issue() {
    let (mut app, mut ui) = two_site_app();
    shared_key_rows(&mut app, &mut ui);
    assert_eq!(
        app.selected_issue_ref(&ui),
        Some(("site-b".to_string(), "SHARED-1".to_string()))
    );

    crate::ui::action::dispatch(crate::ui::keymap::Action::OpenTransition, &mut app, &mut ui);
    match ui.overlays.last() {
        Some(Overlay::Transition(ov)) => assert_eq!(ov.site_id.as_deref(), Some("site-b")),
        other => panic!("expected transition overlay, got {other:?}"),
    }
    ui.pop_overlay();

    app.request_transition_issue_for_site(
        &mut ui,
        Some("site-b"),
        "SHARED-1",
        IssueTransition {
            id: "31".into(),
            name: "Done".into(),
            to_status: IssueStatus {
                id: "3".into(),
                name: "Done".into(),
                category: StatusCategory::Done,
            },
        },
    );
    assert_eq!(app.issues[1].status.name, "Done", "site B row moved");
    assert_eq!(app.issues[0].status.name, "To Do", "site A row untouched");
    assert_eq!(
        app.active_issue.as_ref().unwrap().summary.status.name,
        "To Do",
        "opened site A issue untouched"
    );
}

#[tokio::test]
async fn optimistic_assignee_update_is_site_scoped() {
    let (mut app, mut ui) = two_site_app();
    shared_key_rows(&mut app, &mut ui);
    let bob = UserSummary {
        id: "bob-b".into(),
        display_name: "Bob".into(),
        ..Default::default()
    };
    app.request_assign_user(&mut ui, Some("site-b"), "SHARED-1", Some(bob));
    assert_eq!(app.issues[1].assignee.as_ref().unwrap().id, "bob-b");
    assert!(app.issues[0].assignee.is_none());
    assert!(app.active_issue.as_ref().unwrap().summary.assignee.is_none());

    app.handle_ui_msg(
        &mut ui,
        UiMsg::IssueAssignFailed {
            site_id: "site-b".into(),
            key: "SHARED-1".into(),
            previous_assignee: Some(None),
            task_label: "Assign SHARED-1".into(),
            error: "HTTP 403".into(),
        },
    );
    assert!(app.issues[1].assignee.is_none(), "rollback on site B");
}

#[tokio::test]
async fn tree_cache_does_not_collide_across_sites_with_same_space_key() {
    let (mut app, mut ui) = two_site_app();
    app.site_scope = SiteScope::All;
    app.spaces = vec![space("ENG"), space("ENG")];
    app.space_site_ids = vec!["site-a".into(), "site-b".into()];
    app.root_pages_by_space
        .insert(SiteKey::new("site-a", "ENG"), vec![page("1", "ENG", "A root")]);
    app.root_pages_by_space
        .insert(SiteKey::new("SITE-B", "ENG"), vec![page("1", "ENG", "B root")]);

    ui.confluence_ui.selected_space = 0;
    assert_eq!(app.current_confluence_tree(&ui)[0].page.title, "A root");
    ui.confluence_ui.selected_space = 1;
    assert_eq!(app.current_confluence_tree(&ui)[0].page.title, "B root");

    // Site B's children of page "1" never show under site A's page "1".
    app.handle_ui_msg(
        &mut ui,
        UiMsg::ChildPagesLoaded {
            site_id: "site-b".into(),
            parent_id: "1".into(),
            task_label: "Child pages: B root".into(),
            children: Ok(vec![page("2", "ENG", "B child")]),
        },
    );
    app.expanded_pages.insert(SiteKey::new("site-a", "1"));
    ui.confluence_ui.selected_space = 0;
    assert_eq!(app.current_confluence_tree(&ui).len(), 1);
    app.expanded_pages.insert(SiteKey::new("site-b", "1"));
    ui.confluence_ui.selected_space = 1;
    let tree_b = app.current_confluence_tree(&ui);
    assert_eq!(tree_b.len(), 2);
    assert_eq!(tree_b[1].page.title, "B child");
}

#[tokio::test]
async fn new_page_parent_only_when_active_page_in_selected_space_and_site() {
    let (mut app, mut ui) = two_site_app();
    app.site_scope = SiteScope::All;
    ui.active_tab = WorkspaceTab::Confluence;
    app.spaces = vec![space("ENG"), space("ENG"), space("OPS")];
    app.space_site_ids = vec!["site-a".into(), "site-b".into(), "site-a".into()];
    app.active_page = Some(PageDetail {
        summary: page("500", "ENG", "Opened"),
        ..Default::default()
    });
    app.active_page_site_id = Some("site-a".into());

    let open_form = |app: &mut AppState, ui: &mut UiState, space_idx: usize| {
        ui.confluence_ui.selected_space = space_idx;
        crate::ui::action::dispatch(crate::ui::keymap::Action::NewItem, app, ui);
        let form = match ui.overlays.last() {
            Some(Overlay::PageForm(ov)) => (ov.parent_id.clone(), ov.site_id.clone()),
            other => panic!("expected page form, got {other:?}"),
        };
        ui.pop_overlay();
        form
    };

    assert_eq!(
        open_form(&mut app, &mut ui, 0),
        (Some("500".into()), Some("site-a".into())),
        "same space and site: child of the opened page"
    );
    assert_eq!(
        open_form(&mut app, &mut ui, 1),
        (None, Some("site-b".into())),
        "same space key on another site: space root"
    );
    assert_eq!(
        open_form(&mut app, &mut ui, 2),
        (None, Some("site-a".into())),
        "other space on the same site: space root"
    );
}

#[tokio::test]
async fn single_site_my_work_seed_is_site_scoped_and_mine() {
    let (mut app, _ui) = two_site_app();
    let mut mine = sample_issue("ATL-1", "To Do", StatusCategory::Todo);
    mine.assignee = Some(UserSummary {
        id: "me-a".into(),
        ..Default::default()
    });
    let mut theirs = sample_issue("ATL-2", "To Do", StatusCategory::Todo);
    theirs.assignee = Some(UserSummary {
        id: "someone".into(),
        ..Default::default()
    });
    let mut done = sample_issue("ATL-3", "Done", StatusCategory::Done);
    done.assignee = mine.assignee.clone();
    app.storage
        .upsert_issues_for_site("site-a", &[mine, theirs, done])
        .unwrap();
    app.site_scope = SiteScope::Only("site-a".into());
    app.me = Some(UserSummary {
        id: "me-a".into(),
        ..Default::default()
    });
    app.load_cached_initial();
    let keys: Vec<&str> = app.my_issues.iter().map(|i| i.key.as_str()).collect();
    assert_eq!(keys, vec!["ATL-1"]);
    assert_eq!(app.my_issue_site_ids, vec!["site-a".to_string()]);
}

#[tokio::test]
async fn child_pages_loaded_finishes_task_even_when_scope_changed() {
    let (mut app, mut ui) = two_site_app();
    app.site_scope = SiteScope::Only("site-a".into());
    ui.tasks.begin("Child pages: Runbook");
    // The user switched to site-a while a site-b request was in flight.
    app.handle_ui_msg(
        &mut ui,
        UiMsg::ChildPagesLoaded {
            site_id: "site-b".into(),
            parent_id: "42".into(),
            task_label: "Child pages: Runbook".into(),
            children: Ok(vec![]),
        },
    );
    assert!(
        ui.tasks.first_running().is_none(),
        "out-of-scope child-pages result must still finish its task"
    );
    assert!(!app.page_children_map.contains_key(&SiteKey::new("site-b", "42")));
}

#[tokio::test]
async fn failed_requests_finish_their_tasks() {
    let (mut app, mut ui) = two_site_app();
    let cases: Vec<(&str, UiMsg)> = vec![
        (
            "Board #7",
            UiMsg::BoardLoadFailed {
                seq: 0,
                site_id: "default".into(),
                board_id: 7,
                error: "HTTP 500".into(),
            },
        ),
        (
            "JQL: Mine",
            UiMsg::IssuesLoadFailed {
                seq: 0,
                label: "Mine".into(),
                error: "HTTP 400".into(),
            },
        ),
        (
            "Update ATL-1",
            UiMsg::IssueActionFailed {
                task_label: "Update ATL-1".into(),
                error: "HTTP 500".into(),
            },
        ),
        (
            "Create page",
            UiMsg::PageActionFailed {
                task_label: "Create page".into(),
                error: "HTTP 409".into(),
            },
        ),
        (
            "Download: a.pdf",
            UiMsg::AttachmentDownloaded {
                task_label: "Download: a.pdf".into(),
                result: Err("HTTP 404".into()),
            },
        ),
        (
            "HTML: report.html",
            UiMsg::HtmlAttachmentLoaded {
                title: "Page — report.html".into(),
                source_url: String::new(),
                task_label: "HTML: report.html".into(),
                content_kind: bitefoundry_html::ContentKind::Html,
                result: Err("HTTP 403".into()),
            },
        ),
    ];
    for (label, msg) in cases {
        ui.tasks.begin(label);
        app.handle_ui_msg(&mut ui, msg);
        assert!(
            ui.tasks.running().all(|t| t.label != label),
            "{label} still running after its failure message"
        );
        assert!(
            ui.tasks.last().unwrap().display().contains("failed"),
            "{label} should be shown as failed"
        );
    }
}

#[test]
fn persona_defaults_to_expected_workspace_tab() {
    let cfg = AppConfig::default();
    let jira_ui = UiState::from_config_with_persona(&cfg, Persona::Jira);
    let conf_ui = UiState::from_config_with_persona(&cfg, Persona::Confluence);
    assert_eq!(jira_ui.active_tab, WorkspaceTab::Board);
    assert_eq!(jira_ui.active_tab.index(), 0);
    assert_eq!(conf_ui.active_tab, WorkspaceTab::Confluence);
    assert_eq!(conf_ui.active_tab.index(), 3);
}

#[test]
fn kanban_groups_issues_into_board_columns_and_default_categories() {
    let board = BoardSummary {
        id: 1,
        name: "Sprint Board".into(),
        board_type: "scrum".into(),
        project_key: Some("ATL".into()),
        project_name: None,
        columns: vec![
            BoardColumn {
                name: "Backlog".into(),
                status_ids: vec![],
                status_names: vec!["To Do".into()],
                category_hint: StatusCategory::Todo,
            },
            BoardColumn {
                name: "In Review".into(),
                status_ids: vec![],
                status_names: vec!["In Review".into()],
                category_hint: StatusCategory::InProgress,
            },
            BoardColumn {
                name: "Shipped".into(),
                status_ids: vec![],
                status_names: vec!["Done".into()],
                category_hint: StatusCategory::Done,
            },
        ],
    };
    let issues = vec![
        sample_issue("ATL-1", "To Do", StatusCategory::Todo),
        sample_issue("ATL-2", "In Review", StatusCategory::InProgress),
        sample_issue("ATL-3", "Done", StatusCategory::Done),
    ];

    let cols = build_kanban_columns_for_board(Some(&board), &issues);
    assert_eq!(cols.len(), 3);
    assert_eq!(cols[0].issues[0].key, "ATL-1");
    assert_eq!(cols[1].issues[0].key, "ATL-2");
    assert_eq!(cols[2].issues[0].key, "ATL-3");
}

#[test]
fn confluence_tree_flattens_and_expands_children() {
    let root = PageSummary {
        id: "100".into(),
        title: "Architecture".into(),
        has_children: Some(true),
        ..Default::default()
    };
    let child = PageSummary {
        id: "101".into(),
        parent_id: Some("100".into()),
        title: "TUI Design".into(),
        has_children: Some(false),
        ..Default::default()
    };
    let mut children_map = HashMap::new();
    children_map.insert(SiteKey::new("default", "100"), vec![child]);

    let mut expanded = HashSet::new();
    let collapsed =
        flatten_page_tree(std::slice::from_ref(&root), "default", &children_map, &expanded);
    assert_eq!(collapsed.len(), 1);
    assert!(!collapsed[0].expanded);
    assert!(collapsed[0].has_children);

    expanded.insert(SiteKey::new("DEFAULT", "100"));
    let open =
        flatten_page_tree(std::slice::from_ref(&root), "default", &children_map, &expanded);
    assert_eq!(open.len(), 2);
    assert_eq!(open[0].depth, 0);
    assert!(open[0].expanded);
    assert_eq!(open[1].depth, 1);
    assert_eq!(open[1].page.title, "TUI Design");

    // The same page id on another site is a different node.
    let other = flatten_page_tree(&[root], "site-b", &children_map, &expanded);
    assert_eq!(other.len(), 1);
    assert!(!other[0].expanded);
}

#[test]
fn confluence_tree_marks_unknown_children_expandable_and_confirmed_leaves_not() {
    let unknown = page("1", "ENG", "Unknown");
    let leaf = PageSummary {
        has_children: Some(false),
        ..page("2", "ENG", "Leaf")
    };
    let loaded_empty = page("3", "ENG", "Loaded empty");
    let gained_child = PageSummary {
        has_children: Some(false),
        ..page("4", "ENG", "Gained a child")
    };
    let mut children_map = HashMap::new();
    children_map.insert(SiteKey::new("default", "3"), Vec::new());
    children_map.insert(SiteKey::new("default", "4"), vec![page("5", "ENG", "Kid")]);
    let rows = flatten_page_tree(
        &[unknown, leaf, loaded_empty, gained_child],
        "default",
        &children_map,
        &HashSet::new(),
    );
    let flags: Vec<(bool, bool)> = rows
        .iter()
        .map(|r| (r.expandable, r.has_children))
        .collect();
    assert_eq!(
        flags,
        vec![(true, false), (false, false), (false, false), (true, true)]
    );
}

#[tokio::test]
async fn root_pages_loaded_auto_expands_space_homepage_or_sole_root() {
    let (mut app, mut ui) = two_site_app();
    app.site_scope = SiteScope::Only("site-a".into());
    ui.active_tab = WorkspaceTab::Confluence;
    let eng = SpaceSummary {
        homepage_id: Some("home".into()),
        ..space("ENG")
    };
    app.spaces = vec![eng, space("OPS"), space("DOCS")];
    app.space_site_ids = vec!["site-a".to_string(); 3];
    ui.confluence_ui.selected_space = 0;
    // Cached first-level children of the homepage show without waiting for the network.
    let cached_child = PageSummary {
        parent_id: Some("home".into()),
        ..page("home-kid", "ENG", "Getting started")
    };
    app.storage
        .upsert_pages_for_site("site-a", &[cached_child])
        .unwrap();

    let roots = vec![page("other", "ENG", "Archive"), page("home", "ENG", "ENG Home")];
    let eng_roots_loaded = |pages: Vec<PageSummary>| UiMsg::RootPagesLoaded {
        site_id: "site-a".into(),
        space_key: "ENG".into(),
        pages: Ok(pages),
    };
    app.handle_ui_msg(&mut ui, eng_roots_loaded(roots.clone()));
    let home = SiteKey::new("site-a", "home");
    assert!(app.expanded_pages.contains(&home), "homepage root auto-expands");
    assert!(!app
        .expanded_pages
        .contains(&SiteKey::new("site-a", "other")));
    let titles: Vec<String> = app
        .current_confluence_tree(&ui)
        .iter()
        .map(|r| r.page.title.clone())
        .collect();
    assert_eq!(titles, vec!["Archive", "ENG Home", "Getting started"]);

    // A page the user collapsed in this session stays collapsed when roots arrive again.
    app.toggle_expand_page(&mut ui, &roots[1]);
    assert!(!app.expanded_pages.contains(&home));
    app.handle_ui_msg(&mut ui, eng_roots_loaded(roots.clone()));
    assert!(!app.expanded_pages.contains(&home));
    app.handle_core_event(
        &mut ui,
        CoreEvent::SpacePagesUpdated {
            site_id: "site-a".into(),
            space_key: "ENG".into(),
            pages: roots.clone(),
        },
    );
    assert!(!app.expanded_pages.contains(&home));

    // No homepage id: the sole root page auto-expands (background sync path too).
    ui.confluence_ui.selected_space = 1;
    app.handle_core_event(
        &mut ui,
        CoreEvent::SpacePagesUpdated {
            site_id: "site-a".into(),
            space_key: "OPS".into(),
            pages: vec![page("ops-root", "OPS", "OPS Home")],
        },
    );
    assert!(app
        .expanded_pages
        .contains(&SiteKey::new("site-a", "ops-root")));

    // Left alone: a confirmed-leaf sole root, several roots without a homepage id, and the
    // roots of a space that is not the active one.
    ui.confluence_ui.selected_space = 2;
    let docs_loaded = |pages: Vec<PageSummary>| UiMsg::RootPagesLoaded {
        site_id: "site-a".into(),
        space_key: "DOCS".into(),
        pages: Ok(pages),
    };
    app.handle_ui_msg(
        &mut ui,
        docs_loaded(vec![PageSummary {
            has_children: Some(false),
            ..page("docs-root", "DOCS", "DOCS Home")
        }]),
    );
    app.handle_ui_msg(
        &mut ui,
        docs_loaded(vec![page("d1", "DOCS", "One"), page("d2", "DOCS", "Two")]),
    );
    app.handle_ui_msg(
        &mut ui,
        UiMsg::RootPagesLoaded {
            site_id: "site-a".into(),
            space_key: "OPS".into(),
            pages: Ok(vec![page("ops-only", "OPS", "Other OPS root")]),
        },
    );
    for id in ["docs-root", "d1", "d2", "ops-only"] {
        assert!(
            !app.expanded_pages.contains(&SiteKey::new("site-a", id)),
            "{id} must not auto-expand"
        );
    }
}

#[tokio::test]
async fn cross_link_history_stack_pushes_and_pops() {
    let cfg = AppConfig::default();
    let auth = AuthManager::new("test");
    let client = AtlassianClient::new(cfg.clone(), auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();
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

    app.active_issue = Some(IssueDetail {
        summary: sample_issue("ATL-10", "In Progress", StatusCategory::InProgress),
        ..Default::default()
    });

    app.jump_to_cross_ref(
        &mut ui,
        CrossRefTarget::ConfluencePage {
            page_id: "555".into(),
            title: Some("Design Doc".into()),
        },
    );
    assert_eq!(ui.active_tab, WorkspaceTab::Confluence);
    assert_eq!(app.nav_history.len(), 1);
    assert!(matches!(
        app.nav_history[0].target,
        CrossRefTarget::JiraIssue { ref key, .. } if key == "ATL-10"
    ));

    assert!(app.pop_nav_history(&mut ui));
    assert_eq!(ui.active_tab, WorkspaceTab::Issues);
    assert!(app.nav_history.is_empty());
    assert!(!app.pop_nav_history(&mut ui));
}

#[test]
fn ui_state_initializes_all_preferences_from_app_config() {
    let cfg = AppConfig {
        theme: crate::config::ThemePreset::Light,
        density: DensityMode::Compact,
        inline_images: false,
        show_nav_rail: false,
        notify: false,
        image_protocol: ConfigImageProtocol::Sixel,
        ..Default::default()
    };
    let ui = UiState::from_config_with_persona(&cfg, Persona::Jira);
    assert_eq!(ui.density, DensityMode::Compact);
    assert!(!ui.inline_images);
    assert!(!ui.show_nav_rail);
    assert!(!ui.notify);
    assert_eq!(
        ui.graphics_caps.protocol,
        img_preview::TerminalImageProtocol::Sixel
    );
    assert_eq!(ui.theme.bg_sidebar, Theme::daylight_light().bg_sidebar);
}

#[tokio::test]
async fn optimistic_transition_assign_and_comment_rollback_on_failure() {
    let cfg = AppConfig::default();
    let auth = AuthManager::new("test");
    let client = AtlassianClient::new(cfg.clone(), auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let (core_tx, _) = mpsc::unbounded_channel();
    let sync_ctrl =
        sync::spawn_sync_engine(cfg.clone(), client.clone(), storage.clone(), core_tx);
    let (ui_tx, _ui_rx) = mpsc::unbounded_channel();

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

    let initial_status = IssueStatus {
        id: "1".into(),
        name: "To Do".into(),
        category: StatusCategory::Todo,
    };
    let issue = IssueSummary {
        key: "ATL-99".into(),
        summary: "Optimistic rollback test".into(),
        status: initial_status.clone(),
        ..Default::default()
    };
    app.board_issues.push(issue.clone());
    app.issues.push(issue.clone());
    app.active_issue = Some(IssueDetail {
        summary: issue,
        ..Default::default()
    });
    app.me = Some(UserSummary {
        id: "u1".into(),
        display_name: "Alice".into(),
        ..Default::default()
    });

    // 1. Optimistic transition -> rollback on failure
    app.request_transition_issue(
        &mut ui,
        "ATL-99",
        IssueTransition {
            id: "31".into(),
            name: "Done".into(),
            to_status: IssueStatus {
                id: "3".into(),
                name: "Done".into(),
                category: StatusCategory::Done,
            },
        },
    );
    assert_eq!(
        app.active_issue.as_ref().unwrap().summary.status.name,
        "Done"
    );
    app.handle_ui_msg(
        &mut ui,
        UiMsg::IssueTransitionFailed {
            site_id: "default".into(),
            key: "ATL-99".into(),
            previous_status: Some(initial_status),
            error: "HTTP 500".into(),
        },
    );
    assert_eq!(
        app.active_issue.as_ref().unwrap().summary.status.name,
        "To Do"
    );

    // 2. Optimistic assign to me -> rollback on failure
    app.request_assign_to_me(&mut ui, "ATL-99");
    assert_eq!(
        app.active_issue
            .as_ref()
            .unwrap()
            .summary
            .assignee
            .as_ref()
            .map(|u| u.display_name.as_str()),
        Some("Alice")
    );
    app.handle_ui_msg(
        &mut ui,
        UiMsg::IssueAssignFailed {
            site_id: "default".into(),
            key: "ATL-99".into(),
            previous_assignee: Some(None),
            task_label: "Assign ATL-99 to me".into(),
            error: "HTTP 403".into(),
        },
    );
    assert!(app.active_issue.as_ref().unwrap().summary.assignee.is_none());

    // 3. Optimistic comment -> rollback on failure
    app.request_add_comment(
        &mut ui,
        CommentTarget::Issue("ATL-99".into()),
        "Testing optimistic comment".into(),
    );
    assert_eq!(app.active_issue.as_ref().unwrap().comments.len(), 1);
    assert_eq!(app.active_issue.as_ref().unwrap().summary.comment_count, 1);
    let opt_id = app.active_issue.as_ref().unwrap().comments[0].id.clone();
    app.handle_ui_msg(
        &mut ui,
        UiMsg::CommentAdded {
            site_id: "default".into(),
            issue_key: Some("ATL-99".into()),
            page_id: None,
            optimistic_id: Some(opt_id),
            task_label: "Comment on ATL-99".into(),
            clear_draft_key: None,
            result: Err("network error".into()),
        },
    );
    assert!(app.active_issue.as_ref().unwrap().comments.is_empty());
    assert_eq!(app.active_issue.as_ref().unwrap().summary.comment_count, 0);
}

#[tokio::test]
async fn tui_async_detail_sequence_and_multisite_sync_preserve_open_issue_and_404_clears() {
    use crate::models::{IssueDetail, IssueSummary};
    use crate::sync::MultiSiteSyncOutcome;

    let cfg = AppConfig::default();
    let auth = AuthManager::new("test-seq");
    let client = AtlassianClient::new(cfg.clone(), auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let (core_tx, _core_rx) = mpsc::unbounded_channel();
    let sync_ctrl = sync::spawn_sync_engine(cfg.clone(), client.clone(), storage.clone(), core_tx);
    let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
    let mut app = AppState::new_with_persona(
        cfg.clone(),
        Persona::Jira,
        auth,
        client,
        storage.clone(),
        sync_ctrl,
        ui_tx,
    );
    let mut ui = UiState::from_config_with_persona(&cfg, Persona::Jira);

    let mk = |key: &str, summary: &str, updated: &str| IssueDetail {
        summary: IssueSummary {
            id: key.into(),
            key: key.into(),
            project_key: "ATL".into(),
            summary: summary.into(),
            updated_at: updated.into(),
            ..Default::default()
        },
        description_markdown: format!("Body of {key}"),
        ..Default::default()
    };

    let d1 = mk("ATL-1", "First issue", "2026-11-18T10:00:00Z");
    let d2 = mk("ATL-2", "Second issue", "2026-11-17T10:00:00Z");
    storage.upsert_issue_detail_for_site("default", &d1).unwrap();
    storage.upsert_issue_detail_for_site("default", &d2).unwrap();

    app.active_issue = Some(d2.clone());
    app.active_issue_site_id = Some("default".into());

    // MultiSiteSyncFinished must preserve currently open ATL-2 instead of hijacking to ATL-1
    app.handle_ui_msg(
        &mut ui,
        UiMsg::MultiSiteSyncFinished(MultiSiteSyncOutcome {
            summary: "Synced".into(),
            total_sites: 1,
            succeeded_sites: 1,
            per_site: vec![],
        }),
    );
    assert_eq!(app.active_issue.as_ref().unwrap().summary.key, "ATL-2");

    // Stale out-of-order response (seq=1 when current seq=2) must be ignored
    app.issue_detail_req_seq = 2;
    app.handle_ui_msg(
        &mut ui,
        UiMsg::IssueDetailLoaded {
            seq: Some(1),
            site_id: "default".into(),
            key: "ATL-1".into(),
            detail: Ok(d1),
            not_found: false,
        },
    );
    assert_eq!(app.active_issue.as_ref().unwrap().summary.key, "ATL-2");

    // Matching seq with not_found=true clears active_issue and removes zombie from lists
    app.handle_ui_msg(
        &mut ui,
        UiMsg::IssueDetailLoaded {
            seq: Some(2),
            site_id: "default".into(),
            key: "ATL-2".into(),
            detail: Err("HTTP 404".into()),
            not_found: true,
        },
    );
    assert!(app.active_issue.is_none());
    assert!(!app.issues.iter().any(|i| i.key == "ATL-2"));
}

#[tokio::test]
async fn cached_detail_reconciled_summary_still_fetches_changed_revision_and_deduplicates() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use axum::{routing::get, Json, Router};
    use serde_json::json;
    use tokio::net::TcpListener;

    let issue_detail_hits = Arc::new(AtomicUsize::new(0));
    let page_detail_hits = Arc::new(AtomicUsize::new(0));

    let ih = issue_detail_hits.clone();
    let ph = page_detail_hits.clone();
    let mock = Router::new()
        .route(
            "/rest/api/3/issue/ATL-1",
            get(move || {
                let ih = ih.clone();
                async move {
                    ih.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(60)).await;
                    Json(json!({
                        "id": "100",
                        "key": "ATL-1",
                        "fields": {
                            "project": { "key": "ATL" },
                            "summary": "Remote Issue V2",
                            "issuetype": { "name": "Task" },
                            "status": { "id": "3", "name": "Done", "statusCategory": { "key": "done" } },
                            "description": {
                                "type": "doc",
                                "version": 1,
                                "content": [{ "type": "paragraph", "content": [{ "type": "text", "text": "REMOTE_ISSUE_BODY_V2" }] }]
                            },
                            "updated": "2026-11-20T12:05:00.000+0000"
                        }
                    }))
                }
            }),
        )
        .route("/rest/api/3/issue/ATL-1/remotelink", get(|| async { Json(json!([])) }))
        .route(
            "/wiki/api/v2/pages/1001",
            get(move || {
                let ph = ph.clone();
                async move {
                    ph.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(60)).await;
                    Json(json!({
                        "id": "1001",
                        "spaceId": "5001",
                        "spaceKey": "ENG",
                        "title": "Remote Page V2",
                        "status": "current",
                        "version": { "number": 2, "createdAt": "2026-11-20T12:05:00.000Z" },
                        "body": {
                            "atlas_doc_format": {
                                "representation": "atlas_doc_format",
                                "value": "{\"type\":\"doc\",\"version\":1,\"content\":[{\"type\":\"paragraph\",\"content\":[{\"type\":\"text\",\"text\":\"REMOTE_PAGE_BODY_V2\"}]}]}"
                            }
                        },
                        "_links": { "webui": "/spaces/ENG/pages/1001" }
                    }))
                }
            }),
        )
        .route("/wiki/api/v2/pages/1001/labels", get(|| async { Json(json!({"results": []})) }))
        .route("/wiki/api/v2/pages/1001/attachments", get(|| async { Json(json!({"results": []})) }))
        .route("/wiki/api/v2/pages/1001/footer-comments", get(|| async { Json(json!({"results": []})) }));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let srv = tokio::spawn(async move {
        let _ = axum::serve(listener, mock).await;
    });

    let cfg = AppConfig {
        site_url: base_url.clone(),
        sync_interval_secs: 300,
        ..AppConfig::default()
    };
    let tmp_dir = tempfile::tempdir().unwrap();
    let auth = AuthManager::new("dedup-test")
        .with_fallback_path(tmp_dir.path().join("creds.json"));
    auth.save_credentials(&StoredCredentials::ApiToken {
        site_url: base_url.clone(),
        email: "bob@example.com".into(),
        api_token: "tok".into(),
        deployment: DeploymentKind::Cloud,
    })
    .unwrap();
    let client = AtlassianClient::new(cfg.clone(), auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let (core_tx, _core_rx) = mpsc::unbounded_channel();
    let sync_ctrl = sync::spawn_sync_engine(cfg.clone(), client.clone(), storage.clone(), core_tx);
    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
    let mut app = AppState::new_with_persona(
        cfg.clone(),
        Persona::Jira,
        auth,
        client,
        storage.clone(),
        sync_ctrl,
        ui_tx,
    );
    let mut ui = UiState::from_config_with_persona(&cfg, Persona::Jira);

    // Seed cached v1 details, then upsert newer v2 summary rows (so get_*_detail_for_site
    // reconciles summary to v2 while detail_json still has OLD v1 body!).
    let old_issue_detail = IssueDetail {
        summary: IssueSummary {
            id: "100".into(),
            key: "ATL-1".into(),
            project_key: "ATL".into(),
            summary: "Old V1".into(),
            updated_at: "2026-11-20T10:00:00.000+0000".into(),
            ..Default::default()
        },
        description_markdown: "OLD_V1_DESC".into(),
        ..Default::default()
    };
    storage.upsert_issue_detail_for_site("default", &old_issue_detail).unwrap();
    let mut new_issue_summary = old_issue_detail.summary.clone();
    new_issue_summary.summary = "Remote Issue V2".into();
    new_issue_summary.updated_at = "2026-11-20T12:05:00.000+0000".into();
    storage.upsert_issues_for_site("default", std::slice::from_ref(&new_issue_summary)).unwrap();

    let old_page_detail = PageDetail {
        summary: PageSummary {
            id: "1001".into(),
            space_key: "ENG".into(),
            title: "Old Page V1".into(),
            version_number: 1,
            updated_at: "2026-11-20T10:00:00.000Z".into(),
            ..Default::default()
        },
        body_markdown: "OLD_V1_BODY".into(),
        ..Default::default()
    };
    storage.upsert_page_detail_for_site("default", &old_page_detail).unwrap();
    let mut new_page_summary = old_page_detail.summary.clone();
    new_page_summary.title = "Remote Page V2".into();
    new_page_summary.version_number = 2;
    new_page_summary.updated_at = "2026-11-20T12:05:00.000Z".into();
    storage.upsert_pages_for_site("default", std::slice::from_ref(&new_page_summary)).unwrap();

    // Load from cache: reconciled summary now reads v2, yet body is still v1!
    app.active_page = Some(old_page_detail);
    app.active_page_site_id = Some("default".into());
    app.load_cached_initial();
    assert_eq!(app.active_issue.as_ref().unwrap().summary.updated_at, "2026-11-20T12:05:00.000+0000");
    assert_eq!(app.active_issue.as_ref().unwrap().description_markdown, "OLD_V1_DESC");
    assert_eq!(app.active_page.as_ref().unwrap().summary.version_number, 2);
    assert_eq!(app.active_page.as_ref().unwrap().body_markdown, "OLD_V1_BODY");

    // Concurrent OpenIssueRevision + IssuesUpdated + SyncFinished for the same v2 revision
    // must STILL trigger remote fetch (not blocked by reconciled summary!) AND deduplicate to 1 call!
    app.handle_core_event(
        &mut ui,
        CoreEvent::OpenIssueRevision {
            site_id: "default".into(),
            summary: Box::new(new_issue_summary.clone()),
        },
    );
    app.handle_core_event(
        &mut ui,
        CoreEvent::IssuesUpdated {
            site_id: "default".into(),
            issues: vec![new_issue_summary],
        },
    );
    app.handle_core_event(
        &mut ui,
        CoreEvent::OpenPageRevision {
            site_id: "default".into(),
            summary: new_page_summary.clone(),
        },
    );
    app.handle_core_event(
        &mut ui,
        CoreEvent::SpacePagesUpdated {
            site_id: "default".into(),
            space_key: "ENG".into(),
            pages: vec![new_page_summary],
        },
    );
    app.handle_core_event(
        &mut ui,
        CoreEvent::SyncFinished {
            summary: "Synced".into(),
        },
    );

    let mut got_issue = false;
    let mut got_page = false;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && (!got_issue || !got_page) {
        if let Ok(Some(msg)) = tokio::time::timeout(Duration::from_millis(250), ui_rx.recv()).await {
            if matches!(msg, UiMsg::IssueDetailLoaded { .. }) {
                got_issue = true;
            }
            if matches!(msg, UiMsg::PageDetailLoaded { .. }) {
                got_page = true;
            }
            app.handle_ui_msg(&mut ui, msg);
        }
    }
    assert!(got_issue && got_page, "both details must be fetched remotely");
    assert_eq!(issue_detail_hits.load(Ordering::SeqCst), 1, "issue detail must be deduplicated to 1 HTTP call");
    assert_eq!(page_detail_hits.load(Ordering::SeqCst), 1, "page detail must be deduplicated to 1 HTTP call");
    assert!(app.active_issue.as_ref().unwrap().description_markdown.contains("REMOTE_ISSUE_BODY_V2"));
    assert!(app.active_page.as_ref().unwrap().body_markdown.contains("REMOTE_PAGE_BODY_V2"));

    srv.abort();
}

#[tokio::test]
async fn multisite_same_key_id_late_response_isolated_and_open_edit_forms_preserved() {
    let cfg = AppConfig {
        sites: vec![
            SiteProfile {
                id: "site-a".into(),
                name: "Site A".into(),
                site_url: "https://site-a.example.com".into(),
                confluence_url: String::new(),
                deployment: DeploymentKind::Cloud,
                cloud_id: None,
                account_id: "default".into(),
                enabled: true,
                ..Default::default()
            },
            SiteProfile {
                id: "site-b".into(),
                name: "Site B".into(),
                site_url: "https://site-b.example.com".into(),
                confluence_url: String::new(),
                deployment: DeploymentKind::Cloud,
                cloud_id: None,
                account_id: "default".into(),
                enabled: true,
                ..Default::default()
            },
        ],
        default_site: Some("site-a".into()),
        active_site: Some("site-a".into()),
        ..AppConfig::default()
    };

    let auth = AuthManager::new("ms-iso");
    let client = AtlassianClient::new(cfg.clone(), auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let (core_tx, _core_rx) = mpsc::unbounded_channel();
    let sync_ctrl = sync::spawn_sync_engine(cfg.clone(), client.clone(), storage.clone(), core_tx);
    let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
    let mut app = AppState::new_with_persona(
        cfg.clone(),
        Persona::Jira,
        auth,
        client,
        storage.clone(),
        sync_ctrl,
        ui_tx,
    );
    let mut ui = UiState::from_config_with_persona(&cfg, Persona::Jira);

    let issue_a = IssueDetail {
        summary: IssueSummary {
            id: "1".into(),
            key: "SHARED-1".into(),
            project_key: "SHARED".into(),
            summary: "Site A Issue".into(),
            updated_at: "2026-11-20T10:00:00.000+0000".into(),
            ..Default::default()
        },
        description_markdown: "SITE_A_ISSUE_BODY".into(),
        ..Default::default()
    };
    let issue_b = IssueDetail {
        summary: IssueSummary {
            id: "1".into(),
            key: "SHARED-1".into(),
            project_key: "SHARED".into(),
            summary: "Site B Issue".into(),
            updated_at: "2026-11-20T10:00:00.000+0000".into(),
            ..Default::default()
        },
        description_markdown: "SITE_B_ISSUE_BODY".into(),
        ..Default::default()
    };
    let page_a = PageDetail {
        summary: PageSummary {
            id: "1001".into(),
            space_key: "ENG".into(),
            title: "Site A Page".into(),
            version_number: 1,
            updated_at: "2026-11-20T10:00:00.000Z".into(),
            ..Default::default()
        },
        body_markdown: "SITE_A_PAGE_BODY".into(),
        ..Default::default()
    };
    let page_b = PageDetail {
        summary: PageSummary {
            id: "1001".into(),
            space_key: "ENG".into(),
            title: "Site B Page".into(),
            version_number: 1,
            updated_at: "2026-11-20T10:00:00.000Z".into(),
            ..Default::default()
        },
        body_markdown: "SITE_B_PAGE_BODY".into(),
        ..Default::default()
    };
    storage.upsert_issue_detail_for_site("site-a", &issue_a).unwrap();
    storage.upsert_issue_detail_for_site("site-b", &issue_b).unwrap();
    storage.upsert_page_detail_for_site("site-a", &page_a).unwrap();
    storage.upsert_page_detail_for_site("site-b", &page_b).unwrap();

    app.load_cached_initial();
    let seq_a_before_switch = app.issue_detail_req_seq;
    assert_eq!(app.active_issue.as_ref().unwrap().summary.summary, "Site A Issue");

    // Switch from site-a to site-b
    app.apply_site_switcher_command(SiteSwitcherCommand::SelectSite("site-b".into()), &mut ui);
    assert_eq!(app.active_issue.as_ref().unwrap().summary.summary, "Site B Issue");
    assert_eq!(app.active_issue_site_id.as_deref(), Some("site-b"));

    // Late OpenIssueRevision and IssueDetailLoaded from site-a must NOT overwrite site-b's active_issue,
    // while site-a's SQLite cache receives the updated summary!
    let mut late_a_rev = issue_a.summary.clone();
    late_a_rev.summary = "Site A Late V2".into();
    late_a_rev.updated_at = "2026-11-20T12:00:00.000+0000".into();
    app.handle_core_event(
        &mut ui,
        CoreEvent::OpenIssueRevision {
            site_id: "site-a".into(),
            summary: Box::new(late_a_rev),
        },
    );
    app.handle_ui_msg(
        &mut ui,
        UiMsg::IssueDetailLoaded {
            seq: Some(seq_a_before_switch),
            site_id: "site-a".into(),
            key: "SHARED-1".into(),
            detail: Ok(issue_a),
            not_found: false,
        },
    );
    assert_eq!(app.active_issue.as_ref().unwrap().summary.summary, "Site B Issue");
    assert_eq!(
        storage.get_issue_detail_for_site("site-a", "SHARED-1").unwrap().unwrap().summary.summary,
        "Site A Late V2"
    );
    assert_eq!(
        storage.get_issue_detail_for_site("site-b", "SHARED-1").unwrap().unwrap().summary.summary,
        "Site B Issue"
    );

    // Open edit forms preserve unsaved user text when background detail updates arrive
    let mut issue_form = IssueFormOverlay::edit_issue(&issue_b);
    issue_form.description.set_text("UNSAVED_USER_ISSUE_DRAFT");
    ui.push_overlay(Overlay::IssueForm(issue_form));

    let mut updated_b = issue_b.clone();
    updated_b.description_markdown = "REMOTE_UPDATED_SITE_B".into();
    updated_b.summary.updated_at = "2026-11-20T12:10:00.000+0000".into();
    app.handle_ui_msg(
        &mut ui,
        UiMsg::IssueDetailLoaded {
            seq: Some(app.issue_detail_req_seq),
            site_id: "site-b".into(),
            key: "SHARED-1".into(),
            detail: Ok(updated_b),
            not_found: false,
        },
    );
    assert_eq!(app.active_issue.as_ref().unwrap().description_markdown, "REMOTE_UPDATED_SITE_B");
    match ui.overlays.last() {
        Some(Overlay::IssueForm(ov)) => {
            assert_eq!(ov.description.text, "UNSAVED_USER_ISSUE_DRAFT");
        }
        other => panic!("expected IssueForm overlay intact, got {other:?}"),
    }

    // Same for PageFormOverlay + CommentOverlay
    ui.pop_overlay();
    app.active_page = Some(page_b.clone());
    app.active_page_site_id = Some("site-b".into());
    app.active_page_target = Some(("site-b".into(), "1001".into()));
    let mut page_form = PageFormOverlay::edit_page(&page_b);
    page_form.body.set_text("UNSAVED_USER_PAGE_DRAFT");
    ui.push_overlay(Overlay::PageForm(page_form));

    let mut updated_page_b = page_b;
    updated_page_b.summary.version_number = 2;
    updated_page_b.body_markdown = "REMOTE_PAGE_B_V2".into();
    app.handle_ui_msg(
        &mut ui,
        UiMsg::PageDetailLoaded {
            seq: Some(app.page_detail_req_seq),
            site_id: "site-b".into(),
            page_id: "1001".into(),
            detail: Ok(updated_page_b),
            not_found: false,
        },
    );
    assert_eq!(app.active_page.as_ref().unwrap().body_markdown, "REMOTE_PAGE_B_V2");
    match ui.overlays.last() {
        Some(Overlay::PageForm(ov)) => {
            assert_eq!(ov.body.text, "UNSAVED_USER_PAGE_DRAFT");
            assert_eq!(ov.prev_version, 1, "base version remains pinned for 409 conflict safety");
        }
        other => panic!("expected PageForm overlay intact, got {other:?}"),
    }
}

#[tokio::test]
async fn panel_attachment_and_comment_refresh_updates_independently_of_page_version() {
    use axum::{routing::get, Json, Router};
    use serde_json::json;
    use tokio::net::TcpListener;

    let mock = Router::new()
        .route(
            "/wiki/api/v2/pages/1001/attachments",
            get(|| async {
                Json(json!({
                    "results": [{
                        "id": "att-live-1",
                        "title": "spec-v2.pdf",
                        "mediaType": "application/pdf",
                        "fileSize": 4096,
                        "downloadLink": "/download/spec-v2.pdf"
                    }]
                }))
            }),
        )
        .route(
            "/wiki/api/v2/pages/1001/footer-comments",
            get(|| async {
                Json(json!({
                    "results": [{
                        "id": "cmt-live-1",
                        "version": { "createdAt": "2026-11-20T12:20:00.000Z", "authorId": "Carol" },
                        "body": {
                            "atlas_doc_format": {
                                "value": "{\"type\":\"doc\",\"version\":1,\"content\":[{\"type\":\"paragraph\",\"content\":[{\"type\":\"text\",\"text\":\"LIVE_COMMENT_WITHOUT_BODY_BUMP\"}]}]}"
                            }
                        }
                    }]
                }))
            }),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let srv = tokio::spawn(async move {
        let _ = axum::serve(listener, mock).await;
    });

    let cfg = AppConfig {
        site_url: base_url.clone(),
        sync_interval_secs: 300,
        ..AppConfig::default()
    };
    let tmp_dir = tempfile::tempdir().unwrap();
    let auth = AuthManager::new("panel-refresh")
        .with_fallback_path(tmp_dir.path().join("creds.json"));
    auth.save_credentials(&StoredCredentials::ApiToken {
        site_url: base_url.clone(),
        email: "bob@example.com".into(),
        api_token: "tok".into(),
        deployment: DeploymentKind::Cloud,
    })
    .unwrap();
    let client = AtlassianClient::new(cfg.clone(), auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let (core_tx, _core_rx) = mpsc::unbounded_channel();
    let sync_ctrl = sync::spawn_sync_engine(cfg.clone(), client.clone(), storage.clone(), core_tx);
    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
    let mut app = AppState::new_with_persona(
        cfg.clone(),
        Persona::Confluence,
        auth,
        client,
        storage.clone(),
        sync_ctrl,
        ui_tx,
    );
    let mut ui = UiState::from_config_with_persona(&cfg, Persona::Confluence);

    let page = PageDetail {
        summary: PageSummary {
            id: "1001".into(),
            space_key: "ENG".into(),
            title: "Spec".into(),
            version_number: 1,
            updated_at: "2026-11-20T10:00:00.000Z".into(),
            ..Default::default()
        },
        body_markdown: "BODY_V1".into(),
        ..Default::default()
    };
    storage.upsert_page_detail_for_site("default", &page).unwrap();
    app.active_page = Some(page);
    app.active_page_site_id = Some("default".into());
    app.active_page_target = Some(("default".into(), "1001".into()));
    app.active_page_detail_version = Some(1);

    // 1) Attachments panel open + focus return refreshes attachments even though page version stays 1
    ui.push_overlay(Overlay::Attachments(
        app.build_attachments_overlay(&ui),
    ));
    app.request_attachments_refresh(&ui);
    let msg = tokio::time::timeout(Duration::from_secs(2), ui_rx.recv())
        .await
        .unwrap()
        .unwrap();
    app.handle_ui_msg(&mut ui, msg);
    assert_eq!(app.active_page.as_ref().unwrap().attachments.len(), 1);
    assert_eq!(app.active_page.as_ref().unwrap().attachments[0].filename, "spec-v2.pdf");
    match ui.overlays.last() {
        Some(Overlay::Attachments(ov)) => assert_eq!(ov.items.len(), 1),
        other => panic!("expected AttachmentsOverlay updated, got {other:?}"),
    }

    // 2) Comment panel open with unsaved draft text refreshes comments while preserving draft
    ui.pop_overlay();
    ui.push_overlay(Overlay::Comment(CommentOverlay::with_draft(
        CommentTarget::Page {
            page_id: "1001".into(),
            title: "Spec".into(),
        },
        "UNSAVED_COMMENT_DRAFT",
    )));
    app.request_comments_refresh(&ui);
    let msg2 = tokio::time::timeout(Duration::from_secs(2), ui_rx.recv())
        .await
        .unwrap()
        .unwrap();
    app.handle_ui_msg(&mut ui, msg2);
    assert_eq!(app.active_page.as_ref().unwrap().comments.len(), 1);
    assert!(app.active_page.as_ref().unwrap().comments[0]
        .body_markdown
        .contains("LIVE_COMMENT_WITHOUT_BODY_BUMP"));
    match ui.overlays.last() {
        Some(Overlay::Comment(ov)) => assert_eq!(ov.draft_text(), "UNSAVED_COMMENT_DRAFT"),
        other => panic!("expected CommentOverlay intact, got {other:?}"),
    }

    srv.abort();
}
fn board(id: i64, project: &str) -> BoardSummary {
    BoardSummary {
        id,
        name: format!("Board {id}"),
        board_type: "scrum".into(),
        project_key: Some(project.into()),
        ..Default::default()
    }
}

fn named_board(id: i64, name: &str) -> BoardSummary {
    BoardSummary {
        id,
        name: name.into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn my_work_404_removal_keeps_site_vectors_aligned() {
    let (mut app, _ui) = two_site_app();
    let row = |k: &str| sample_issue(k, "To Do", StatusCategory::Todo);
    app.my_issues = vec![row("SHARED-1"), row("SHARED-1"), row("B-2")];
    app.my_issue_site_ids = vec!["site-a".into(), "site-b".into(), "site-b".into()];
    app.my_issue_site_labels = vec![Some("A".into()), Some("B".into()), Some("B".into())];
    app.remove_my_issue("site-b", "SHARED-1");
    let keys: Vec<&str> = app.my_issues.iter().map(|i| i.key.as_str()).collect();
    assert_eq!(keys, vec!["SHARED-1", "B-2"]);
    assert_eq!(app.my_issue_site_ids, vec!["site-a".to_string(), "site-b".to_string()]);
    assert_eq!(app.my_issue_site_labels, vec![Some("A".to_string()), Some("B".to_string())]);

    app.my_pages = vec![page("1", "ENG", "A"), page("1", "ENG", "B"), page("2", "ENG", "B2")];
    app.my_page_site_ids = vec!["site-a".into(), "site-b".into(), "site-b".into()];
    app.my_page_site_labels = vec![Some("A".into()), Some("B".into()), Some("B".into())];
    app.remove_my_page("site-b", "1");
    let titles: Vec<&str> = app.my_pages.iter().map(|p| p.title.as_str()).collect();
    assert_eq!(titles, vec!["A", "B2"]);
    assert_eq!(app.my_page_site_ids, vec!["site-a".to_string(), "site-b".to_string()]);
    assert_eq!(app.my_page_site_labels, vec![Some("A".to_string()), Some("B".to_string())]);
}

#[tokio::test]
async fn boards_updated_for_other_site_is_ignored_in_single_site_scope() {
    let (mut app, mut ui) = two_site_app();
    app.site_scope = SiteScope::Only("site-a".into());
    app.boards = vec![named_board(1, "A board")];
    app.board_site_ids = vec!["site-a".into()];

    app.handle_core_event(
        &mut ui,
        CoreEvent::BoardsUpdated {
            site_id: "site-b".into(),
            items: vec![named_board(2, "B board")],
        },
    );
    assert_eq!(app.boards[0].name, "A board", "site B list must not replace site A's");
    app.handle_core_event(
        &mut ui,
        CoreEvent::ProjectsUpdated {
            site_id: "site-b".into(),
            items: vec![ProjectSummary {
                key: "BPROJ".into(),
                ..Default::default()
            }],
        },
    );
    assert!(app.projects.is_empty());
    app.handle_core_event(
        &mut ui,
        CoreEvent::SpacesUpdated {
            site_id: "site-b".into(),
            items: vec![space("BSPACE")],
        },
    );
    assert!(app.spaces.is_empty());

    app.handle_core_event(
        &mut ui,
        CoreEvent::BoardsUpdated {
            site_id: "SITE-A".into(),
            items: vec![named_board(3, "A board 2"), named_board(4, "A board 3")],
        },
    );
    assert_eq!(app.boards.len(), 2);
    assert_eq!(app.board_site_ids, vec!["SITE-A".to_string(); 2]);

    // A legacy daemon sends no site: it is the effective site's list.
    app.handle_core_event(
        &mut ui,
        CoreEvent::SpacesUpdated {
            site_id: String::new(),
            items: vec![space("ENG")],
        },
    );
    assert_eq!(app.spaces.len(), 1);
    assert_eq!(app.space_site_ids, vec!["site-a".to_string()]);
}

#[tokio::test]
async fn pin_toggle_writes_to_row_site() {
    let (mut app, mut ui) = two_site_app();
    let proj = ProjectSummary {
        id: "1".into(),
        key: "ENG".into(),
        name: "Engineering".into(),
        ..Default::default()
    };
    for sid in ["site-a", "site-b"] {
        app.storage
            .upsert_projects_for_site(sid, std::slice::from_ref(&proj))
            .unwrap();
        app.storage.upsert_spaces_for_site(sid, &[space("DOC")]).unwrap();
    }
    app.site_scope = SiteScope::All;
    app.reload_projects_all_sites();
    app.spaces = vec![space("DOC"), space("DOC")];
    app.space_site_ids = vec!["site-a".into(), "site-b".into()];
    let b_row = app
        .project_site_ids
        .iter()
        .position(|s| s == "site-b")
        .expect("site B project row");

    ui.active_tab = WorkspaceTab::Directory;
    ui.directory_ui.section = 0;
    ui.directory_ui.selected_project = b_row;
    crate::ui::action::dispatch(crate::ui::keymap::Action::TogglePin, &mut app, &mut ui);
    let pinned = |sid: &str| app.storage.list_projects_for_site(sid).unwrap()[0].pinned;
    assert!(pinned("site-b"), "site B row pinned");
    assert!(!pinned("site-a"), "site A project with the same key untouched");

    ui.directory_ui.section = 1;
    ui.directory_ui.selected_space = 1;
    crate::ui::action::dispatch(crate::ui::keymap::Action::TogglePin, &mut app, &mut ui);
    let space_pinned = |sid: &str| app.storage.list_spaces_for_site(sid).unwrap()[0].pinned;
    assert!(space_pinned("site-b"));
    assert!(!space_pinned("site-a"));
}

#[tokio::test]
async fn board_load_uses_site_of_selected_row_when_ids_collide() {
    let (mut app, _ui) = two_site_app();
    app.site_scope = SiteScope::All;
    app.boards = vec![named_board(10, "A board"), named_board(10, "B board")];
    app.board_site_ids = vec!["site-a".into(), "site-b".into()];
    assert_eq!(app.board_row_target(1), Some((10, Some("site-b"))));
    assert_eq!(app.board_row_target(0), Some((10, Some("site-a"))));
    assert_eq!(app.board_row_target(2), None);
    let (sid, _client) = app.site_id_and_client_for(app.board_row_target(1).unwrap().1);
    assert_eq!(sid, "site-b");
}
#[tokio::test]
async fn drafts_are_site_scoped_only_with_multiple_sites() {
    let (mut app, _ui) = two_site_app();
    assert_eq!(
        app.scoped_draft_key(Some("Site-B"), "comment:issue:ENG-1".into()),
        "site:site-b:comment:issue:ENG-1"
    );
    assert_ne!(
        app.scoped_draft_key(Some("site-a"), "comment:issue:ENG-1".into()),
        app.scoped_draft_key(Some("site-b"), "comment:issue:ENG-1".into())
    );
    assert_eq!(app.scoped_draft_key(None, "issue:new".into()), "issue:new");
    // Single-site users keep the keys their saved drafts already use.
    app.config.sites.truncate(1);
    assert_eq!(
        app.scoped_draft_key(Some("site-a"), "comment:issue:ENG-1".into()),
        "comment:issue:ENG-1"
    );
}

#[tokio::test]
async fn back_history_and_cross_links_keep_the_source_site() {
    let (mut app, mut ui) = two_site_app();
    shared_key_rows(&mut app, &mut ui);
    app.active_issue_site_id = Some("site-b".into());
    ui.issue_ui.selected_issue = 0;

    crate::ui::action::dispatch(crate::ui::keymap::Action::OpenCrossLinks, &mut app, &mut ui);
    match ui.overlays.last() {
        Some(Overlay::CrossLink(ov)) => assert_eq!(ov.site_id.as_deref(), Some("site-b")),
        other => panic!("expected cross-link overlay, got {other:?}"),
    }
    ui.pop_overlay();

    app.jump_to_cross_ref_for_site(
        &mut ui,
        Some("site-b"),
        CrossRefTarget::ConfluencePage {
            page_id: "555".into(),
            title: None,
        },
    );
    assert_eq!(app.nav_history.len(), 1);
    assert_eq!(app.nav_history[0].site_id.as_deref(), Some("site-b"));

    // Back reopens SHARED-1 on site B: the site-B row is selected, not site A's.
    assert!(app.pop_nav_history(&mut ui));
    assert_eq!(ui.active_tab, WorkspaceTab::Issues);
    assert_eq!(ui.issue_ui.selected_issue, 1);
}

#[tokio::test]
async fn image_preview_is_keyed_and_fetched_by_owner_site() {
    let (mut app, mut ui) = two_site_app();
    shared_key_rows(&mut app, &mut ui);
    app.active_issue_site_id = Some("site-b".into());
    app.active_issue.as_mut().unwrap().attachments = vec![AttachmentItem {
        id: "10".into(),
        filename: "shot.png".into(),
        mime_type: "image/png".into(),
        ..Default::default()
    }];
    crate::ui::action::dispatch(crate::ui::keymap::Action::OpenImagePreview, &mut app, &mut ui);
    let Some(Overlay::ImagePreview(ov)) = ui.overlays.last() else {
        panic!("expected image preview overlay, got {:?}", ui.overlays.last());
    };
    assert_eq!(ov.site_id.as_deref(), Some("site-b"));
    let att = ov.current().unwrap().clone();
    assert_eq!(ov.cache_id(&att), "site-b/10");
    let mut unsited = ov.clone();
    unsited.site_id = None;
    assert_eq!(unsited.cache_id(&att), "10");
}

#[tokio::test]
async fn default_space_index_prefers_configured_key() {
    let (mut app, _ui) = two_site_app();
    app.site_scope = SiteScope::All;
    app.config.active_site = None;
    app.spaces = vec![space("AAA"), space("ENG"), space("ENG")];
    app.space_site_ids = vec!["site-b".into(), "site-b".into(), "site-a".into()];
    assert_eq!(app.resolve_default_space_index(), 0, "nothing configured");

    app.config.default_space_key = Some(" eng ".into());
    assert_eq!(
        app.resolve_default_space_index(),
        2,
        "case-insensitive key on the configured site (site-a)"
    );
    app.config.default_site = Some("site-b".into());
    assert_eq!(app.resolve_default_space_index(), 1, "site-b's ENG");
    app.config.default_site = Some("site-a".into());
    app.space_site_ids = vec!["site-a".into(), "site-b".into(), "site-c".into()];
    assert_eq!(
        app.resolve_default_space_index(),
        1,
        "first ENG without a site match"
    );

    app.config.default_space_key = Some("NOPE".into());
    assert_eq!(app.resolve_default_space_index(), 0, "unknown key");
}

#[tokio::test]
async fn default_board_index_prefers_board_id_then_project() {
    let (mut app, _ui) = two_site_app();
    app.boards = vec![
        board(1, "OPS"),
        board(2, "ENG"),
        board(42, "WEB"),
        board(5, "ENG"),
    ];
    app.board_site_ids = vec!["site-a".into(); 4];
    assert_eq!(app.resolve_default_board_index(), 0, "nothing configured");

    app.config.default_project = Some("eng".into());
    assert_eq!(
        app.resolve_default_board_index(),
        1,
        "first board of the project"
    );
    app.config.default_board_id = Some(42);
    assert_eq!(
        app.resolve_default_board_index(),
        2,
        "board id beats project"
    );
    app.config.default_board_id = Some(999);
    assert_eq!(
        app.resolve_default_board_index(),
        1,
        "missing id falls back to project"
    );
    app.config.default_project = Some("NONE".into());
    assert_eq!(app.resolve_default_board_index(), 0);
}

#[tokio::test]
async fn site_switch_selects_default_space_not_zero() {
    let (mut app, mut ui) = two_site_app();
    app.config.default_space_key = Some("OPS".into());
    app.storage
        .upsert_spaces_for_site("site-b", &[space("API"), space("ENG"), space("OPS")])
        .unwrap();
    app.storage
        .upsert_boards_for_site("site-b", &[board(7, "API"), board(8, "OPS")])
        .unwrap();
    app.config.default_project = Some("OPS".into());
    // The user had moved on site A; a site switch starts from the defaults.
    ui.confluence_ui.user_moved_space = true;
    ui.board_ui.user_moved_board = true;

    app.apply_site_switcher_command(SiteSwitcherCommand::SelectSite("site-b".into()), &mut ui);
    assert_eq!(ui.confluence_ui.selected_space, 2);
    assert_eq!(app.active_space(&ui).unwrap().key, "OPS");
    assert_eq!(app.active_board(&ui).unwrap().id, 8);

    // A background reload keeps a selection the user made.
    ui.confluence_ui.selected_space = 1;
    ui.confluence_ui.user_moved_space = true;
    app.load_cached_initial();
    app.apply_default_selection(&mut ui);
    assert_eq!(ui.confluence_ui.selected_space, 1);

    // All Sites: the default site's OPS, after site-a's rows.
    app.storage
        .upsert_spaces_for_site("site-a", &[space("OPS"), space("ZED")])
        .unwrap();
    app.config.default_site = Some("site-b".into());
    app.apply_site_switcher_command(SiteSwitcherCommand::SelectAllSites, &mut ui);
    let idx = ui.confluence_ui.selected_space;
    assert_eq!(idx, 4, "site-a: OPS, ZED; site-b: API, ENG, OPS");
    assert_eq!(app.active_space(&ui).unwrap().key, "OPS");
    assert_eq!(app.active_space_site_id(&ui), Some("site-b"));
}

#[tokio::test]
async fn preload_reads_root_pages_of_default_space() {
    let (mut app, mut ui) = two_site_app();
    app.config.default_space_key = Some("OPS".into());
    app.storage
        .upsert_spaces_for_site("site-a", &[space("ENG"), space("OPS")])
        .unwrap();
    app.storage
        .upsert_spaces_for_site("site-b", &[space("OPS")])
        .unwrap();
    app.storage
        .upsert_authoritative_pages_for_site("site-a", &[page("10", "OPS", "A ops home")])
        .unwrap();
    app.storage
        .upsert_authoritative_pages_for_site("site-a", &[page("11", "ENG", "A eng home")])
        .unwrap();
    app.storage
        .upsert_authoritative_pages_for_site("site-b", &[page("20", "OPS", "B ops home")])
        .unwrap();

    // Single site (site-a): the default space's roots, not the first space's.
    app.load_cached_initial();
    assert!(app
        .root_pages_by_space
        .contains_key(&SiteKey::new("site-a", "OPS")));
    assert!(!app
        .root_pages_by_space
        .contains_key(&SiteKey::new("site-a", "ENG")));
    app.apply_default_selection(&mut ui);
    assert_eq!(app.current_confluence_tree(&ui)[0].page.title, "A ops home");

    // All Sites with site-b as the default site: site-b's OPS.
    app.root_pages_by_space.clear();
    app.site_scope = SiteScope::All;
    app.config.active_site = None;
    app.config.default_site = Some("site-b".into());
    app.load_cached_initial();
    assert!(app
        .root_pages_by_space
        .contains_key(&SiteKey::new("site-b", "OPS")));
    assert!(!app
        .root_pages_by_space
        .contains_key(&SiteKey::new("site-a", "ENG")));
    app.apply_default_selection(&mut ui);
    assert_eq!(app.active_space_site_id(&ui), Some("site-b"));
    assert_eq!(app.current_confluence_tree(&ui)[0].page.title, "B ops home");
}

#[tokio::test]
async fn spawn_bootstrap_overlaps_identity_sync_and_filters() {
    use std::sync::Arc;
    use axum::{routing::{get, post}, Json, Router};
    use serde_json::json;
    use tokio::{net::TcpListener, sync::Barrier};

    let barrier = Arc::new(Barrier::new(3));
    let b_me = barrier.clone();
    let b_sync = barrier.clone();
    let b_filt = barrier.clone();

    let mock = Router::new()
        .route(
            "/rest/api/3/myself",
            get(move || {
                let b = b_me.clone();
                async move {
                    b.wait().await;
                    Json(json!({ "accountId": "acc-1", "displayName": "Alice" }))
                }
            }),
        )
        .route(
            "/rest/api/3/project/search",
            get(move || {
                let b = b_sync.clone();
                async move {
                    b.wait().await;
                    Json(json!({ "values": [{ "id": "1", "key": "ENG", "name": "Eng" }] }))
                }
            }),
        )
        .route(
            "/rest/agile/1.0/board",
            get(|| async { Json(json!({ "isLast": true, "values": [] })) }),
        )
        .route(
            "/rest/api/3/search/jql",
            post(|| async { Json(json!({ "issues": [] })) }),
        )
        .route(
            "/wiki/api/v2/spaces",
            get(|| async { Json(json!({ "results": [] })) }),
        )
        .route(
            "/rest/api/3/filter/favourite",
            get(move || {
                let b = b_filt.clone();
                async move {
                    b.wait().await;
                    Json(json!([{ "id": "10", "name": "My Bugs", "jql": "issuetype = Bug" }]))
                }
            }),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let srv = tokio::spawn(async move {
        let _ = axum::serve(listener, mock).await;
    });

    let cfg = AppConfig {
        site_url: base_url.clone(),
        confluence_url: format!("{base_url}/wiki"),
        sync_interval_secs: 300,
        ..AppConfig::default()
    };
    let auth = AuthManager::with_credentials(
        "boot-par",
        StoredCredentials::ApiToken {
            site_url: base_url.clone(),
            email: "alice@example.com".into(),
            api_token: "tok".into(),
            deployment: DeploymentKind::Cloud,
        },
    );
    let client = AtlassianClient::new(cfg.clone(), auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let (core_tx, _core_rx) = mpsc::unbounded_channel();
    let sync_ctrl =
        sync::spawn_sync_engine(cfg.clone(), client.clone(), storage.clone(), core_tx);
    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
    let app = AppState::new_with_persona(
        cfg.clone(),
        Persona::Jira,
        auth,
        client,
        storage,
        sync_ctrl,
        ui_tx,
    );
    let mut ui = UiState::from_config_with_persona(&cfg, Persona::Jira);

    app.spawn_bootstrap(&mut ui);

    let mut got_me = false;
    let mut got_boot = false;
    let mut got_filters = false;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && !(got_me && got_boot && got_filters) {
        if let Ok(Some(msg)) =
            tokio::time::timeout(Duration::from_millis(500), ui_rx.recv()).await
        {
            match msg {
                UiMsg::MeLoaded(Ok(_)) => got_me = true,
                UiMsg::BootstrapFinished(Ok(_)) => got_boot = true,
                UiMsg::SavedFiltersLoaded(f) if !f.is_empty() => got_filters = true,
                _ => {}
            }
        }
    }
    assert!(
        got_me && got_boot && got_filters,
        "spawn_bootstrap must overlap identity, sync, and filters (me={got_me}, boot={got_boot}, filters={got_filters})"
    );
    srv.abort();
}

#[tokio::test]
async fn request_board_load_and_my_work_overlap_independent_requests() {
    use std::sync::Arc;
    use axum::{routing::{get, post}, Json, Router};
    use serde_json::json;
    use tokio::{net::TcpListener, sync::Barrier};

    let board_barrier = Arc::new(Barrier::new(3));
    let my_work_barrier = Arc::new(Barrier::new(2));
    let b1 = board_barrier.clone();
    let b2 = board_barrier.clone();
    let b3 = board_barrier.clone();
    let m1 = my_work_barrier.clone();
    let m2 = my_work_barrier.clone();

    let mock = Router::new()
        .route(
            "/rest/agile/1.0/board/11/configuration",
            get(move || {
                let b = b1.clone();
                async move {
                    b.wait().await;
                    Json(json!({ "columnConfig": { "columns": [{ "name": "To Do", "statuses": [{ "id": "1" }] }] } }))
                }
            }),
        )
        .route(
            "/rest/agile/1.0/board/11/sprint",
            get(move || {
                let b = b2.clone();
                async move {
                    b.wait().await;
                    Json(json!({ "isLast": true, "values": [{ "id": 101, "name": "Sprint 1", "state": "active" }] }))
                }
            }),
        )
        .route(
            "/rest/agile/1.0/sprint/101/issue",
            get(move || {
                let b = b3.clone();
                async move {
                    b.wait().await;
                    Json(json!({ "issues": [], "total": 0 }))
                }
            }),
        )
        .route(
            "/rest/api/3/search/jql",
            post(move || {
                let b = m1.clone();
                async move {
                    b.wait().await;
                    Json(json!({ "issues": [] }))
                }
            }),
        )
        .route(
            "/wiki/rest/api/search",
            get(move || {
                let b = m2.clone();
                async move {
                    b.wait().await;
                    Json(json!({ "results": [] }))
                }
            }),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let srv = tokio::spawn(async move {
        let _ = axum::serve(listener, mock).await;
    });

    let cfg = AppConfig {
        site_url: base_url.clone(),
        confluence_url: format!("{base_url}/wiki"),
        sync_interval_secs: 300,
        ..AppConfig::default()
    };
    let auth = AuthManager::with_credentials(
        "board-mw-par",
        StoredCredentials::ApiToken {
            site_url: base_url.clone(),
            email: "alice@example.com".into(),
            api_token: "tok".into(),
            deployment: DeploymentKind::Cloud,
        },
    );
    let client = AtlassianClient::new(cfg.clone(), auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let (core_tx, _core_rx) = mpsc::unbounded_channel();
    let sync_ctrl =
        sync::spawn_sync_engine(cfg.clone(), client.clone(), storage.clone(), core_tx);
    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
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
    app.boards = vec![board(11, "ENG")];
    app.board_site_ids = vec!["default".into()];

    app.request_board_load(&mut ui, 0, Some(101));
    app.request_my_work_refresh(&mut ui);

    let mut got_board = false;
    let mut got_my_work = false;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && !(got_board && got_my_work) {
        if let Ok(Some(msg)) =
            tokio::time::timeout(Duration::from_millis(500), ui_rx.recv()).await
        {
            match msg {
                UiMsg::BoardDataLoaded { board_id: 11, .. } => got_board = true,
                UiMsg::MyWorkLoaded { .. } => got_my_work = true,
                _ => {}
            }
        }
    }
    assert!(got_board && got_my_work, "board={got_board}, my_work={got_my_work}");
    srv.abort();
}

#[tokio::test]
async fn interactive_sibling_prefetch_respects_budget_and_skips_cached_items() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };
    use axum::{extract::Path, routing::get, Json, Router};
    use serde_json::json;
    use tokio::net::TcpListener;

    let fetched_keys = Arc::new(Mutex::new(Vec::<String>::new()));
    let hits = Arc::new(AtomicUsize::new(0));
    let fk = fetched_keys.clone();
    let h = hits.clone();
    let mock = Router::new()
        .route(
            "/rest/api/3/issue/{key}",
            get(move |Path(key): Path<String>| {
                let fk = fk.clone();
                let h = h.clone();
                async move {
                    h.fetch_add(1, Ordering::SeqCst);
                    fk.lock().unwrap().push(key.clone());
                    Json(json!({
                        "id": key,
                        "key": key,
                        "fields": {
                            "project": { "key": "ENG" },
                            "summary": format!("Summary {key}"),
                            "status": { "id": "1", "name": "To Do", "statusCategory": { "key": "new" } },
                            "issuetype": { "id": "10", "name": "Task", "subtask": false },
                            "updated": "2026-11-20T10:00:00.000+0000"
                        }
                    }))
                }
            }),
        )
        .route(
            "/rest/api/3/issue/{key}/remotelink",
            get(|| async { Json(json!([])) }),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let srv = tokio::spawn(async move {
        let _ = axum::serve(listener, mock).await;
    });

    let cfg = AppConfig {
        site_url: base_url.clone(),
        sync_interval_secs: 300,
        ..AppConfig::default()
    };
    let auth = AuthManager::with_credentials(
        "prefetch-test",
        StoredCredentials::ApiToken {
            site_url: base_url.clone(),
            email: "alice@example.com".into(),
            api_token: "tok".into(),
            deployment: DeploymentKind::Cloud,
        },
    );
    let client = AtlassianClient::new(cfg.clone(), auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();
    // Pre-cache ENG-2 so prefetch skips it.
    storage
        .upsert_issue_detail_for_site(
            "default",
            &IssueDetail {
                summary: sample_issue("ENG-2", "To Do", StatusCategory::Todo),
                description_markdown: "Already cached".into(),
                ..Default::default()
            },
        )
        .unwrap();
    let (core_tx, _core_rx) = mpsc::unbounded_channel();
    let sync_ctrl =
        sync::spawn_sync_engine(cfg.clone(), client.clone(), storage.clone(), core_tx);
    let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
    let app = AppState::new_with_persona(
        cfg.clone(),
        Persona::Jira,
        auth,
        client,
        storage.clone(),
        sync_ctrl,
        ui_tx,
    );

    let keys: Vec<String> = ["ENG-1", "ENG-2", "ENG-3", "ENG-4", "ENG-5", "ENG-6"]
        .into_iter()
        .map(str::to_string)
        .collect();
    app.prefetch_uncached_issues("default", &keys, Some("ENG-1"));

    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && hits.load(Ordering::SeqCst) < INTERACTIVE_SIBLING_PREFETCH_LIMIT {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(80)).await;

    assert_eq!(hits.load(Ordering::SeqCst), INTERACTIVE_SIBLING_PREFETCH_LIMIT);
    let mut seen = fetched_keys.lock().unwrap().clone();
    seen.sort();
    assert_eq!(seen, vec!["ENG-3", "ENG-4", "ENG-5"]);
    assert!(storage.get_issue_detail_for_site("default", "ENG-3").unwrap().is_some());
    assert!(storage.get_issue_detail_for_site("default", "ENG-6").unwrap().is_none());
    srv.abort();
}
