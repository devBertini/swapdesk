@echo off
rem Firewall rules of a Swapdesk development instance (asks for administrator rights once). Arguments go to
rem dev-firewall.ps1; read its header first.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0dev-firewall.ps1" %*
exit /b %ERRORLEVEL%
