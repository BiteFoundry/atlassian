# jira · confluence · conf (`@shinick/atlassian` v0.1.0)

**Atlassian Jira(프로젝트 · 애자일 칸반 보드 · 스프린트 · 이슈)** 와 **Atlassian Confluence(스페이스 · 계층형 페이지 트리 · 리치 문서 · 코멘트)** 를 **터미널에서** 하나로 통합해 탐색·생성·편집·상태 전이·첨부파일 업로드/다운로드·통합 검색할 수 있는 **네이티브 Rust CLI · TUI · HTTP JSON API · MCP 서버 통합 도구**입니다.

> **단 한 번의 인증 (`jira login` / `confluence login` / `conf login`)** 으로 **Cloud (API Token)** 와 **Data Center / Server (PAT)** 환경 모두에서 Jira와 Confluence 전체 기능을 즉시 사용할 수 있습니다.
> 세 명령어(`jira`, `confluence`, `conf`)는 동일한 단일 바이너리(`jira` / `jira.exe`), 동일한 OS 키링 자격 증명, 동일한 **SQLite WAL + FTS5 로컬 캐시(`cache.db`)**, 동일한 HTTP API(`jira serve`, 기본 포트 `8789`) 및 MCP 서버(`jira mcp`)를 공유합니다.

---

## 목차

