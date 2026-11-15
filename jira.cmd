@echo off
chcp 65001 >nul
setlocal
set "ROOT=%~dp0"

if exist "%ROOT%target\release\jira.exe" goto run

where gh >nul 2>nul
if %errorlevel% equ 0 (
    echo [atlassian] downloading prebuilt binary from GitHub Release ^(shinick-dev/atlassian^)... >&2
    if not exist "%ROOT%target\release" mkdir "%ROOT%target\release"
    gh release download -R shinick-dev/atlassian --pattern "jira.exe" --dir "%ROOT%target\release" --clobber >nul 2>&1
    if exist "%ROOT%target\release\jira.exe" goto run
)

echo [atlassian] prebuilt binary not found - building from source with cargo... >&2
cargo build --release --manifest-path "%ROOT%Cargo.toml"
if errorlevel 1 exit /b 1

:run
"%ROOT%target\release\jira.exe" %*
exit /b %errorlevel%
