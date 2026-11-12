@echo off
rem Removes Swapdesk from this PC (asks for administrator rights once). Arguments go to uninstall.ps1.
rem Started with no argument (a double click, or Settings > Apps), it waits for a key at the end so the window stays
rem readable; set SWAPDESK_NOPAUSE=1 to skip that.
rem Run from the install folder this file deletes itself, and cmd must not read it again: the last line ends the batch
rem context with (goto), after which only the commands left on that line run (the `call` lines expand their variables then).
rem The folder is left first so it can be deleted.
set "SWAPDESK_PAUSE=cd ."
if "%~1"=="" if not defined SWAPDESK_NOPAUSE set "SWAPDESK_PAUSE=pause"
if /i "%CD%\"=="%~dp0" cd /d "%SystemRoot%"
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0uninstall.ps1" %* & call set "SWAPDESK_RC=%%ERRORLEVEL%%" & (goto) 2>nul & call %%SWAPDESK_PAUSE%% & call exit /b %%SWAPDESK_RC%%
