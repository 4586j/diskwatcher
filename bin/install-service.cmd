@echo off
setlocal
rem Install diskwatch as a Windows service (must run as Administrator).
rem Replaces the old "DiskWatch Collect" scheduled task + watchdog.
rem The nightly "DiskWatch Report" task is preserved.

set "EXE=%~dp0diskwatch.exe"

echo [1/7] Removing old "DiskWatch Collect" scheduled task (watchdog)...
schtasks /end /tn "DiskWatch Collect" >nul 2>&1
schtasks /delete /tn "DiskWatch Collect" /f >nul 2>&1

echo [2/7] Stopping any running diskwatch process / service...
sc stop DiskWatch >nul 2>&1
sc delete DiskWatch >nul 2>&1
taskkill /f /im diskwatch.exe >nul 2>&1
timeout /t 2 /nobreak >nul

if not exist "%EXE%" (
  echo ERROR: %EXE% not found. Build the project first.
  exit /b 1
)

echo [3/7] Creating service DiskWatch...
sc create DiskWatch binPath= "\"%EXE%\" service" start= auto
if errorlevel 1 (
  echo ERROR: sc create failed. Run this script as Administrator.
  exit /b 1
)

echo [4/7] Setting delayed-auto start...
sc config DiskWatch start= delayed-auto

echo [5/7] Setting failure recovery (restart after 10s/30s/60s, reset counter after 24h)...
sc failure DiskWatch reset= 86400 actions= restart/10000/restart/30000/restart/60000

echo [6/7] Setting service description...
sc description DiskWatch "Monitors C: drive write activity via ETW, backfills offline changes from the NTFS USN journal, and raises alerts on large writes (diskwatch)."

echo [7/7] Starting service...
sc start DiskWatch
timeout /t 3 /nobreak >nul
sc query DiskWatch | findstr "STATE"

echo.
echo Done. Useful commands:
echo   sc query DiskWatch        - check status
echo   sc stop DiskWatch         - graceful stop (flushes data and updates USN bookmark)
echo   type "%~dp0logs\collect.log" - collector log
echo   curl http://127.0.0.1:8421/api/status  - heartbeat, row counts, config snapshot
echo   Web UI http://127.0.0.1:8421/  (loopback only; if the port is busy at service start,
echo           collection keeps running and event id 102 is written to the System log)
endlocal
