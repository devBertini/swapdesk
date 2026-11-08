@echo off
rem Builds and zips the Swapdesk release (no administrator rights needed). Arguments go to build-release.ps1.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0build-release.ps1" %*
exit /b %ERRORLEVEL%
