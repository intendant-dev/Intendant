@echo off
:: Double-click this file to run the Intendant LAN Access Setup.
:: Handles admin elevation and PowerShell execution policy automatically.

:: Check for admin rights
net session >nul 2>&1
if %errorlevel% neq 0 (
    echo Requesting administrator privileges...
    powershell -Command "Start-Process '%~f0' -Verb RunAs"
    exit /b
)

:: Run the PowerShell script with execution policy bypass
cd /d "%~dp0"
powershell -ExecutionPolicy Bypass -File "%~dp0setup-lan.ps1" %*
pause
