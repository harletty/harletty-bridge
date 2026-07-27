@echo off
setlocal

set "REPO_DIR=%~dp0"
cd /d "%REPO_DIR%"

rem -p: the workspace also holds the offline truehdd CLI; only build the plugin.
cargo build --release -p harletty-bridge
if errorlevel 1 exit /b %errorlevel%

set "ARTIFACT=%REPO_DIR%target\release\harletty_bridge.dll"

if not exist "%ARTIFACT%" (
  echo Build succeeded but artifact not found: %ARTIFACT%
  exit /b 1
)

echo Built .dll bridge: %ARTIFACT%
