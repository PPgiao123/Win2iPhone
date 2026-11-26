# Unity IPA Installer

集成了 **Unity iOS 工程检测/自动补环境/导出 + GitHub Actions 云端编译（免 `gh` CLI） + USB 签名直装 iPhone** 的单文件向导程序。

## 双击向导与命令行

直接双击 `Unity-IPA-Installer.exe` 即可按提示选择：
1. 从 Unity 工程一键打包并安装到 iPhone
2. 安装已有的 `.ipa` 文件到 iPhone
3. 检测电脑环境与 iPhone 连接

常用命令行：

```powershell
.\Unity-IPA-Installer.exe --check-env "D:\Path\To\UnityProject"
.\Unity-IPA-Installer.exe --build "D:\Path\To\UnityProject"
.\Unity-IPA-Installer.exe --build "D:\Path\To\UnityProject" --export-only
.\Unity-IPA-Installer.exe --build "D:\Path\To\UnityProject" --skip-install --repo owner/my-ios-builds
.\Unity-IPA-Installer.exe --github-login
.\Unity-IPA-Installer.exe --github-logout
.\Unity-IPA-Installer.exe --devices
.\Unity-IPA-Installer.exe --pair-device
.\Unity-IPA-Installer.exe --show-developer-mode
.\Unity-IPA-Installer.exe --inspect "C:\path\your-app.ipa"
.\Unity-IPA-Installer.exe "C:\path\your-app.ipa"
```

## 内置 GitHub OAuth App Client ID（面向开源发布者）

为让终端用户无需自己申请 GitHub Token 或安装 `gh` CLI，发布者可在 GitHub 创建一个启用了 **Device Flow** 的 OAuth App，将公开的 `Client ID` 在编译时写入二进制（OAuth Device Flow 不需要也不会打包 Client Secret）：

```powershell
powershell -ExecutionPolicy Bypass -File ..\tools\Build-InstallerRelease.ps1 -GitHubClientId "Ov23liXXXXXXXXXXXXXX"
```

若编译时未传入 `-GitHubClientId`（且运行环境变量 `UNITY_IOS_GITHUB_CLIENT_ID` 为空），程序首次登录时会引导用户输入一次 Client ID 并保存在 `%LOCALAPPDATA%\UnityIpaWorkflow\Auth\github-client-id.txt`。

## 手动构建

在具备 Rust、MSVC C++ 工具和 Windows SDK 的终端，从 `portable-installer` 目录运行：

```powershell
$env:RUSTFLAGS = '-C target-feature=+crt-static'
cargo build --locked --release --target x86_64-pc-windows-msvc
```

生成 `target/x86_64-pc-windows-msvc/release/unity-ipa-installer.exe`。要生成完整的可分发压缩包与许可证清单，请使用 `tools/Build-InstallerRelease.ps1`。
