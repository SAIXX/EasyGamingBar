@echo off
cd /d D:\EasyGammingBar
echo Starting EasyGamingBar dev (vite HMR + tauri)...
echo Keep this window OPEN - closing it stops the floating bar.
echo.
node node_modules\@tauri-apps\cli\tauri.js dev
pause
