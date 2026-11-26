# 在 Windows 把 Unity 项目一键装到自己的 iPhone

只需双击 `Unity-IPA-Installer.exe`（或 `Build-Unity-Ipa.cmd`），插上自己的 iPhone 并选择 Unity 工程根目录，程序会自动完成环境检测、缺失项引导/安装、云端编译与真机直装。

## 第一次从 Unity 工程生成并安装 IPA

1. **解压工具包并关闭工程编辑器**
   - 从解压后的目录双击运行 `Unity-IPA-Installer.exe`（保持同目录的许可证文件即可）。
   - 向导菜单默认选 `1 · 从 Unity 工程一键打包并安装到 iPhone`，按 **Enter** 后选择包含 `Assets` 和 `ProjectSettings` 的工程文件夹。
   - **注意**：在导出之前请确认已在 Unity 的 **File → Build Profiles** 勾选了游戏主场景，并关闭正在占用该工程的 Unity 窗口。

2. **自动检测并补齐 Unity 与 iOS Build Support**
   - 工具会读取 `ProjectSettings/ProjectVersion.txt` 检查精确版本与 `iOSSupport` 模块。
   - 若缺失，程序按以下顺序自动帮你处理：
     1. 调用 **Unity Hub CLI** 自动下载安装缺失的编辑器或 `iOS Build Support`；
     2. 自动查询 Unity 官方下载通道拉取 `UnitySetup-iOS-Support-for-Editor` 安装器（会弹出一次系统 UAC 确认）；
     3. 若是特殊版本或离线环境，自动打开 Unity Hub / Unity 官网，等你装好后按 **Enter** 原地复检继续。

3. **自动打开浏览器授权 GitHub（无需安装 Git 或 `gh`）**
   - 首次使用时，程序会自动打开 GitHub 设备授权网页并在黑框显示 8 位验证码。
   - 登录你的 GitHub 账号、输入验证码并确认授权。
   - 回到黑框核对目标私有仓库名（默认 `<你的用户名>/unity-ios-builds`），按 **Enter** 开始上传 Xcode 归档并触发 GitHub Actions 的 macOS 云端编译（个人私有仓库会有对应免费/计费 Actions 额度提示）。

4. **自动检测 Apple Devices 并直装到 iPhone**
   - 若电脑未装 Apple USB 驱动，程序会自动打开微软商店 **Apple Devices（Apple 设备）** 页面，装完按 **Enter** 复检即可。
   - 用数据线连接并解锁 iPhone；若手机弹出「信任此电脑」，点 **信任** 并输入锁屏密码。
   - 提示「开发者模式未开启」时，打开手机 **设置 → 隐私与安全性 → 开发者模式**，打开并按提示重启解锁确认，回到 Windows 窗口按 **Enter** 继续。
   - 核对手机名称后，输入你自己的 Apple 账户、密码与双重验证码完成本地签名并安装（Apple 账户仅在本机与 Apple 官方 / Anisette 签名服务通信，绝不上传 GitHub）。

5. **首次打开 App 的手机信任操作**
   - 安装完成后点 iPhone 桌面的应用图标，若提示「不受信任的开发者」，打开 **设置 → 通用 → VPN 与设备管理 → 开发者 App**，选择你的 Apple 账户点 **信任**；如出现 **允许并重新启动** 按提示操作即可。

> 💡 **7 天签名过期怎么续签？**  
> 免费个人 Apple 账户签名有效期为 7 天。到期后不需要重新编译，直接双击 `Unity-IPA-Installer.exe` 选 `2 · 安装已有的 .ipa 文件到 iPhone`，选中之前保存的 `.ipa`（位于 `%LOCALAPPDATA%\UnityIpaWorkflow\Builds\<构建ID>\IPA\`）即可秒重装。

## 常见问题速查

| 屏幕上的情况 | 处理方式 |
| --- | --- |
| 提示 Build Profiles 场景列表为空 | 打开 Unity → `File → Build Profiles` 勾选主场景，保存后关闭 Unity 再试 |
| Unity Hub 自动安装失败 | 按提示继续使用方案 2（官方独立安装器）或方案 3 在 Unity Hub 界面手动添加 `iOS Build Support` |
| 需要切换另一个 GitHub 账号 | 运行 `Unity-IPA-Installer.exe --github-logout`，下次打包会重新打开授权页 |
| 云端编译失败 | 打开窗口打印的 GitHub Actions 链接，查看 Xcode 编译步骤日志 |
| 手机上传时掉线 | 保持窗口不要关闭，重新插线并解锁 iPhone 后按 **Enter** 继续 |
