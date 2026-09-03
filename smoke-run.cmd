@echo off
"D:\al\tools\diskwatch\target\release\diskwatch.exe" collect --min-bytes 1024 --debug-events 40 --duration 25 > "D:\al\tools\diskwatch\smoke.log" 2>&1
