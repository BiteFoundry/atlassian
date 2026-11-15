//! Background synchronization engine for Jira projects, boards, active sprint issues, and Confluence spaces & root page trees.

use std::time::Duration;

use anyhow::Result;
use tokio::{
    sync::mpsc,
    time::{interval_at, Instant},
};

use crate::{
    atlassian::AtlassianClient,
    config::AppConfig,
    models::{BoardSummary, CoreEvent, IssueSummary, SpaceSummary, SprintSummary},
    storage::Storage,
};

#[derive(Debug, Clone)]
pub enum SyncCommand {
    RefreshAll,
    RefreshBoard { board_id: i64, sprint_id: Option<i64> },
    RefreshProjectIssues { project_key: String },
    RefreshSpace { space_key: String },
    Shutdown,
}

#[derive(Clone)]
pub struct SyncController {
    tx: mpsc::UnboundedSender<SyncCommand>,
}

impl SyncController {
    pub fn request(&self, cmd: SyncCommand) -> bool {
        self.tx.send(cmd).is_ok()
    }
}

pub async fn bootstrap_workspace(client: &AtlassianClient, storage: &Storage) -> Result<String> {
    let outcome = run_full_sync(client, storage, None).await;
    if outcome.succeeded_steps == 0 {
        if let Some(err) = outcome.first_error {
            return Err(err);
        }
    }
    Ok(outcome.summary)
}

pub fn spawn_sync_engine(
    config: AppConfig,
    client: AtlassianClient,
    storage: Storage,
    core_tx: mpsc::UnboundedSender<CoreEvent>,
) -> SyncController {
    let (tx, mut rx) = mpsc::unbounded_channel();

    tokio::spawn(async move {
        let secs = config.sync_interval_secs.max(5);
        let period = Duration::from_secs(secs);
        let mut ticker = interval_at(Instant::now() + period, period);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                maybe_cmd = rx.recv() => {
                    let Some(cmd) = maybe_cmd else {
                        break;
                    };
                    match cmd {
                        SyncCommand::Shutdown => break,
                        SyncCommand::RefreshAll => {
                            let _ = core_tx.send(CoreEvent::SyncStarted);
                            let outcome = run_full_sync(&client, &storage, Some(&core_tx)).await;
                            if outcome.succeeded_steps == 0 {
                                if let Some(err) = outcome.first_error {
                                    let _ = core_tx.send(CoreEvent::SyncError(format!("{err:#}")));
                                }
                            }
                            let _ = core_tx.send(CoreEvent::SyncFinished {
                                summary: outcome.summary,
                            });
                        }
                        SyncCommand::RefreshBoard { board_id, sprint_id } => {
                            let _ = core_tx.send(CoreEvent::SyncStarted);
                            if let Err(err) = sync_board(&client, &storage, board_id, sprint_id, Some(&core_tx)).await {
                                let _ = core_tx.send(CoreEvent::SyncError(format!("보드 {board_id} 동기화 실패: {err:#}")));
                            } else {
                                let _ = core_tx.send(CoreEvent::SyncFinished {
                                    summary: format!("보드 #{board_id} 동기화 완료"),
                                });
                            }
                        }
                        SyncCommand::RefreshProjectIssues { project_key } => {
                            let _ = core_tx.send(CoreEvent::SyncStarted);
                            match sync_project_issues(&client, &storage, &project_key, Some(&core_tx)).await {
                                Ok(count) => {
                                    let _ = core_tx.send(CoreEvent::SyncFinished {
                                        summary: format!("프로젝트 {project_key} 이슈 {count}건 동기화 완료"),
                                    });
                                }
                                Err(err) => {
                                    let _ = core_tx.send(CoreEvent::SyncError(format!(
                                        "프로젝트 {project_key} 이슈 동기화 실패: {err:#}"
                                    )));
                                }
                            }
                        }
                        SyncCommand::RefreshSpace { space_key } => {
                            let _ = core_tx.send(CoreEvent::SyncStarted);
                            match sync_space_pages(&client, &storage, &space_key, Some(&core_tx)).await {
                                Ok(count) => {
                                    let _ = core_tx.send(CoreEvent::SyncFinished {
                                        summary: format!("스페이스 {space_key} 페이지 {count}개 동기화 완료"),
                                    });
                                }
                                Err(err) => {
                                    let _ = core_tx.send(CoreEvent::SyncError(format!(
                                        "스페이스 {space_key} 동기화 실패: {err:#}"
                                    )));
                                }
                            }
                        }
                    }
                }
                _ = ticker.tick() => {
                    let _ = core_tx.send(CoreEvent::SyncStarted);
                    let outcome = run_full_sync(&client, &storage, Some(&core_tx)).await;
                    if outcome.succeeded_steps == 0 {
                        if let Some(err) = outcome.first_error {
                            let _ = core_tx.send(CoreEvent::SyncError(format!("{err:#}")));
                        }
                    }
                    let _ = core_tx.send(CoreEvent::SyncFinished {
                        summary: outcome.summary,
                    });
                }
            }
        }
    });

    SyncController { tx }
}

