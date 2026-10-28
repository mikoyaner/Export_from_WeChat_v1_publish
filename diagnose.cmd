@echo off
rem One-click self check. No AI needed. Prints account/key/entry status.
chcp 65001 >nul
cd /d "%~dp0"
python pipeline.py --diagnose
echo.
pause
