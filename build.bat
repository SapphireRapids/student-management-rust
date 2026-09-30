@echo off
chcp 65001 >nul
setlocal
cd /d %~dp0

where cargo >nul 2>nul
if errorlevel 1 (
  echo [ERROR] cargo not found. Install Rust from https://rustup.rs and reopen this window.
  exit /b 1
)

cargo build --release
if errorlevel 1 (
  echo [ERROR] cargo build failed.
  exit /b 1
)

copy /y "target\release\sms.exe" "%~dp0sms.exe" >nul
if errorlevel 1 (
  echo [ERROR] failed to copy sms.exe to the project root.
  exit /b 1
)

echo.
echo [OK] sms.exe built, copied next to index.html.
echo Usage:
echo   sms.exe            console menu + web server on port 4399
echo   sms.exe --server   web server only
exit /b 0