struct FullSyncOutcome {
    summary: String,
    succeeded_steps: usize,
    first_error: Option<anyhow::Error>,
}

async fn run_full_sync(
    client: &AtlassianClient,
    storage: &Storage,
    core_tx: Option<&mpsc::UnboundedSender<CoreEvent>>,
) -> FullSyncOutcome {
    let mut project_count = 0usize;
    let mut board_count = 0usize;
    let mut issue_count = 0usize;
    let mut space_count = 0usize;
    let mut succeeded_steps = 0usize;
    let mut first_error: Option<anyhow::Error> = None;

    let mut record_err = |err: anyhow::Error| {
        if first_error.is_none() {
            first_error = Some(err);
        }
    };

    // 1. Jira projects
    match client.list_projects().await {
        Ok(projects) => {
            succeeded_steps += 1;
            project_count = projects.len();
            if let Err(err) = storage.upsert_projects(&projects) {
                record_err(err);
            } else if let Some(tx) = core_tx {
                let stored = storage.list_projects().unwrap_or(projects);
                let _ = tx.send(CoreEvent::ProjectsUpdated(stored));
            }
        }
        Err(err) => record_err(err),
    }

    // 2. Jira Agile boards + top/default board columns, sprints, active sprint issues
    match client.list_boards(None).await {
        Ok(mut boards) => {
            succeeded_steps += 1;
            board_count = boards.len();
            if let Err(err) = storage.upsert_boards(&boards) {
                record_err(err);
            }
            if let Some(target_board) = select_default_board(&boards, client.config()) {
                let board_id = target_board.id;
                if let Ok(cols) = client.get_board_columns(board_id).await {
                    if let Some(b) = boards.iter_mut().find(|b| b.id == board_id) {
                        b.columns = cols;
                    }
                    let _ = storage.upsert_boards(&boards);
                }
                if let Ok((_, count)) = sync_board_details(client, storage, board_id, None, core_tx).await {
                    issue_count += count;
                }
            }
            if let Some(tx) = core_tx {
                let _ = tx.send(CoreEvent::BoardsUpdated(boards));
            }
        }
        Err(err) => record_err(err),
    }

    // 3. My open Jira issues
    match client
        .search_issues_jql(AtlassianClient::my_open_issues_jql(), 50, None, None)
        .await
    {
        Ok(page) => {
            succeeded_steps += 1;
            let fetched = page.issues.len();
            if fetched > issue_count {
                issue_count = fetched;
            }
            if let Err(err) = storage.upsert_issues(&page.issues) {
                record_err(err);
            } else if let Some(tx) = core_tx {
                let _ = tx.send(CoreEvent::IssuesUpdated(page.issues));
            }
        }
        Err(err) => record_err(err),
    }

    // 4. Confluence spaces + default/first space root pages
    match client.list_spaces(50).await {
        Ok(spaces) => {
            succeeded_steps += 1;
            space_count = spaces.len();
            if let Err(err) = storage.upsert_spaces(&spaces) {
                record_err(err);
            } else if let Some(tx) = core_tx {
                let stored = storage.list_spaces().unwrap_or_else(|_| spaces.clone());
                let _ = tx.send(CoreEvent::SpacesUpdated(stored));
            }
            if let Some(target_space) = select_default_space(&spaces, client.config()) {
                let _ = sync_space_pages(client, storage, &target_space.key, core_tx).await;
            }
        }
        Err(err) => record_err(err),
    }

    let summary = format_sync_summary(project_count, board_count, issue_count, space_count);
    FullSyncOutcome {
        summary,
        succeeded_steps,
        first_error,
    }
}

