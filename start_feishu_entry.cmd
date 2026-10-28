@echo off
rem One launcher for both media modes. Double-click, then choose:
rem   [1] local      - only parse pictures/voice already on this PC (default)
rem   [2] once       - arm ONE reply command to let WeChat download missing
rem                      pictures itself (patches WeChat memory during that
rem                      request only, then restores and goes back to local)
rem No input within 20 seconds = local. Advanced users can keep injection on for
rem the whole session by setting REPLY_MEDIA_MODE=imghelper in .env.
rem Every line is also appended to logs/feishu_entry.log.
chcp 65001 >nul
cd /d "%~dp0"

set "MODE="
if /i "%~1"=="local" set "MODE=local"
if /i "%~1"=="once" set "MODE=once"
if /i "%~1"=="imghelper" set "MODE=once"

if not defined MODE (
  echo Pick a media mode:
  echo   [1] local  - only files already downloaded on this PC  (default)
  echo   [2] once   - next reply command asks WeChat to fetch missing pictures
  echo Default local is used if you press nothing within 20 seconds.
  choice /c 12 /n /t 20 /d 1 /m "1 or 2: "
  rem Match the code exactly: `if errorlevel 2` also fires on 92 (choice with no
  rem console input), which would silently arm the injection.
  if "%ERRORLEVEL%"=="2" set "MODE=once"
)
if not defined MODE set "MODE=local"

if "%MODE%"=="once" python pipeline.py --arm-media
echo Starting Feishu entry. Media mode: %MODE%. Log: logs/feishu_entry.log
node feishu_ws.cjs
echo.
echo Entry exited. The lines above are why.
pause
