@echo off
cd /d D:\al\tools\diskwatch\bin
if not exist logs mkdir logs
:loop
diskwatch.exe collect >> logs\collect.log 2>&1
echo %date% %time% collector exited code %errorlevel%, restart in 30s >> logs\collect.log
timeout /t 30 /nobreak >nul
goto loop