async fn sync_board(
    client: &AtlassianClient,
    storage: &Storage,
    board_id: i64,
    sprint_id: Option<i64>,
    core_tx: Option<&mpsc::UnboundedSender<CoreEvent>>,
) -> Result<(Option<SprintSummary>, usize)> {
    if let Ok(cols) = client.get_board_columns(board_id).await {
        let mut boards = storage.list_boards(None).unwrap_or_default();
        if let Some(b) = boards.iter_mut().find(|b| b.id == board_id) {
            b.columns = cols;
            let _ = storage.upsert_boards(&boards);
            if let Some(tx) = core_tx {
                let _ = tx.send(CoreEvent::BoardsUpdated(boards));
            }
        }
    }
    sync_board_details(client, storage, board_id, sprint_id, core_tx).await
}

async fn sync_board_details(
    client: &AtlassianClient,
    storage: &Storage,
    board_id: i64,
    explicit_sprint_id: Option<i64>,
    core_tx: Option<&mpsc::UnboundedSender<CoreEvent>>,
) -> Result<(Option<SprintSummary>, usize)> {
    let sprints = client.list_board_sprints(board_id).await.unwrap_or_default();
    if !sprints.is_empty() {
        storage.upsert_sprints(&sprints)?;
    }

    let active_sprint = if let Some(sid) = explicit_sprint_id {
        sprints.iter().find(|s| s.id == sid).cloned()
    } else {
        sprints
            .iter()
            .find(|s| s.state.eq_ignore_ascii_case("active"))
            .cloned()
    };
    let target_sprint_id = explicit_sprint_id.or_else(|| active_sprint.as_ref().map(|s| s.id));

    let issues: Vec<IssueSummary> = client
        .list_board_issues(board_id, target_sprint_id, 50)
        .await?;
    let count = issues.len();
    storage.upsert_board_issues(board_id, target_sprint_id, &issues)?;

    if let Some(tx) = core_tx {
        let _ = tx.send(CoreEvent::BoardIssuesUpdated {
            board_id,
            sprint: active_sprint.clone(),
            issues,
        });
    }
    Ok((active_sprint, count))
}

async fn sync_project_issues(
    client: &AtlassianClient,
    storage: &Storage,
    project_key: &str,
    core_tx: Option<&mpsc::UnboundedSender<CoreEvent>>,
) -> Result<usize> {
    let jql = format!("project = \"{project_key}\" ORDER BY updated DESC");
    let page = client.search_issues_jql(&jql, 50, None, None).await?;
    let count = page.issues.len();
    storage.upsert_issues(&page.issues)?;
    if let Some(tx) = core_tx {
        let _ = tx.send(CoreEvent::IssuesUpdated(page.issues));
    }
    Ok(count)
}

