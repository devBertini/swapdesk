@echo off
rem Removes Swapdesk from this PC (asks for administrator rights once). Arguments go to uninstall.ps1.
rem Started with no argument (a double click), it waits for a key at the end so the window stays readable;
rem set SWAPDESK_NOPAUSE=1 to skip that.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0uninstall.ps1" %*
set "SWAPDESK_RC=%ERRORLEVEL%"
if "%~1"=="" if not defined SWAPDESK_NOPAUSE pause
exit /b %SWAPDESK_RC%
