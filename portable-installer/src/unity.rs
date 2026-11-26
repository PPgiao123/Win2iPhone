use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    env, fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const UNITY_EXPORT_CS: &str = include_str!("../../tools/UnityIpaExport.cs");
const UNITY_RELEASES_API: &str = "https://services.unityreleases.com/v1/public/releases";
const UNITY_DOWNLOAD_WEB: &str = "https://unity.com/download";

#[derive(Debug, Clone)]
pub struct UnityProjectInfo {
    pub root: PathBuf,
    pub version: String,
    pub revision: Option<String>,
    pub enabled_scenes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ExportArtifact {
    pub tag: String,
    pub work_dir: PathBuf,
    pub archive_path: PathBuf,
    pub sha256: String,
    pub log_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IosInstallerCandidate {
    pub url: String,
    pub sha256: Option<String>,
    pub size_bytes: Option<u64>,
}

struct ExportBridgeGuard {
    script_path: PathBuf,
    meta_path: PathBuf,
    editor_dir: PathBuf,
    created_editor_dir: bool,
}

impl Drop for ExportBridgeGuard {
    fn drop(&mut self) {
        for path in [&self.script_path, &self.meta_path] {
            if path.is_file() {
                let _ = fs::remove_file(path);
            }
        }
        if self.created_editor_dir {
            let is_empty = fs::read_dir(&self.editor_dir)
                .map(|mut iter| iter.next().is_none())
                .unwrap_or(false);
            if is_empty {
                let _ = fs::remove_dir(&self.editor_dir);
                if let Some(name) = self.editor_dir.file_name().and_then(|n| n.to_str()) {
                    let editor_meta = self.editor_dir.with_file_name(format!("{name}.meta"));
                    if editor_meta.is_file() {
                        let _ = fs::remove_file(editor_meta);
                    }
                }
            }
        }
    }
}

/// Validates a Unity project root and extracts its required Unity version, revision, and enabled scenes.
pub fn inspect_project(dir: &Path) -> Result<UnityProjectInfo> {
    let canonical = dir
        .canonicalize()
        .with_context(|| format!("找不到目录 {}", dir.display()))?;
    let root = strip_verbatim_prefix(&canonical);
    let assets_dir = root.join("Assets");
    let version_path = root.join("ProjectSettings").join("ProjectVersion.txt");
    ensure!(
        assets_dir.is_dir() && version_path.is_file(),
        "请选择同时包含 Assets 和 ProjectSettings 的 Unity 工程根目录。"
    );

    let version_raw = fs::read_to_string(&version_path)
        .with_context(|| format!("无法读取 {}", version_path.display()))?;
    let (version, revision) = parse_project_version(&version_raw)
        .context("无法从 ProjectSettings/ProjectVersion.txt 读取 Unity 编辑器版本。")?;

    let build_settings_path = root.join("ProjectSettings").join("EditorBuildSettings.asset");
    let enabled_scenes = if build_settings_path.is_file() {
        let content = fs::read_to_string(&build_settings_path).unwrap_or_default();
        parse_enabled_scenes(&content)
    } else {
        Vec::new()
    };

    Ok(UnityProjectInfo {
        root,
        version,
        revision,
        enabled_scenes,
    })
}

fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    let raw = path.to_string_lossy();
    if let Some(rest) = raw.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = raw.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    path.to_path_buf()
}

/// Parses `m_EditorVersion` and optional `m_EditorVersionWithRevision` changeset hash.
pub fn parse_project_version(content: &str) -> Option<(String, Option<String>)> {
    let mut version = None;
    let mut revision = None;
    for raw_line in content.lines() {
        let line = raw_line.trim();
        if let Some(rest) = line.strip_prefix("m_EditorVersion:") {
            let val = rest.split_whitespace().next().unwrap_or("").trim();
            if !val.is_empty() {
                version = Some(val.to_owned());
            }
        } else if let Some(rest) = line.strip_prefix("m_EditorVersionWithRevision:") {
            if let (Some(start), Some(end)) = (rest.find('('), rest.find(')')) {
                if end > start + 1 {
                    let rev = rest[start + 1..end].trim();
                    if !rev.is_empty() && rev.chars().all(|c| c.is_ascii_alphanumeric()) {
                        revision = Some(rev.to_owned());
                    }
                }
            }
        }
    }
    version.map(|v| (v, revision))
}

/// Extracts enabled scene paths from `ProjectSettings/EditorBuildSettings.asset`.
pub fn parse_enabled_scenes(content: &str) -> Vec<String> {
    let mut scenes = Vec::new();
    let mut current_enabled: Option<bool> = None;
    for raw_line in content.lines() {
        let line = raw_line.trim();
        if let Some(rest) = line.strip_prefix("- enabled:") {
            current_enabled = Some(rest.trim() == "1");
        } else if let Some(rest) = line.strip_prefix("enabled:") {
            current_enabled = Some(rest.trim() == "1");
        } else if let Some(rest) = line.strip_prefix("path:") {
            if current_enabled == Some(true) {
                let path = rest.trim().trim_matches('"').trim_matches('\'');
                if !path.is_empty() {
                    scenes.push(path.to_owned());
                }
            }
            current_enabled = None;
        }
    }
    scenes
}

/// Detects Chinese Unity / Tuanjie versions (e.g. `2022.3.44f1c1` or `tuanjie`).
pub fn is_china_edition(version: &str) -> bool {
    let lower = version.to_ascii_lowercase();
    if lower.contains("tuanjie") {
        return true;
    }
    if let Some(pos) = lower.rfind('c') {
        let suffix = &lower[pos + 1..];
        let prefix = &lower[..pos];
        if !suffix.is_empty()
            && suffix.chars().all(|c| c.is_ascii_digit())
            && prefix.chars().any(|c| matches!(c, 'f' | 'a' | 'b' | 'p'))
        {
            return true;
        }
    }
    false
}

/// Checks whether `<Unity.exe parent>\Data\PlaybackEngines\iOSSupport` exists.
pub fn has_ios_support(unity_exe: &Path) -> bool {
    unity_exe
        .parent()
        .map(|dir| dir.join("Data").join("PlaybackEngines").join("iOSSupport"))
        .is_some_and(|p| p.is_dir())
}

/// Locates the matching Unity Editor executable for `version`.
pub fn find_unity_editor(version: &str) -> Option<PathBuf> {
    let appdata = env::var_os("APPDATA").map(PathBuf::from);
    let editors_json = appdata
        .as_ref()
        .map(|d| d.join("UnityHub").join("editors.json"))
        .and_then(|p| fs::read_to_string(p).ok());
    let secondary_json = appdata
        .as_ref()
        .map(|d| d.join("UnityHub").join("secondaryInstallPath.json"))
        .and_then(|p| fs::read_to_string(p).ok());
    let default_root = env::var_os("ProgramFiles")
        .map(PathBuf::from)
        .map(|p| p.join("Unity").join("Hub").join("Editor"));

    find_unity_editor_in(
        version,
        editors_json.as_deref(),
        secondary_json.as_deref(),
        default_root.as_deref(),
    )
}

pub fn find_unity_editor_in(
    version: &str,
    editors_json: Option<&str>,
    secondary_json: Option<&str>,
    default_root: Option<&Path>,
) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(raw) = editors_json {
        if let Ok(val) = serde_json::from_str::<Value>(raw) {
            if let Some(loc) = val.get(version).and_then(|v| v.get("location")) {
                match loc {
                    Value::String(s) => candidates.push(PathBuf::from(s)),
                    Value::Array(arr) => {
                        for item in arr {
                            if let Some(s) = item.as_str() {
                                candidates.push(PathBuf::from(s));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    let mut roots = Vec::new();
    if let Some(r) = default_root {
        roots.push(r.to_path_buf());
    }
    if let Some(raw) = secondary_json {
        if let Ok(val) = serde_json::from_str::<Value>(raw) {
            match val {
                Value::String(s) if !s.trim().is_empty() => roots.push(PathBuf::from(s)),
                Value::Array(arr) => {
                    for item in arr {
                        if let Some(s) = item.as_str().filter(|s| !s.trim().is_empty()) {
                            roots.push(PathBuf::from(s));
                        }
                    }
                }
                _ => {}
            }
        }
    }

    for root in roots {
        candidates.push(root.join(version).join("Editor").join("Unity.exe"));
        candidates.push(
            root.join(format!("Unity {version}"))
                .join("Editor")
                .join("Unity.exe"),
        );
    }

    candidates.into_iter().find(|p| p.is_file())
}

/// Locates `Unity Hub.exe` in standard Windows install directories.
pub fn find_unity_hub() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    for var in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(base) = env::var_os(var) {
            candidates.push(PathBuf::from(base).join("Unity Hub").join("Unity Hub.exe"));
        }
    }
    if let Some(local) = env::var_os("LOCALAPPDATA") {
        candidates.push(
            PathBuf::from(local)
                .join("Programs")
                .join("Unity Hub")
                .join("Unity Hub.exe"),
        );
    }
    candidates.into_iter().find(|p| p.is_file())
}

pub fn official_ios_installer_url(version: &str, revision: &str) -> String {
    format!(
        "https://download.unity3d.com/download_unity/{revision}/TargetSupportInstaller/UnitySetup-iOS-Support-for-Editor-{version}.exe"
    )
}

fn is_allowed_unity_download_url(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    parsed.scheme() == "https"
        && matches!(
            parsed.host_str(),
            Some("download.unity3d.com" | "netstorage.unity3d.com")
        )
}

/// Ensures both the required Unity Editor and `iOSSupport` exist, using the 3-tier remediation strategy.
pub async fn ensure_unity_and_ios_support(
    info: &UnityProjectInfo,
    http: &reqwest::Client,
) -> Result<PathBuf> {
    if let Some(exe) = find_unity_editor(&info.version) {
        if has_ios_support(&exe) {
            println!("✅ Unity {} 及 iOS Build Support 已就绪。", info.version);
            return Ok(exe);
        }
        println!(
            "⚠️ 已找到 Unity {}（{}），但缺少 iOS Build Support 模块。",
            info.version,
            exe.display()
        );
    } else {
        println!("⚠️ 本机未找到项目所需的 Unity {}。", info.version);
    }

    // Tier 1: Unity Hub CLI
    if let Some(hub) = find_unity_hub() {
        let editor_exists = find_unity_editor(&info.version).is_some();
        println!();
        println!("【方案 1】检测到 Unity Hub（{}）。", hub.display());
        let prompt_msg = if editor_exists {
            format!(
                "是否调用 Unity Hub 自动为 Unity {} 安装 iOS Build Support？按 Enter 开始，输入 s 跳过，q 取消",
                info.version
            )
        } else {
            format!(
                "是否调用 Unity Hub 自动安装 Unity {} + iOS Build Support？按 Enter 开始，输入 s 跳过，q 取消",
                info.version
            )
        };
        match crate::prompt(&prompt_msg)?.to_ascii_lowercase().as_str() {
            "" => {
                if let Err(err) = run_unity_hub_install(&hub, info, editor_exists) {
                    eprintln!("Unity Hub 自动安装未能完成：{err:#}");
                }
                if let Some(exe) = find_unity_editor(&info.version) {
                    if has_ios_support(&exe) {
                        println!("✅ Unity {} 及 iOS Build Support 安装完成！", info.version);
                        return Ok(exe);
                    }
                }
                println!("Unity Hub 执行后仍未检测到完整的 iOS Build Support，将尝试下一步方案。");
            }
            "s" => {}
            "q" => bail!("已取消操作。"),
            _ => println!("未识别输入，尝试下一步方案。"),
        }
    }

    // Tier 2: Official standalone UnitySetup-iOS-Support-for-Editor-<ver>.exe
    if let Some(exe) = find_unity_editor(&info.version) {
        if !has_ios_support(&exe) && !is_china_edition(&info.version) {
            println!();
            println!("【方案 2】正在查询 Unity 官方 iOS Build Support 独立安装包...");
            if let Some(candidate) = resolve_official_ios_installer(info, http).await {
                let size_hint = candidate
                    .size_bytes
                    .map(|b| format!("（约 {:.1} MB）", b as f64 / (1024.0 * 1024.0)))
                    .unwrap_or_default();
                println!("找到官方安装器{size_hint}：{}", candidate.url);
                match crate::prompt(
                    "按 Enter 下载并运行官方 iOS 模块安装器（会弹出系统 UAC 授权），输入 s 跳过，q 取消",
                )?
                .to_ascii_lowercase()
                .as_str()
                {
                    "" => {
                        match download_and_run_ios_installer(&candidate, &info.version, &exe, http)
                            .await
                        {
                            Ok(()) => {
                                if has_ios_support(&exe) {
                                    println!(
                                        "✅ Unity {} 的 iOS Build Support 安装完成！",
                                        info.version
                                    );
                                    return Ok(exe);
                                }
                                eprintln!("安装器已退出，但未在预期路径检测到 iOSSupport。");
                            }
                            Err(err) => {
                                eprintln!("官方独立安装器执行失败：{err:#}");
                            }
                        }
                    }
                    "s" => {}
                    "q" => bail!("已取消操作。"),
                    _ => {}
                }
            } else {
                println!("未能自动获取到匹配的官方独立安装包，将进入手动引导。");
            }
        }
    }

    // Tier 3: Guided fallback + re-check loop
    println!();
    println!("【方案 3】请手动安装所需 Unity 环境：");
    println!("  1. 需要的精确版本：{}", info.version);
    if is_china_edition(&info.version) {
        println!("  2. 检测到国内版/团结引擎版本号，请在团结引擎 Hub / Unity Hub 中国版中添加 iOS Build Support。");
    } else {
        println!("  2. 安装该版本时勾选「iOS Build Support」（已安装可在 Installs → Add modules 添加）。");
    }

    if let Some(hub) = find_unity_hub() {
        if crate::prompt("按 Enter 自动打开 Unity Hub（输入 s 跳过，q 取消）")?
            .to_ascii_lowercase()
            .as_str()
            == ""
        {
            if let Err(err) = crate::winui::launch_app(&hub) {
                eprintln!("{err:#}");
            }
        }
    } else if crate::prompt("未检测到 Unity Hub，按 Enter 打开 Unity 官网下载页（输入 s 跳过，q 取消）")?
        .to_ascii_lowercase()
        .as_str()
        == ""
    {
        if let Err(err) = crate::winui::open_url(UNITY_DOWNLOAD_WEB) {
            eprintln!("{err:#}");
        }
    }

    loop {
        let answer = crate::prompt(&format!(
            "安装好 Unity {} 及 iOS Build Support 后，按 Enter 重新检测（输入 q 取消）",
            info.version
        ))?;
        if answer.eq_ignore_ascii_case("q") {
            bail!("已取消操作。");
        }
        if let Some(exe) = find_unity_editor(&info.version) {
            if has_ios_support(&exe) {
                println!("✅ 已检测到 Unity {} 及 iOS Build Support！", info.version);
                return Ok(exe);
            }
            println!(
                "已找到 Unity {}，但仍缺少 iOSSupport 目录（{}）。",
                info.version,
                exe.parent()
                    .map(|d| d.join("Data\\PlaybackEngines\\iOSSupport"))
                    .unwrap_or_default()
                    .display()
            );
        } else {
            println!("仍未找到 Unity {}，请确认版本号完全一致后重试。", info.version);
        }
    }
}

fn run_unity_hub_install(hub: &Path, info: &UnityProjectInfo, editor_exists: bool) -> Result<()> {
    let mut cmd = Command::new(hub);
    cmd.args(["--", "--headless"]);
    if editor_exists {
        cmd.args(["install-modules", "--version", &info.version, "-m", "ios"]);
    } else {
        cmd.args(["install", "--version", &info.version]);
        if let Some(rev) = &info.revision {
            cmd.args(["--changeset", rev]);
        }
        cmd.args(["-m", "ios"]);
    }
    println!("正在执行 Unity Hub 自动安装，请耐心等待下载与安装完成...");
    let status = cmd.status().context("无法启动 Unity Hub CLI")?;
    ensure!(
        status.success(),
        "Unity Hub CLI 返回非零状态码（{}）。提示：若 Unity Hub 界面正在运行或编辑器非 Hub 安装，请改用方案 2 或在 Hub 界面中操作。",
        status
    );
    Ok(())
}

async fn resolve_official_ios_installer(
    info: &UnityProjectInfo,
    http: &reqwest::Client,
) -> Option<IosInstallerCandidate> {
    let api_url = format!(
        "{UNITY_RELEASES_API}?version={}&platform=WINDOWS",
        urlencoding::encode(&info.version)
    );
    if let Ok(resp) = tokio::time::timeout(Duration::from_secs(15), http.get(&api_url).send()).await
    {
        if let Ok(ok_resp) = resp {
            if ok_resp.status().is_success() {
                if let Ok(json) = ok_resp.json::<Value>().await {
                    if let Some(found) = extract_ios_module_from_releases_json(&json) {
                        return Some(found);
                    }
                }
            }
        }
    }

    let rev = info.revision.as_deref()?;
    let url = official_ios_installer_url(&info.version, rev);
    let head = tokio::time::timeout(Duration::from_secs(15), http.head(&url).send())
        .await
        .ok()?
        .ok()?;
    if head.status().is_success() {
        let size_bytes = head
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());
        return Some(IosInstallerCandidate {
            url,
            sha256: None,
            size_bytes,
        });
    }
    None
}

fn extract_ios_module_from_releases_json(value: &Value) -> Option<IosInstallerCandidate> {
    match value {
        Value::Array(items) => items.iter().find_map(extract_ios_module_from_releases_json),
        Value::Object(map) => {
            let id_matches = map
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|s| s.eq_ignore_ascii_case("ios"));
            let url = map.get("url").and_then(Value::as_str);
            if id_matches {
                if let Some(url) = url.filter(|u| is_allowed_unity_download_url(u)) {
                    let sha256 = map
                        .get("integrity")
                        .and_then(Value::as_str)
                        .and_then(parse_sha256_integrity)
                        .or_else(|| {
                            map.get("sha256")
                                .and_then(Value::as_str)
                                .map(|s| s.trim().to_ascii_lowercase())
                        });
                    let size_bytes = map.get("downloadSize").and_then(|d| {
                        d.get("value")
                            .and_then(Value::as_u64)
                            .or_else(|| d.as_u64())
                    });
                    return Some(IosInstallerCandidate {
                        url: url.to_owned(),
                        sha256,
                        size_bytes,
                    });
                }
            }
            for child in map.values() {
                if let Some(found) = extract_ios_module_from_releases_json(child) {
                    return Some(found);
                }
            }
            None
        }
        _ => None,
    }
}

fn parse_sha256_integrity(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix("sha256-") {
        if let Ok(bytes) = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, rest) {
            if bytes.len() == 32 {
                return Some(hex::encode(bytes));
            }
        }
    }
    if trimmed.len() == 64 && trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Some(trimmed.to_ascii_lowercase());
    }
    None
}

async fn download_and_run_ios_installer(
    candidate: &IosInstallerCandidate,
    version: &str,
    unity_exe: &Path,
    http: &reqwest::Client,
) -> Result<()> {
    ensure!(
        is_allowed_unity_download_url(&candidate.url),
        "拒绝下载非 Unity 官方域名的安装程序。"
    );
    let cache_dir = PathBuf::from(env::var_os("LOCALAPPDATA").context("无法定位 LOCALAPPDATA")?)
        .join("UnityIpaWorkflow")
        .join("Cache");
    fs::create_dir_all(&cache_dir)?;
    let installer_path = cache_dir.join(format!("UnitySetup-iOS-Support-for-Editor-{version}.exe"));

    println!("正在下载官方 iOS Build Support 安装器...");
    let resp = tokio::time::timeout(Duration::from_secs(600), http.get(&candidate.url).send())
        .await
        .context("下载 Unity iOS 模块安装器超时")?
        .context("请求 Unity iOS 模块安装器失败")?
        .error_for_status()
        .context("Unity 官方服务器返回错误状态")?;
    let bytes = tokio::time::timeout(Duration::from_secs(600), resp.bytes())
        .await
        .context("读取安装器下载流超时")?
        .context("读取安装器下载内容失败")?;

    if let Some(expected_sha) = &candidate.sha256 {
        let actual = hex::encode(Sha256::digest(&bytes));
        ensure!(
            &actual == expected_sha,
            "下载的 Unity iOS 安装器 SHA-256 校验未通过。"
        );
    }

    let mut file = fs::File::create(&installer_path)?;
    file.write_all(&bytes)?;
    file.sync_all()?;

    let editor_root = unity_exe
        .parent()
        .and_then(Path::parent)
        .unwrap_or_else(|| Path::new(""));
    let args = if !editor_root.as_os_str().is_empty() {
        Some(format!("/D={}", editor_root.display()))
    } else {
        None
    };

    println!("正在启动官方安装器，请在弹出的窗口中完成安装...");
    let code = crate::winui::run_installer_and_wait(&installer_path, args.as_deref())?;
    ensure!(code == 0, "Unity 官方 iOS 模块安装器退出码为 {code}。");
    Ok(())
}

/// Exports the Unity project to an Xcode project archive (`unity-ios-xcode.tar.gz`) locally.
pub fn export_xcode_archive(
    project: &UnityProjectInfo,
    unity_exe: &Path,
) -> Result<ExportArtifact> {
    ensure!(
        !project.enabled_scenes.is_empty(),
        "未在 Unity 的 File → Build Profiles 中勾选任何场景。请先在 Unity 中勾选至少一个主场景，保存并关闭 Unity 窗口后再试。"
    );

    let tag = build_unique_tag();
    let work_dir = PathBuf::from(env::var_os("LOCALAPPDATA").context("无法定位 LOCALAPPDATA")?)
        .join("UnityIpaWorkflow")
        .join("Builds")
        .join(&tag);
    let export_dir = work_dir.join("Xcode");
    let log_path = work_dir.join("unity-export.log");
    let archive_path = work_dir.join("unity-ios-xcode.tar.gz");
    fs::create_dir_all(&work_dir)?;

    let editor_dir = project.root.join("Assets").join("Editor");
    let script_path = editor_dir.join("UnityIpaWorkflowExport.cs");
    let meta_path = editor_dir.join("UnityIpaWorkflowExport.cs.meta");
    ensure!(
        !script_path.exists() && !meta_path.exists(),
        "工程中已存在 {}，请先确认或移除它。",
        script_path.display()
    );

    let created_editor_dir = !editor_dir.exists();
    if created_editor_dir {
        fs::create_dir_all(&editor_dir)?;
    }

    {
        let _guard = ExportBridgeGuard {
            script_path: script_path.clone(),
            meta_path,
            editor_dir,
            created_editor_dir,
        };
        fs::write(&script_path, UNITY_EXPORT_CS)
            .with_context(|| format!("无法写入导出桥接脚本 {}", script_path.display()))?;

        println!("Unity 正在导出 iOS Xcode 工程（可能需要几分钟）...");
        println!("日志路径：{}", log_path.display());

        let status = Command::new(unity_exe)
            .args([
                "-batchmode",
                "-quit",
                "-nographics",
                "-projectPath",
            ])
            .arg(&project.root)
            .args(["-buildTarget", "iOS", "-executeMethod", "UnityIpaWorkflowExport.Run", "-logFile"])
            .arg(&log_path)
            .env("UNITY_IPA_XCODE_OUTPUT", &export_dir)
            .status()
            .with_context(|| format!("无法启动 Unity {}", unity_exe.display()))?;

        let pbxproj = export_dir
            .join("Unity-iPhone.xcodeproj")
            .join("project.pbxproj");
        if !status.success() || !pbxproj.is_file() {
            bail!(
                "Unity 导出 iOS 工程失败。请确认已关闭正在编辑此项目的 Unity 窗口，并查看日志：{}",
                log_path.display()
            );
        }
    }

    println!("正在打包 Xcode 工程...");
    let tar_bin = Path::new(r"C:\Windows\System32\tar.exe");
    let tar_exe = if tar_bin.is_file() {
        tar_bin
    } else {
        Path::new("tar.exe")
    };
    let tar_status = Command::new(tar_exe)
        .arg("-czf")
        .arg(&archive_path)
        .arg("-C")
        .arg(&export_dir)
        .arg(".")
        .status()
        .context("无法调用 tar.exe 打包 Xcode 工程")?;
    ensure!(tar_status.success(), "打包 Xcode 工程失败。");

    let sha256 = sha256_file(&archive_path)?;
    println!("✅ Xcode 归档已生成：{}", archive_path.display());
    Ok(ExportArtifact {
        tag,
        work_dir,
        archive_path,
        sha256,
        log_path,
    })
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).with_context(|| format!("无法读取 {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buf)?;
        if count == 0 {
            break;
        }
        hasher.update(&buf[..count]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn build_unique_tag() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (year, month, day, hour, minute, second) = utc_from_unix(secs);
    let id = uuid::Uuid::new_v4().simple().to_string();
    format!(
        "unity-ios-{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}-{}",
        &id[..8]
    )
}

fn utc_from_unix(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = (secs % 86_400) as u32;
    let hour = rem / 3600;
    let minute = (rem % 3600) / 60;
    let second = rem % 60;

    // Howard Hinnant's civil-from-days algorithm
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if month <= 2 { 1 } else { 0 };
    (year, month as u32, day as u32, hour, minute, second)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_project_version_and_revision() {
        let raw = "m_EditorVersion: 6000.3.28f1\nm_EditorVersionWithRevision: 6000.3.28f1 (8f082d2fe476)\n";
        let (ver, rev) = parse_project_version(raw).unwrap();
        assert_eq!(ver, "6000.3.28f1");
        assert_eq!(rev.as_deref(), Some("8f082d2fe476"));
    }

    #[test]
    fn parses_enabled_scenes_only() {
        let raw = r#"
EditorBuildSettings:
  m_Scenes:
  - enabled: 0
    path: Assets/Disabled.unity
  - enabled: 1
    path: Assets/Scenes/Main.unity
"#;
        assert_eq!(
            parse_enabled_scenes(raw),
            vec!["Assets/Scenes/Main.unity"]
        );
    }

    #[test]
    fn detects_china_edition_versions() {
        assert!(is_china_edition("2022.3.44f1c1"));
        assert!(is_china_edition("Tuanjie 1.2.0"));
        assert!(!is_china_edition("6000.3.28f1"));
        assert!(!is_china_edition("2022.3.41f1"));
    }

    #[test]
    fn finds_unity_editor_from_hub_json_and_secondary_root() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let editor_exe = temp.path().join("6000.3.28f1/Editor/Unity.exe");
        fs::create_dir_all(editor_exe.parent().unwrap())?;
        fs::write(&editor_exe, b"unity")?;

        let editors_json = serde_json::json!({
            "6000.3.28f1": { "location": [editor_exe.to_string_lossy()] }
        })
        .to_string();
        assert_eq!(
            find_unity_editor_in("6000.3.28f1", Some(&editors_json), None, None),
            Some(editor_exe.clone())
        );

        let secondary_json = serde_json::json!(temp.path().to_string_lossy()).to_string();
        assert_eq!(
            find_unity_editor_in("6000.3.28f1", None, Some(&secondary_json), None),
            Some(editor_exe)
        );
        Ok(())
    }

    #[test]
    fn builds_official_ios_installer_url() {
        assert_eq!(
            official_ios_installer_url("6000.3.28f1", "8f082d2fe476"),
            "https://download.unity3d.com/download_unity/8f082d2fe476/TargetSupportInstaller/UnitySetup-iOS-Support-for-Editor-6000.3.28f1.exe"
        );
    }

    #[test]
    fn strips_windows_verbatim_prefix() {
        assert_eq!(
            strip_verbatim_prefix(Path::new(r"\\?\D:\Projects\Game")),
            PathBuf::from(r"D:\Projects\Game")
        );
        assert_eq!(
            strip_verbatim_prefix(Path::new(r"\\?\UNC\server\share\Game")),
            PathBuf::from(r"\\server\share\Game")
        );
    }

    #[test]
    fn export_bridge_guard_cleans_created_editor_folder() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let editor_dir = temp.path().join("Assets/Editor");
        fs::create_dir_all(&editor_dir)?;
        let script = editor_dir.join("UnityIpaWorkflowExport.cs");
        let meta = editor_dir.join("UnityIpaWorkflowExport.cs.meta");
        let editor_meta = temp.path().join("Assets/Editor.meta");
        fs::write(&script, b"cs")?;
        fs::write(&meta, b"meta")?;
        fs::write(&editor_meta, b"dirmeta")?;

        {
            let _guard = ExportBridgeGuard {
                script_path: script.clone(),
                meta_path: meta.clone(),
                editor_dir: editor_dir.clone(),
                created_editor_dir: true,
            };
        }

        assert!(!script.exists());
        assert!(!meta.exists());
        assert!(!editor_dir.exists());
        assert!(!editor_meta.exists());
        Ok(())
    }
}
