# Win2iPhone

**Windows 直装 Apple（iPhone）** — One-click Windows tool to build Unity iOS IPAs via GitHub Actions and install directly to your iPhone via USB.

无需自备 Mac、无需 \$99 苹果开发者年费，也无需预先安装 Git 或 GitHub CLI (`gh`)。仅用一个独立 Windows 向导程序 `Unity-IPA-Installer.exe`，即可完成 **Unity 环境检测与自动补齐 → 本地导出 Xcode 工程 → GitHub Actions macOS 云端编译 IPA → 本机签名并经 USB 直装到自己的 iPhone**。

---

## 快速开始（End-User Quickstart）

1. 前往本仓库右侧 [**Releases**](https://github.com/PPgiao123/Win2iPhone/releases) 下载最新版 `Unity-iOS-Tools-v*-windows-x64.zip` 并解压到任意英文路径。
2. 确保要打包的 Unity 工程在 **File → Build Profiles** 中已勾选至少一个主场景，并关闭正在占用该工程的 Unity 编辑器窗口。
3. 双击运行 `Unity-IPA-Installer.exe`（或 `Build-Unity-Ipa.cmd`）：
   - 选择 **1 · 从 Unity 工程一键打包并安装到 iPhone**，并选中包含 `Assets` 和 `ProjectSettings` 的工程根目录；
   - 按屏幕向导提示，在浏览器中完成一次 GitHub Device Flow 授权，插入解锁后的 iPhone，并输入自己的 Apple 账户完成本机签名安装。
4. 第一次打开应用时如提示「不受信任的开发者」，前往 iPhone **设置 → 通用 → VPN 与设备管理 → 开发者 App** 信任你的 Apple 账户即可。

> 详细图文/排错指南请看 [新手使用指南 (START-HERE.md)](START-HERE.md)。

---

## 核心特性

1. **Unity 与 `iOSSupport` 三级自动检测与补齐**
   - 自动读取 `ProjectSettings/ProjectVersion.txt` 匹配本地精确版本，并检查 `EditorBuildSettings.asset` 启用场景；
   - 缺失编辑器或 `iOS Build Support` 时依次尝试：
     1. 调用 **Unity Hub CLI** 自动下载安装缺失模块或对应版本；
     2. 查询 Unity 官方 Releases API / CDN 自动下载并拉起 `UnitySetup-iOS-Support-for-Editor` 独立安装器；
     3. 针对国内/团结引擎或离线环境，一键打开 Unity Hub / 官网并支持装完原地复检继续。
2. **免 `gh` CLI — 内置 GitHub OAuth Device Flow**
   - 直接打开浏览器跳转 GitHub 设备授权页，令牌经 **Windows DPAPI** 加密保存于本地；
   - 全自动在**用户自己账号下**创建或复用私有构建仓库、同步 [macOS 云端工作流](workflows/unity-ios-from-xcode.yml)、上传 Xcode 归档并校验下载 IPA 的 SHA-256。
3. **Apple USB 驱动自检与真机直装**
   - 自动检测 `Apple Mobile Device Service`；未安装时自动打开微软商店 **Apple Devices（Apple 设备）** 安装页并支持复检；
   - 基于开源 Rust 库 [isideload](https://github.com/nab138/isideload) 与 [idevice](https://github.com/jkcoxson/idevice) 在本机完成 Apple 登录、签名与 USB 安装，Apple 账户凭据绝不上传 GitHub。

---

## 使用前须知（边界与说明）

- **定位为真机自测工具，而非 App Store 上架工具**：使用免费个人 Apple ID 签名安装的测试应用有效期为 **7 天**（且单台设备通常最多同时装 3 个侧载 App）。到期打不开时无需重新编译，直接双击 `Unity-IPA-Installer.exe` 选 **2 · 安装已有的 .ipa 文件** 选中之前生成的 `.ipa` 即可快速续签。
- **GitHub Actions 额度说明**：云端 macOS 编译运行在**使用者自己 GitHub 账号下的私有仓库**中。GitHub 免费账号每月提供一定免费 Actions 额度，但 macOS runner 按 10 倍消耗分钟数（例如 2000 免费分钟约合 200 分钟 macOS 编译时长），超出后需按 GitHub 计费规则处理。
- **Windows SmartScreen 提示**：由于工具尚未购买商业代码签名证书，首次双击时 Windows 可能弹出「Windows 已保护你的电脑」，点击 **更多信息 → 仍要运行** 即可。
- **工程兼容性**：已在 Unity `6000.3.28f1` 上验证 Windows 导出。依赖 CocoaPods（如重度商业广告/第三方原生 SDK）、定制 Xcode Scheme 或特殊签名权限（Push / iCloud 等）的工程可能需要额外配置。

---

## 常用命令行与源码构建

```powershell
.\Unity-IPA-Installer.exe --check-env "D:\Path\To\UnityProject"
.\Unity-IPA-Installer.exe --build "D:\Path\To\UnityProject"
.\Unity-IPA-Installer.exe --build "D:\Path\To\UnityProject" --export-only
.\Unity-IPA-Installer.exe --build "D:\Path\To\UnityProject" --skip-install
.\Unity-IPA-Installer.exe --github-logout
```

- 安装器命令行参数与 OAuth Client ID 编译说明见 [portable-installer/README.md](portable-installer/README.md)。
- 打包可分发压缩包与许可证清单（含 `LGPL-2.1` 依赖合规源码）见 [tools/Build-InstallerRelease.ps1](tools/Build-InstallerRelease.ps1)。
