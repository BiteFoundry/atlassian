@echo off
chcp 65001 >nul
setlocal
set "ROOT=%~dp0"
set "ATLASSIAN_PERSONA=confluence"
call "%ROOT%atlassian.cmd" %*
exit /b %errorlevel%
