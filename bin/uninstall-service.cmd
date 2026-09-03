@echo off
setlocal
rem Uninstall the diskwatch service (must run as Administrator).
rem Note: the old "DiskWatch Collect" scheduled task was already removed
rem during install and is NOT restored by this script.

echo [1/2] Stopping service DiskWatch...
sc stop DiskWatch >nul 2>&1
timeout /t 2 /nobreak >nul

echo [2/2] Deleting service DiskWatch...
sc delete DiskWatch
if errorlevel 1 (
  echo WARN: sc delete failed. Run this script as Administrator.
  exit /b 1
)

echo Done. Service removed. The nightly "DiskWatch Report" task (if present) is untouched.
echo Data remains in %~dp0data\diskwatch.db
endlocal
