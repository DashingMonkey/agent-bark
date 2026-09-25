@echo off
setlocal
cd /d "%~dp0"

echo ================================================================
echo   agent-bark build (Tauri 2 + NSIS installer)
echo ================================================================
echo.

rem A user-level CARGO_TARGET_DIR redirects cargo's output away from target\,
rem so the artifacts listed below (and the exe this script starts) would be
rem missing or stale. Warn instead of silently "succeeding" with an old exe.
if defined CARGO_TARGET_DIR (
    echo [WARN] CARGO_TARGET_DIR is set to "%CARGO_TARGET_DIR%".
    echo        Build artifacts may not be under target\ - check that path
    echo        if the expected files are missing below.
    echo.
)

where pnpm >nul 2>nul
if errorlevel 1 (
    echo [ERROR] pnpm not found. Install Node.js and pnpm first.
    goto :fail
)

where cargo >nul 2>nul
if errorlevel 1 (
    echo [ERROR] cargo not found. Install Rust from https://rustup.rs
    goto :fail
)

cd app

echo [1/5] Installing frontend dependencies...
rem --frozen-lockfile keeps the release build reproducible (pnpm's own CI default).
rem If it errors because package.json changed, run `pnpm install` once to refresh
rem pnpm-lock.yaml, then rerun this script.
call pnpm install --frozen-lockfile --prefer-offline
if errorlevel 1 goto :fail

echo.
echo [2/5] Closing a running AgentBark instance (the exe must be unlocked for linking)...
rem taskkill's errorlevel tells whether anything was killed: 0 = killed, 128 = not found
taskkill /F /IM AgentBark.exe >nul 2>nul
if not errorlevel 1 (
    rem Wait for the file lock to release before cargo links, or linking may hit Access denied.
    rem ping-based delay works even when stdin is redirected; timeout.exe would bail out
    %SystemRoot%\System32\ping.exe -n 2 127.0.0.1 >nul
    echo AgentBark was running and has been closed.
) else (
    echo AgentBark is not running.
)
rem Clear the errorlevel taskkill leaves when nothing matched (128),
rem so later checks only see results of their own commands
ver >nul

echo.
echo [3/5] Type-checking frontend...
call pnpm typecheck
if errorlevel 1 goto :fail

echo.
echo [4/5] Building frontend + Rust release. First run may take several minutes...
call pnpm tauri build
if errorlevel 1 goto :fail

echo.
echo [5/5] Starting AgentBark...
if exist "%~dp0target\release\AgentBark.exe" (
    start "" "%~dp0target\release\AgentBark.exe"
) else (
    echo [WARN] AgentBark.exe not found under target\release\ - not started.
    echo        If CARGO_TARGET_DIR is set, the exe may have been built elsewhere.
)

echo.
echo ================================================================
echo   BUILD OK
echo ================================================================
echo Installer:
if exist "%~dp0target\release\bundle\nsis\*.exe" (
    for %%f in ("%~dp0target\release\bundle\nsis\*.exe") do echo   %%~nxf
) else (
    echo   No installer found under target\release\bundle\nsis\
)
echo Location: %~dp0target\release\bundle\nsis\
echo.
goto :end

:fail
echo.
echo [FAILED] Build error. Check the messages above.
pause
exit /b 1

:end
pause
exit /b 0
