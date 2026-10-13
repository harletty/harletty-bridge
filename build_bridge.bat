@echo off
setlocal

rem Build the three bridge plugins, one per codec family, as a release does:
rem harletty_dolby_bridge.dll, harletty_dts_bridge.dll and
rem harletty_iamf_bridge.dll. A host loads them together.
rem
rem The IAMF plugin links libopus statically. With OPUS_LIB_DIR unset, it is
rem built from source first with scripts\build-static-opus.sh, which needs
rem Git Bash (bash on PATH) and CMake; or build libopus yourself and set
rem OPUS_LIB_DIR to the folder holding opus.lib, and OPUS_STATIC=1.

set "REPO_DIR=%~dp0"
cd /d "%REPO_DIR%"

if not defined OPUS_LIB_DIR (
  where bash >nul 2>nul
  if errorlevel 1 (
    echo OPUS_LIB_DIR is not set and bash is not on PATH: cannot build libopus for the IAMF plugin.
    exit /b 1
  )
  for /f "usebackq tokens=1,* delims==" %%A in (`bash scripts/build-static-opus.sh ^| findstr /b "OPUS_"`) do set "%%A=%%B"
)
if not defined OPUS_LIB_DIR (
  echo Building libopus failed.
  exit /b 1
)
if not defined OPUS_STATIC set "OPUS_STATIC=1"

rem -p: the workspace also holds the offline CLI; only build the plugins.
cargo build --release -p harletty-dolby-bridge -p harletty-dts-bridge -p harletty-iamf-bridge
if errorlevel 1 exit /b %errorlevel%

for %%F in (dolby dts iamf) do (
  if not exist "%REPO_DIR%target\release\harletty_%%F_bridge.dll" (
    echo Build succeeded but artifact not found: %REPO_DIR%target\release\harletty_%%F_bridge.dll
    exit /b 1
  )
  echo Built %%F bridge: %REPO_DIR%target\release\harletty_%%F_bridge.dll
)
