@echo off
setlocal EnableExtensions

set "SERVICE=DiskWatch"
set "BIN_DIR=%~dp0"
for %%I in ("%BIN_DIR%..") do set "ROOT=%%~fI"
set "LIVE_EXE=%BIN_DIR%diskwatch.exe"
set "NEW_EXE=%ROOT%\target\release\diskwatch.exe"
set "CARGO=D:\al\rust\cargo\bin\cargo.exe"
set "DRY_RUN=0"
if /i "%~1"=="--dry-run" set "DRY_RUN=1"

if not exist "%CARGO%" set "CARGO=cargo.exe"

if "%DRY_RUN%"=="0" (
    fltmc >nul 2>&1
    if errorlevel 1 (
        echo Requesting Administrator permission...
        powershell.exe -NoProfile -Command "Start-Process -FilePath '%~f0' -Verb RunAs"
        exit /b
    )
)

echo.
echo ============================================================
echo  DiskWatch build, test and safe deployment
if "%DRY_RUN%"=="1" echo  DRY RUN: service and deployed executable will not be changed
 echo ============================================================
echo.

echo [1/6] Running tests...
pushd "%ROOT%"
"%CARGO%" test
if errorlevel 1 (
    popd
    echo.
    echo ERROR: Tests failed. Nothing was deployed.
    if "%DRY_RUN%"=="0" pause
    exit /b 1
)

echo.
echo [2/6] Building release executable...
"%CARGO%" build --release
if errorlevel 1 (
    popd
    echo.
    echo ERROR: Release build failed. Nothing was deployed.
    if "%DRY_RUN%"=="0" pause
    exit /b 1
)
popd

if not exist "%NEW_EXE%" (
    echo ERROR: Build succeeded but %NEW_EXE% was not found.
    if "%DRY_RUN%"=="0" pause
    exit /b 1
)

if "%DRY_RUN%"=="1" (
    echo.
    echo DRY RUN PASSED: tests and release build succeeded.
    exit /b 0
)

for /f %%I in ('powershell.exe -NoProfile -Command "Get-Date -Format yyyyMMdd-HHmmss"') do set "STAMP=%%I"
set "BACKUP=%BIN_DIR%diskwatch.exe.bak-%STAMP%"
set "HAVE_BACKUP=0"
set "SERVICE_EXISTS=0"
sc.exe query "%SERVICE%" >nul 2>&1
if not errorlevel 1 set "SERVICE_EXISTS=1"

if "%SERVICE_EXISTS%"=="1" (
    echo.
    echo [3/6] Stopping %SERVICE% gracefully...
    sc.exe stop "%SERVICE%" >nul 2>&1
    call :wait_stopped
    if errorlevel 1 (
        echo ERROR: %SERVICE% did not stop within 30 seconds. Nothing was replaced.
        pause
        exit /b 1
    )
) else (
    echo.
    echo [3/6] %SERVICE% is not installed. It will be installed after copying the executable.
)

if exist "%LIVE_EXE%" (
    echo.
    echo [4/6] Backing up current executable...
    copy /y "%LIVE_EXE%" "%BACKUP%" >nul
    if errorlevel 1 (
        echo ERROR: Could not create backup %BACKUP%
        call :start_if_existing
        pause
        exit /b 1
    )
    set "HAVE_BACKUP=1"
    echo Backup: %BACKUP%
) else (
    echo.
    echo [4/6] No existing executable to back up.
)

echo.
echo [5/6] Installing new executable...
copy /y "%NEW_EXE%" "%LIVE_EXE%" >nul
if errorlevel 1 (
    echo ERROR: Could not replace %LIVE_EXE%
    goto rollback
)

if "%SERVICE_EXISTS%"=="0" (
    call "%BIN_DIR%install-service.cmd"
    if errorlevel 1 goto rollback
) else (
    sc.exe start "%SERVICE%" >nul 2>&1
    call :wait_running
    if errorlevel 1 (
        echo ERROR: %SERVICE% did not reach RUNNING state.
        goto rollback
    )
)

echo.
echo [6/6] Checking Web API...
call :wait_health
if errorlevel 1 (
    echo ERROR: Service is running but http://127.0.0.1:8421/api/status did not respond.
    goto rollback
)

echo.
echo ============================================================
echo  DEPLOYMENT SUCCEEDED
 echo ============================================================
sc.exe query "%SERVICE%" | findstr /C:"STATE"
echo.
echo Web UI: http://127.0.0.1:8421/
echo Backup: %BACKUP%
echo.
echo Current process memory:
powershell.exe -NoProfile -Command "Get-Process diskwatch -ErrorAction SilentlyContinue | Select-Object Id,@{N='WorkingSetMB';E={[math]::Round($_.WorkingSet64/1MB,1)}},@{N='PrivateMB';E={[math]::Round($_.PrivateMemorySize64/1MB,1)}} | Format-Table -AutoSize"
pause
exit /b 0

:rollback
echo.
echo Deployment failed. Rolling back...
sc.exe stop "%SERVICE%" >nul 2>&1
call :wait_stopped >nul 2>&1
if "%HAVE_BACKUP%"=="1" (
    copy /y "%BACKUP%" "%LIVE_EXE%" >nul
    if errorlevel 1 (
        echo CRITICAL: Automatic rollback copy failed.
        echo Restore manually: %BACKUP%
        pause
        exit /b 2
    )
    echo Old executable restored.
)
call :start_if_existing
echo Rollback finished.
pause
exit /b 1

:start_if_existing
sc.exe query "%SERVICE%" >nul 2>&1
if errorlevel 1 exit /b 0
sc.exe start "%SERVICE%" >nul 2>&1
call :wait_running
exit /b 0

:wait_stopped
for /l %%I in (1,1,30) do (
    sc.exe query "%SERVICE%" 2>nul | findstr /C:"STOPPED" >nul && exit /b 0
    timeout /t 1 /nobreak >nul
)
exit /b 1

:wait_running
for /l %%I in (1,1,30) do (
    sc.exe query "%SERVICE%" 2>nul | findstr /C:"RUNNING" >nul && exit /b 0
    timeout /t 1 /nobreak >nul
)
exit /b 1

:wait_health
for /l %%I in (1,1,15) do (
    curl.exe --fail --silent --max-time 2 "http://127.0.0.1:8421/api/status" >nul 2>&1 && exit /b 0
    timeout /t 1 /nobreak >nul
)
exit /b 1
