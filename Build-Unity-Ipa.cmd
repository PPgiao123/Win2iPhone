@echo off
"%~dp0Unity-IPA-Installer.exe" --build %*
set "UNITY_IPA_EXIT=%errorlevel%"
echo.
pause
exit /b %UNITY_IPA_EXIT%
