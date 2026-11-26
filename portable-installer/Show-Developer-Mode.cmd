@echo off
"%~dp0Unity-IPA-Installer.exe" --show-developer-mode
set "UNITY_IPA_DEV_EXIT=%errorlevel%"
echo.
pause
exit /b %UNITY_IPA_DEV_EXIT%
