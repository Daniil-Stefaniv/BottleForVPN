@echo off
rem Установка без ограничений execution policy: двойной клик по этому файлу.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0Install-BottleForApps.ps1"
pause