- [주요 기능](#주요-기능)
- [빠른 시작 (Quickstart)](#빠른-시작-quickstart)
  - [다른 PC에서 프리빌트 설치 (`install-profile.ps1` / `gh release download` / `npm`)](#다른-pc에서-프리빌트-설치-install-profileps1--gh-release-download--npm)
  - [단일 인증 (Cloud API Token & Data Center PAT)](#단일-인증-cloud-api-token--data-center-pat)
  - [원클릭 자가 업데이터 (`jira update` / `confluence update`)](#원클릭-자가-업데이터-jira-update--confluence-update)
- [트리플 명령어 & 페르소나 (`jira`, `confluence`, `conf`)](#트리플-명령어--페르소나-jira-confluence-conf)
- [TUI 가이드 & 단축키 치트시트](#tui-가이드--단축키-치트시트)
- [CLI 빠른 레퍼런스](#cli-빠른-레퍼런스)
- [HTTP JSON API (`jira serve`)](#http-json-api-jira-serve)
- [MCP 서버 (`jira mcp` — 26개 도구)](#mcp-서버-jira-mcp--26개-도구)
- [빌드, 런처 및 npm 배포](#빌드-런처-및-npm-배포)
- [설정 및 데이터 저장 위치](#설정-및-데이터-저장-위치)

---

## 주요 기능

- **단일 바이너리 트리플 커맨드 (`jira`, `confluence`, `conf`)**:
  - `jira`: Jira 프로젝트 · 애자일 보드 · 이슈 중심으로 TUI/CLI 실행
  - `confluence` / `conf`: `ATLASSIAN_PERSONA=confluence`로 실행되어 Confluence 스페이스 · 페이지 트리 화면으로 바로 진입
- **Atlassian Cloud & Data Center / Server 동시 지원**:
  - **Cloud (`*.atlassian.net`)**: Email + API Token (`Basic` 인증), Jira REST v3 (`/rest/api/3/search/jql` `nextPageToken` 페이지네이션 + ADF JSON), Agile v1, Confluence v2 (`/wiki/api/v2`)
  - **Data Center / Server (사내 구축형)**: PAT (`Authorization: Bearer <PAT>`) 또는 Basic 인증, Jira v2 (`/rest/api/2/search` `startAt` + Wiki Markup), Confluence v1 (`/rest/api/content` Storage XHTML), Jira/Confluence 분리 호스트(`--confluence-url`) 지원
- **네이티브 멀티 컬럼 칸반 보드 & 이슈 인스펙터 TUI**:
  - 프로젝트 · 스프린트 · 멀티 컬럼 칸반 보드 탐색, 컬럼 간 좌우 이동 및 이슈 상태 전이(`m`), 나에게 할당(`i`), 담당자 선택(`a`), 코멘트(`c`), 이슈 생성(`n`)/편집(`e`), 인라인 JQL 검색(`J`)
- **Confluence 확장형 페이지 트리 & 리치 리더/에디터**:
  - 계층형 페이지 트리 접기/펼치기(`←`/`→` 또는 `Space`), ADF JSON · Storage XHTML · Wiki Markup을 통합 `DocBlock` AST로 변환해 코드 블록 · 패널 · 태스크 리스트 · 표 · 목차(TOC)와 함께 렌더링
  - 외부 `$EDITOR` 연동 마크다운 왕복 편집(`Ctrl+E`) 및 낙관적 버전 잠금(`version.number` 자동 증가)
- **`Jira <-> Confluence` 크로스 앱 점프 (`g` / `Backspace` / `Alt+←`)**:
  - Jira 이슈에 연결된 Confluence 페이지 또는 본문 내 언급된 이슈 키(`PROJ-101`)·페이지 링크로 `g` 키 한 번에 즉시 이동하고, `Backspace` / `Alt+←`로 이전 내비게이션 스택 복귀
- **인라인 터미널 이미지 미리보기 (`v` / `--preview-images`)**:
  - Jira 이슈 및 Confluence 페이지 첨부 이미지(`png`, `jpg`, `gif`, `webp`)를 외부 뷰어 없이 터미널에 직접 렌더링 (Kitty / iTerm2 / WezTerm / Ghostty 그래픽 프로토콜 및 24-bit Truecolor ANSI Half-Block `▀` 지원)
- **SQLite WAL + FTS5 로컬 오프라인 캐시 & 통합 검색**:
  - Jira 이슈(`issues_fts`)와 Confluence 페이지(`pages_fts`)를 단일 `cache.db`에 저장해 2글자 한글(예: `배포`) 포함 부분 문자열을 오프라인에서도 빠르게 통합 검색
- **원클릭 GitHub Release 자가 업데이터 (`jira update`)**:
  - `shinick-dev/atlassian` 최신 릴리즈 조회, `SHA256SUMS.txt` 무결성 검증, Windows NTFS 실행 중 `.old` 안전 교체 지원
- **AI 에이전트 자동화 (`jira serve` & `jira mcp`)**:
  - `axum` 기반 로컬 HTTP JSON API(`127.0.0.1:8789`, Bearer 토큰 보호) 및 26개 도구를 제공하는 stdio JSON-RPC 2.0 MCP 서버

---

## 빠른 시작 (Quickstart)

### 다른 PC에서 프리빌트 설치 (`install-profile.ps1` / `gh release download` / `npm`)

빌드된 단일 실행 파일(`jira.exe` / `jira`, SQLite·rustls 내장)이 [GitHub Releases](https://github.com/shinick-dev/atlassian/releases)와 npm(`@shinick/atlassian`)으로 제공되므로 Rust 컴파일러 없이도 다른 PC에 바로 설치할 수 있습니다.

#### 방법 A: Windows PowerShell 프로필 + PATH 심 일괄 등록 (권장)

```powershell
# 저장소 클론 후 PowerShell $PROFILE + $HOME\Tools\bin 심(jira, confluence, conf) 동시 등록
# target\release\jira.exe가 없으면 gh release download로 자동 내려받습니다.
gh repo clone shinick-dev/atlassian
cd atlassian
pwsh -File .\scripts\install-profile.ps1
. $PROFILE

jira --version
confluence --version
conf --version
```

> `scripts/install-profile.ps1`는 멱등적으로 동작하며, `$HOME\Tools\bin` 디렉터리가 있으면 `jira.cmd`, `confluence.cmd`, `conf.cmd` 심도 함께 생성합니다.

#### 방법 B: npm 글로벌 설치 (Windows / macOS / Linux 공통)

```bash
# `jira`, `confluence`, `conf` 3개 명령어 동시 설치
npm install -g @shinick/atlassian

jira --version
confluence --version
conf --version

# 또는 일회성 실행
npx @shinick/atlassian --version
npx --yes -p @shinick/atlassian confluence --version
npx --yes -p @shinick/atlassian conf --version
```

#### 방법 C: `gh release download`로 단일 실행 파일만 직접 내려받기

```powershell
# Windows: jira.exe 직접 다운로드 후 confluence.cmd / conf.cmd 또는 ATLASSIAN_PERSONA=confluence 사용
gh release download -R shinick-dev/atlassian --pattern "jira.exe" --dir "$HOME\Tools\bin" --clobber
```

```bash
# macOS / Linux (예: Linux x64)
gh release download -R shinick-dev/atlassian --pattern "atlassian-*-x86_64-unknown-linux-gnu.tar.gz" --dir /tmp --clobber
tar -xzf /tmp/atlassian-*-x86_64-unknown-linux-gnu.tar.gz -C "$HOME/.local/bin"
chmod +x "$HOME/.local/bin/jira"
ln -sf "$HOME/.local/bin/jira" "$HOME/.local/bin/confluence"
ln -sf "$HOME/.local/bin/jira" "$HOME/.local/bin/conf"
```

---

### 단일 인증 (Cloud API Token & Data Center PAT)

`jira login`, `confluence login`, `conf login` 중 어느 명령으로 로그인해도 동일한 자격 증명 저장소(OS 키링 + `credentials.json` 폴백)와 `config.toml`에 저장됩니다.

```bash
# 1. Atlassian Cloud (Email + API Token -> Basic auth)
jira login \
  --url https://acme.atlassian.net \
  --email user@acme.com \
  --token "$ATLASSIAN_API_TOKEN"

# 2. Data Center / Server (PAT -> Bearer auth, Jira/Confluence 호스트 분리 시 --confluence-url 지정)
jira login \
  --url https://jira.company.internal \
  --confluence-url https://confluence.company.internal \
  --deployment datacenter \
  --pat "$ATLASSIAN_PAT"

# 3. 환경 변수로 일시 주입 (CLI 플래그/CI용)
export ATLASSIAN_SITE_URL="https://acme.atlassian.net"
export ATLASSIAN_EMAIL="user@acme.com"
export ATLASSIAN_API_TOKEN="..."
# 또는 Data Center PAT:
export ATLASSIAN_SITE_URL="https://jira.company.internal"
export ATLASSIAN_CONFLUENCE_URL="https://confluence.company.internal"
export ATLASSIAN_DEPLOYMENT="datacenter"
export ATLASSIAN_PAT="..."

# 로그인 상태 확인 및 연결 진단
jira whoami
jira doctor
```

---

### 원클릭 자가 업데이터 (`jira update` / `confluence update`)

```bash
# 최신 버전 확인만 수행
jira update --check

# 최신 릴리즈 다운로드, SHA256SUMS.txt 무결성 검증 및 바이너리 안전 교체
jira update
# 또는 동일 버전 강제 재설치
confluence update --force
```

---

## 트리플 명령어 & 페르소나 (`jira`, `confluence`, `conf`)

| 명령어 | 기본 페르소나 | `ATLASSIAN_PERSONA` | 초기 진입 화면 / 역할 |
| --- | --- | --- | --- |
| `jira` | Jira | `jira` (기본값) | 애자일 칸반 보드 · 이슈 목록 · 인스펙터 중심 |
| `confluence` | Confluence | `confluence` | 스페이스 · 계층형 페이지 트리 · 리치 문서 리더/에디터 중심 |
| `conf` | Confluence | `confluence` | `confluence`의 짧은 별칭 (동일 기능 및 모든 CLI 하위 명령 공유) |

---

## TUI 가이드 & 단축키 치트시트

`jira`, `confluence`, `conf`를 인자 없이 실행하거나 `jira tui` / `confluence tui`를 실행하면 풀스크린 TUI가 열립니다.

| 키 | 동작 | 적용 영역 |
| --- | --- | --- |
| `1` ~ `6` / `Ctrl+1` ~ `6` / `Alt+1` ~ `6` | 워크스페이스 탭 전환 (Board / Issues / Confluence / Search / Sync / Settings) | 전역 |
| `Tab` / `Shift+Tab` | 다음 / 이전 패널 포커스 이동 | 전역 |
| `j` / `k` / `↑` / `↓` | 목록 · 트리 · 본문 위/아래 이동 | 목록 / 트리 / 리더 |
| `h` / `l` / `←` / `→` | 칸반 컬럼 좌우 이동 (Confluence 트리에서는 접기/펼치기) | 보드 / 트리 |
| `Space` | Confluence 트리 노드 토글 / 페이지 다운 | 트리 / 일반 |
| `Enter` | 항목 열기 · 선택 · 활성화 | 전역 |
| `m` | Jira 이슈 상태 전이(Transition) 오버레이 열기 | Jira 보드 / 이슈 |
| `i` | 현재 이슈를 나에게 할당 (Assign to me) | Jira 보드 / 이슈 |
| `a` | 담당자 검색 및 지정 오버레이 열기 | Jira 보드 / 이슈 |
| `c` | 코멘트 작성 오버레이 열기 | Jira / Confluence |
| `n` | 새 Jira 이슈 또는 새 Confluence 페이지 생성 | Jira / Confluence |
| `e` | 선택한 이슈 또는 Confluence 페이지 편집 (`Ctrl+E`: 외부 `$EDITOR`) | Jira / Confluence |
| `u` / `d` | 첨부파일 업로드 / 다운로드 | Jira / Confluence |
| `v` | 첨부 이미지 인라인 미리보기 모달 열기 (Truecolor Half-Block `▀`) | Jira / Confluence |
| `g` | `Jira <-> Confluence` 크로스 앱 링크 점프 오버레이 열기 | 이슈 / 페이지 |
| `Backspace` / `Alt+←` | 크로스 앱 내비게이션 이전 기록으로 뒤로 가기 | 전역 |
| `o` | 현재 이슈 또는 페이지를 기본 웹 브라우저로 열기 | 이슈 / 페이지 |
| `J` | JQL 입력 바 포커스 (Jira) | Jira |
| `/` | 현재 뷰 필터 / 로컬 검색 바 포커스 | 전역 |
| `Ctrl+R` | 원격 JQL / CQL 통합 검색 실행 | 전역 |
| `t` | 우측 상세 패널 / Confluence 목차(TOC) 드로어 토글 | 전역 |
| `p` | 프로젝트 또는 스페이스 즐겨찾기 고정(Pin) 토글 | 네비게이션 |
| `r` | 전체 백그라운드 동기화(`SyncAll`) 실행 | 전역 |
| `U` | GitHub Release 업데이트 확인 (`CheckUpdate`) | 전역 |
| `:` / `Ctrl+P` | 커맨드 팔레트 열기 | 전역 |
| `?` | 단축키 도움말 오버레이 열기 | 전역 |
| `Esc` | 현재 모달 / 오버레이 / 입력 모드 닫기 | 오버레이 / 입력 |
| `q` / `Ctrl+C` | TUI 종료 | 전역 |

---

## CLI 빠른 레퍼런스

> 모든 CLI 명령은 `--json` 플래그(또는 기계 가독 JSON 출력)와 스크립팅 친화적인 종료 코드를 지원합니다. 자세한 스펙은 [docs/API.md](docs/API.md) 및 [docs/MCP.md](docs/MCP.md)를 참고하세요.

```bash
# 인증 및 진단
jira login --url https://acme.atlassian.net --email me@acme.com --token "$TOKEN"
jira logout
jira whoami
jira doctor

# Jira 프로젝트 · 보드 · 이슈
jira projects
jira boards --project PROJ
jira board 11
jira issues --jql "project = PROJ AND statusCategory != Done ORDER BY updated DESC"
jira view PROJ-101 --preview-images
jira create --project PROJ --type Task --summary "배포 체크리스트 정리" --body "..."
jira transition PROJ-101 "In Progress" --comment "작업 시작"
jira assign PROJ-101 --to me
jira comment PROJ-101 "확인 완료했습니다"
jira attach PROJ-101 ./screenshot.png
jira preview PROJ-101

# Confluence 스페이스 · 페이지 · 검색
confluence spaces
conf pages ENG
conf view 123456 --preview-images
conf create --space ENG --title "릴리즈 노트" --body "# 배포 안내"
conf edit 123456 --title "릴리즈 노트 v0.1.0" --body "..."
conf comment 123456 "검토 완료"
conf search "배포"

# 오프라인 동기화 · 통합 검색 · 서버 · 업데이트
jira sync
jira search "배포"
jira serve --bind 127.0.0.1 --port 8789 --token "$API_TOKEN"
jira mcp
jira update --check
```

---

## HTTP JSON API (`jira serve`)

`jira serve` (또는 `confluence serve`)를 실행하면 기본 `127.0.0.1:8789`에서 `axum` 기반 로컬 HTTP JSON API 서버와 백그라운드 동기화 엔진이 시작됩니다. `--token` 옵션을 지정하면 `/health`를 제외한 모든 엔드포인트에 `Authorization: Bearer <token>` 헤더가 필요합니다.

| 메서드 | 경로 | 설명 |
| --- | --- | --- |
| `GET` | `/health` | 서버 상태, 버전, 배포 유형(`cloud`/`datacenter`), 사이트 URL 반환 (인증 예외) |
| `GET` | `/myself` | 현재 인증된 사용자 프로필 조회 |
| `GET` | `/projects` | Jira 프로젝트 목록 (`?refresh=true`) |
| `GET` | `/boards` | Jira 애자일 보드 목록 (`?project=PROJ&refresh=true`) |
| `GET` | `/boards/{board_id}/sprints` | 보드 스프린트 목록 (`?offline=true`) |
| `GET` | `/boards/{board_id}/issues` | 보드/스프린트 이슈 목록 (`?sprint_id=...&limit=...&offline=true`) |
| `GET` / `POST` | `/issues` | JQL 이슈 검색(`?jql=...&project=...&page_token=...&offline=true`) / 새 이슈 생성 |
| `GET` / `PATCH` / `DELETE` | `/issues/{key}` | 이슈 상세 조회(연결된 Confluence 페이지·서브태스크 포함) / 수정 / 삭제 |
| `GET` / `POST` | `/issues/{key}/transitions` | 가능한 상태 전이 목록 조회 / 이슈 상태 전이 실행 |
| `PUT` | `/issues/{key}/assignee` | 이슈 담당자 지정 또는 해제 |
| `GET` / `POST` | `/issues/{key}/comments` | 이슈 코멘트 목록 조회 / 코멘트 추가 |
| `GET` | `/spaces` | Confluence 스페이스 목록 (`?refresh=true&limit=...`) |
| `GET` | `/spaces/{space}/pages` | 스페이스 루트 페이지 목록 (`?limit=...&offline=true`) |
| `POST` | `/pages` | 새 Confluence 페이지 생성 |
| `GET` / `PUT` / `DELETE` | `/pages/{id}` | 페이지 상세 조회(TOC·이슈 크로스 링크 포함) / 수정 / 삭제 |
| `GET` | `/pages/{id}/children` | 하위 페이지 목록 조회 |
| `GET` / `POST` | `/pages/{id}/comments` | 페이지 코멘트 조회 / 추가 |
| `GET` | `/search` | Jira + Confluence 통합 검색 (`?q=배포&scope=all|jira|confluence&remote=true`) |
| `POST` | `/sync` | 백그라운드 워크스페이스 동기화 트리거 |

자세한 요청/응답 스키마는 [docs/API.md](docs/API.md) 및 [docs/openapi.yaml](docs/openapi.yaml)을 참고하세요.

---

## MCP 서버 (`jira mcp` — 26개 도구)

`jira mcp`는 Claude Desktop, Claude Code, Codex 등 MCP 호스트와 stdio JSON-RPC 2.0(줄바꿈 구분)으로 통신하는 26개 도구 서버입니다. 삭제 등 파괴적 작업 도구는 실수를 방지하기 위해 `confirm: true` 파라미터 가드를 요구합니다.

Claude Desktop / Claude Code 설정 예시 (`mcpServers`):

```json
{
  "mcpServers": {
    "atlassian": {
      "command": "jira",
      "args": ["mcp"]
    }
  }
}
```

전체 26개 MCP 도구 카탈로그 및 인자 스키마는 [docs/MCP.md](docs/MCP.md)를 참고하세요.

---

## 빌드, 런처 및 npm 배포

저장소 루트에는 소스 실행 및 패키지 배포를 위한 런처와 스크립트가 포함되어 있습니다.

- **Windows 런처**: `jira.cmd`, `confluence.cmd`, `conf.cmd` (`target\release\jira.exe` 존재 시 바로 실행 -> 없으면 `gh release download` -> 최후 수단으로 `cargo build --release`)
- **Unix (POSIX `sh`) 런처**: `jira`, `confluence`, `conf` (LF 줄바꿈, `confluence` / `conf`는 `ATLASSIAN_PERSONA=confluence` 설정 후 `./jira` 실행)
- **npm 멀티 플랫폼 패키지**: `@shinick/atlassian` (`bin/jira.js`, `bin/confluence.js`, `bin/conf.js`) + 6개 플랫폼 optionalDependency 패키지 (`win32-x64`, `win32-arm64`, `darwin-x64`, `darwin-arm64`, `linux-x64`, `linux-arm64`)
- **버전 동기화 & 스테이징**:
  ```bash
  node scripts/sync-version.mjs --check
  node scripts/build-npm.mjs
  ```
- **GitHub Actions CI/CD**: `.github/workflows/release.yml` (`v*` 태그 푸시 시 6개 타깃 빌드, npm 퍼블리시, `atlassian-<ver>-<target>.zip` / `.tar.gz` + `jira.exe` + `SHA256SUMS.txt` GitHub Release 자동 생성)

자세한 npm 배포 및 트러블슈팅 가이드는 [docs/NPM.md](docs/NPM.md)를 참고하세요.

---

## 설정 및 데이터 저장 위치

| 항목 | 기본 경로 (`ATLASSIAN_CONFIG_DIR`로 재정의 가능) |
| --- | --- |
| 설정 파일 (`config.toml`) | Windows: `%APPDATA%\atlassian-cli\config.toml` / Unix: `~/.config/atlassian-cli/config.toml` |
| 자격 증명 | OS 키링 (`atlassian-cli` 서비스) + 폴백 `credentials.json` |
| 로컬 SQLite FTS5 캐시 | Windows: `%LOCALAPPDATA%\atlassian-cli\<profile>\cache.db` / Unix: `~/.local/share/atlassian-cli/<profile>/cache.db` |
| 임시 초안 (`drafts.json`) | `%APPDATA%\atlassian-cli\drafts.json` / `~/.config/atlassian-cli/drafts.json` |
| 첨부파일 기본 다운로드 | `<OS Downloads>\atlassian` |
