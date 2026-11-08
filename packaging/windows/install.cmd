@echo off
rem Installs Swapdesk for all users (asks for administrator rights once). Arguments go to install.ps1.
rem Started with no argument (a double click), it waits for a key at the end so the window stays readable;
rem set SWAPDESK_NOPAUSE=1 to skip that.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0install.ps1" %*
set "SWAPDESK_RC=%ERRORLEVEL%"
if "%~1"=="" if not defined SWAPDESK_NOPAUSE pause
exit /b %SWAPDESK_RC%