async fn sync_space_pages(
    client: &AtlassianClient,
    storage: &Storage,
    space_key: &str,
    core_tx: Option<&mpsc::UnboundedSender<CoreEvent>>,
) -> Result<usize> {
    let pages = client.list_space_root_pages(space_key, 50).await?;
    let count = pages.len();
    storage.upsert_pages(&pages)?;
    if let Some(tx) = core_tx {
        let _ = tx.send(CoreEvent::SpacePagesUpdated {
            space_key: space_key.to_string(),
            pages,
        });
    }
    Ok(count)
}

fn select_default_board<'a>(
    boards: &'a [BoardSummary],
    config: &AppConfig,
) -> Option<&'a BoardSummary> {
    if let Some(bid) = config.default_board_id {
        if let Some(b) = boards.iter().find(|b| b.id == bid) {
            return Some(b);
        }
    }
    if let Some(ref pk) = config.default_project {
        if let Some(b) = boards
            .iter()
            .find(|b| b.project_key.as_deref().map(|k| k.eq_ignore_ascii_case(pk)).unwrap_or(false))
        {
            return Some(b);
        }
    }
    boards.first()
}

fn select_default_space<'a>(
    spaces: &'a [SpaceSummary],
    config: &AppConfig,
) -> Option<&'a SpaceSummary> {
    if let Some(ref sk) = config.default_space_key {
        if let Some(s) = spaces.iter().find(|s| s.key.eq_ignore_ascii_case(sk)) {
            return Some(s);
        }
    }
    spaces.first()
}

pub(crate) fn format_sync_summary(
    projects: usize,
    boards: usize,
    issues: usize,
    spaces: usize,
) -> String {
    format!(
        "프로젝트 {projects}개 · 보드 {boards}개 · 이슈 {issues}건 · 스페이스 {spaces}개 동기화 완료"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        auth::{AuthManager, StoredCredentials},
        config::DeploymentKind,
    };

    #[test]
    fn test_format_sync_summary_and_default_selection() {
        assert_eq!(
            format_sync_summary(3, 2, 28, 4),
            "프로젝트 3개 · 보드 2개 · 이슈 28건 · 스페이스 4개 동기화 완료"
        );

        let cfg = AppConfig {
            default_board_id: Some(20),
            default_space_key: Some("ENG".into()),
            ..Default::default()
        };
        let boards = vec![
            BoardSummary {
                id: 10,
                name: "First".into(),
                ..Default::default()
            },
            BoardSummary {
                id: 20,
                name: "Target".into(),
                ..Default::default()
            },
        ];
        assert_eq!(select_default_board(&boards, &cfg).unwrap().id, 20);

        let spaces = vec![
            SpaceSummary {
                key: "OPS".into(),
                ..Default::default()
            },
            SpaceSummary {
                key: "ENG".into(),
                ..Default::default()
            },
        ];
        assert_eq!(select_default_space(&spaces, &cfg).unwrap().key, "ENG");
    }

    #[tokio::test]
    async fn test_sync_engine_shutdown_and_error_reporting() {
        let cfg = AppConfig {
            site_url: "http://127.0.0.1:1".into(),
            deployment: DeploymentKind::Cloud,
            sync_interval_secs: 300,
            ..Default::default()
        };
        let auth = AuthManager::with_credentials(
            "test",
            StoredCredentials::ApiToken {
                site_url: cfg.site_url.clone(),
                email: "user@example.com".into(),
                api_token: "tok".into(),
                deployment: DeploymentKind::Cloud,
            },
        );
        let client = AtlassianClient::new(cfg.clone(), auth).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (core_tx, mut core_rx) = mpsc::unbounded_channel();

        // Offline bootstrap returns Err rather than panicking
        assert!(bootstrap_workspace(&client, &storage).await.is_err());

        let ctrl = spawn_sync_engine(cfg, client, storage, core_tx);
        assert!(ctrl.request(SyncCommand::RefreshAll));
        let ev1 = tokio::time::timeout(Duration::from_secs(2), core_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(ev1, CoreEvent::SyncStarted));
        assert!(ctrl.request(SyncCommand::Shutdown));
    }
}
