@echo off
cd /d D:\al\tools\diskwatch\bin
if not exist logs mkdir logs
set LOG=logs\install.log
echo === install %date% %time% === >> %LOG%
schtasks /create /tn "DiskWatch Collect" /tr "D:\al\tools\diskwatch\bin\collect-start.cmd" /sc onstart /ru SYSTEM /rl highest /f >> %LOG% 2>&1
schtasks /create /tn "DiskWatch Report" /tr "D:\al\tools\diskwatch\bin\diskwatch.exe report --hours 24" /sc daily /st 21:00 /ru SYSTEM /rl highest /f >> %LOG% 2>&1
schtasks /run /tn "DiskWatch Collect" >> %LOG% 2>&1
schtasks /run /tn "DiskWatch Report" >> %LOG% 2>&1
echo === done %date% %time% === >> %LOG%
