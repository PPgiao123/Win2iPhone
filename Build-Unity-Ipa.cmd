@echo off
if not exist "%~dp0Unity-IPA-Installer.exe" (
    chcp 65001 >nul
    echo 找不到 Unity-IPA-Installer.exe。
    echo 请从 GitHub Releases 下载并解压完整工具包（或使用 release-local 里的产物）：
    echo https://github.com/PPgiao123/Win2iPhone/releases/latest
    echo.
    pause
    exit /b 1
)
"%~dp0Unity-IPA-Installer.exe" --build %*
set "UNITY_IPA_EXIT=%errorlevel%"
echo.
pause
exit /b %UNITY_IPA_EXIT%
