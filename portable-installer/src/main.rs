mod github;
mod install;
mod pairing;
mod password;
mod unity;
mod winui;

use anyhow::{Context, Result, anyhow, bail, ensure};
use idevice::usbmuxd::{UsbmuxdAddr, UsbmuxdDevice};
use isideload::{
    anisette::remote_v3::RemoteV3AnisetteProvider,
    auth::apple_account::{AppleAccount, TwoFactorCallbackParams, TwoFactorCallbackResponse},
    dev::{
        developer_session::{DeveloperSession, DevicesApi},
        teams::DeveloperTeam,
    },
    sideload::{SideloaderBuilder, TeamSelection, builder::MaxCertsBehavior},
    util::{
        callbacks::{ExtensionsBehaviorCallbackBox, MaxCertsCallbackBox},
        keyring_storage::KeyringStorage,
    },
};
use std::{
    env,
    ffi::{OsStr, OsString},
    fs::{self, File},
    io::{self, Cursor, IsTerminal, Read, Write},
    os::windows::ffi::OsStringExt,
    path::{Path, PathBuf},
    process::ExitCode,
};
use windows_sys::Win32::UI::Controls::Dialogs::{
    CommDlgExtendedError, GetOpenFileNameW, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR,
    OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use zeroize::Zeroizing;
use zip::ZipArchive;

const APP_NAME: &str = "Unity IPA Installer";
const CREDENTIAL_STORE: &str = "UnityIpaWorkflow.IPAInstaller";
const HELP: &str = "双击程序打开图形向导，或使用命令行：\n  Unity-IPA-Installer.exe                        打开向导（支持 Unity 打包与 IPA 安装）\n  Unity-IPA-Installer.exe <Unity 工程或 IPA>     自动识别并打包/安装\n  Unity-IPA-Installer.exe --build [工程目录] [--export-only] [--skip-install] [--repo owner/repo]\n  Unity-IPA-Installer.exe --export-only [工程目录]\n  Unity-IPA-Installer.exe --skip-install [工程目录]\n  Unity-IPA-Installer.exe --check-env [工程目录]\n  Unity-IPA-Installer.exe --github-login\n  Unity-IPA-Installer.exe --github-logout\n  Unity-IPA-Installer.exe --inspect [IPA 路径]\n  Unity-IPA-Installer.exe --devices\n  Unity-IPA-Installer.exe --pair-device\n  Unity-IPA-Installer.exe --show-developer-mode\n  Unity-IPA-Installer.exe --check-login-network\n  Unity-IPA-Installer.exe --verify-login";

#[derive(Default)]
struct BuildCliOptions {
    project_dir: Option<PathBuf>,
    export_only: bool,
    skip_install: bool,
    repo: Option<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<_> = env::args_os().skip(1).collect();
    let result = run(&args).await;
    if let Err(error) = &result {
        eprintln!("错误：{error:#}");
    }
    if args.is_empty() && io::stdin().is_terminal() {
        let _ = prompt("按 Enter 关闭");
    }
    if result.is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

async fn run(args: &[OsString]) -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow!("无法初始化 TLS。"))?;
    if args.is_empty() {
        return run_interactive_wizard().await;
    }

    let first = args[0].to_string_lossy();
    match first.as_ref() {
        "--help" | "-h" => {
            println!("{APP_NAME} {}\n{HELP}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "--version" => {
            println!("{APP_NAME} {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "--check-login-network" if args.len() == 1 => {
            prepare_apple_account("network-check@example.invalid").await?;
            println!("Anisette 与 Apple 登录配置已通过连接检查。");
            Ok(())
        }
        "--verify-login" if args.len() == 1 => {
            let result = apple_login().await.map(|_| ());
            let summary = match &result {
                Ok(()) => "Apple 登录和开发者会话验证成功。".to_owned(),
                Err(error) => error.to_string(),
            };
            if write_login_result(&summary).is_err() {
                eprintln!("无法保存本机验证结果。");
            }
            if result.is_ok() {
                println!("{summary}");
            }
            result
        }
        "--devices" if args.len() == 1 => {
            list_devices().await?;
            Ok(())
        }
        "--show-developer-mode" if args.len() == 1 => {
            let (address, device) = choose_device().await?;
            let (provider, _) = pairing::connect(&address, &device).await?;
            if pairing::developer_mode(&provider).await == pairing::DeveloperMode::Enabled {
                println!("开发者模式已开启。");
            } else {
                request_developer_mode(&provider).await?;
            }
            Ok(())
        }
        "--pair-device" if args.len() == 1 => {
            let (address, device) = choose_device().await?;
            let (_, info) = pairing::connect(&address, &device).await?;
            println!("USB 会话可用：{}", info.name);
            Ok(())
        }
        "--inspect" if args.len() == 1 => {
            let path = default_ipa()?.context("未找到唯一的 IPA。请指定 IPA 路径。")?;
            println!("{} · {}", path.display(), describe_ipa(&path)?);
            Ok(())
        }
        "--inspect" if args.len() == 2 => {
            println!("{}", describe_ipa(Path::new(&args[1]))?);
            Ok(())
        }
        "--github-login" if args.len() == 1 => {
            let http = github::build_http_client()?;
            let _ = github::ensure_github_auth(&http).await?;
            Ok(())
        }
        "--github-logout" if args.len() == 1 => github::logout(),
        "--check-env" if args.len() <= 2 => {
            let project_dir = args.get(1).map(PathBuf::from);
            check_environment(project_dir.as_deref()).await
        }
        "--build" | "--export-only" | "--skip-install" => {
            let opts = parse_build_args(args)?;
            build_unity_workflow(opts).await
        }
        _ if args.len() == 1 && !first.starts_with('-') => {
            let target = PathBuf::from(&args[0]);
            if target.is_dir() {
                build_unity_workflow(BuildCliOptions {
                    project_dir: Some(target),
                    ..Default::default()
                })
                .await
            } else {
                install(target).await
            }
        }
        _ => bail!("{HELP}"),
    }
}

fn parse_build_args(args: &[OsString]) -> Result<BuildCliOptions> {
    let mut opts = BuildCliOptions::default();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].to_string_lossy();
        match arg.as_ref() {
            "--build" => {}
            "--export-only" => opts.export_only = true,
            "--skip-install" => opts.skip_install = true,
            "--repo" => {
                i += 1;
                let value = args
                    .get(i)
                    .context("--repo 参数缺少 owner/repo 值。")?
                    .to_string_lossy()
                    .into_owned();
                opts.repo = Some(value);
            }
            other if other.starts_with('-') => bail!("未知参数：{other}\n{HELP}"),
            _ => {
                ensure!(
                    opts.project_dir.is_none(),
                    "只能指定一个 Unity 工程目录。"
                );
                opts.project_dir = Some(PathBuf::from(&args[i]));
            }
        }
        i += 1;
    }
    Ok(opts)
}

async fn run_interactive_wizard() -> Result<()> {
    println!("========================================");
    println!("  {APP_NAME} v{}", env!("CARGO_PKG_VERSION"));
    println!("  Windows 一键导出 Unity iOS 并直装 iPhone");
    println!("========================================");
    println!("1 · 从 Unity 工程一键打包并安装到 iPhone（推荐）");
    println!("2 · 安装已有的 .ipa 文件到 iPhone");
    println!("3 · 检测电脑环境与 iPhone 连接");
    let choice = prompt("请选择操作编号（直接按 Enter 默认选 1）")?;
    match choice.as_str() {
        "" | "1" => build_unity_workflow(BuildCliOptions::default()).await,
        "2" => match choose_ipa()? {
            Some(path) => install(path).await,
            None => Ok(()),
        },
        "3" => {
            let folder = winui::choose_folder("可选：选择 Unity 工程根目录进行检测（取消则仅检测系统环境）")?;
            check_environment(folder.as_deref()).await
        }
        _ => bail!("无效选项：{choice}"),
    }
}

async fn check_environment(project_dir: Option<&Path>) -> Result<()> {
    println!("正在检测本机环境...");
    if let Some(dir) = project_dir {
        let project = unity::inspect_project(dir)?;
        println!("工程目录：{}", project.root.display());
        println!("需要 Unity 版本：{}", project.version);
        if let Some(rev) = &project.revision {
            println!("Changeset：{}", rev);
        }
        if project.enabled_scenes.is_empty() {
            println!("⚠️ Build Profiles 场景列表为空，导出前请先在 Unity 中勾选主场景。");
        } else {
            println!("✅ 已启用场景：{}", project.enabled_scenes.join(", "));
        }
        match unity::find_unity_editor(&project.version) {
            Some(exe) => {
                let ios_ok = unity::has_ios_support(&exe);
                println!("✅ Unity Editor：{}", exe.display());
                if ios_ok {
                    println!("✅ iOS Build Support：已安装");
                } else {
                    println!("❌ iOS Build Support：未安装");
                }
            }
            None => {
                println!("❌ 未找到 Unity {}，请安装对应版本并勾选 iOS Build Support。", project.version);
            }
        }
    } else {
        println!("Unity 工程：未指定（可传入工程目录进一步检查精确版本）");
    }

    match unity::find_unity_hub() {
        Some(hub) => println!("✅ Unity Hub：{}", hub.display()),
        None => println!("⚠️ 未检测到 Unity Hub（若手动安装 Unity 可忽略）"),
    }

    if github::has_saved_token() {
        let http = github::build_http_client()?;
        match github::ensure_github_auth(&http).await {
            Ok(_) => {}
            Err(err) => println!("⚠️ GitHub 凭据检查失败：{err:#}"),
        }
    } else {
        println!("ℹ️ GitHub：尚未登录（打包时将自动打开浏览器扫码/输入验证码授权，无需安装 Git 或 gh CLI）");
    }

    if pairing::is_usbmuxd_available().await {
        println!("✅ Apple USB 驱动服务（Apple Mobile Device Service）：运行中");
        match pairing::devices().await {
            Ok((address, devs)) => {
                for (idx, dev) in devs.iter().enumerate() {
                    match pairing::check(&address, dev).await {
                        Some((provider, info)) => println!(
                            "✅ 已连接 iPhone {}：{} · {}",
                            idx + 1,
                            info.name,
                            pairing::developer_mode(&provider).await
                        ),
                        None => println!("⚠️ 已连接 USB 设备 {}（待信任/配对）", idx + 1),
                    }
                }
            }
            Err(_) => println!("ℹ️ 暂未检测到插线的 USB iPhone"),
        }
    } else {
        println!("❌ 未检测到 Apple USB 驱动服务，请在微软商店安装「Apple Devices（Apple 设备）」");
    }
    Ok(())
}

async fn build_unity_workflow(opts: BuildCliOptions) -> Result<()> {
    let project_dir = match opts.project_dir {
        Some(dir) => dir,
        None => match winui::choose_folder("请选择包含 Assets 和 ProjectSettings 的 Unity 工程根目录")? {
            Some(dir) => dir,
            None => return Ok(()),
        },
    };
    let project = unity::inspect_project(&project_dir)?;
    println!("已选择 Unity 工程：{}", project.root.display());
    println!("项目所需 Unity 版本：{}", project.version);
    if project.enabled_scenes.is_empty() {
        bail!(
            "当前工程的 Build Profiles 未启用任何场景。请先在 Unity 打开 File → Build Profiles，勾选至少一个场景并保存关闭 Unity 窗口。"
        );
    }

    let http = github::build_http_client()?;

    // 1. 环境预检与自动补齐（避免在云端或导出中途失败）
    let unity_exe = unity::ensure_unity_and_ios_support(&project, &http).await?;
    let auth = if !opts.export_only {
        Some(github::ensure_github_auth(&http).await?)
    } else {
        None
    };
    if !opts.export_only && !opts.skip_install {
        winui::ensure_apple_usb_ready().await?;
    }

    // 2. 本地导出 Xcode 工程并打包
    let export = unity::export_xcode_archive(&project, &unity_exe)?;
    if opts.export_only {
        println!("仅导出模式已完成。归档路径：{}", export.archive_path.display());
        println!("导出日志：{}", export.log_path.display());
        return Ok(());
    }

    // 3. GitHub Actions 云端编译生成无签名 IPA
    let auth = auth.context("缺少 GitHub 登录态")?;
    let ipa_path =
        github::run_cloud_ipa_build(&http, &auth, &export, opts.repo.as_deref()).await?;
    if opts.skip_install {
        println!("已跳过真机安装。IPA 保存路径：{}", ipa_path.display());
        return Ok(());
    }

    // 4. USB 签名并直装到 iPhone
    println!();
    println!("准备将生成的 IPA 签名并安装到 iPhone...");
    install(ipa_path).await
}

fn default_ipa() -> Result<Option<PathBuf>> {
    let executable = env::current_exe().context("无法确定程序所在位置")?;
    let directory = executable.parent().context("程序没有所在目录")?;
    let mut ipa = None;
    for entry in fs::read_dir(directory).context("无法读取程序所在目录")? {
        let entry = entry.context("无法读取程序所在目录的文件")?;
        let path = entry.path();
        if path.is_file() && is_ipa(&path) {
            if ipa.is_some() {
                return Ok(None);
            }
            ipa = Some(path);
        }
    }
    Ok(ipa)
}

fn is_ipa(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|ext| ext.eq_ignore_ascii_case("ipa"))
}

fn choose_ipa() -> Result<Option<PathBuf>> {
    let filter: Vec<u16> = "iOS 应用包 (*.ipa)\0*.ipa\0\0".encode_utf16().collect();
    let title: Vec<u16> = "选择要安装到 iPhone 的 IPA\0".encode_utf16().collect();
    let mut path = vec![0u16; 32_768];
    let mut dialog = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        lpstrFilter: filter.as_ptr(),
        lpstrFile: path.as_mut_ptr(),
        nMaxFile: path.len() as u32,
        lpstrTitle: title.as_ptr(),
        Flags: OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR,
        ..Default::default()
    };
    // All buffers referenced by the dialog are alive and writable for the duration of this call.
    if unsafe { GetOpenFileNameW(&mut dialog) } == 0 {
        let error = unsafe { CommDlgExtendedError() };
        ensure!(
            error == 0,
            "无法打开 IPA 文件选择窗口（Windows 错误 {error:#x}）。"
        );
        return Ok(None);
    }
    let length = path
        .iter()
        .position(|character| *character == 0)
        .unwrap_or(path.len());
    Ok(Some(PathBuf::from(OsString::from_wide(&path[..length]))))
}

fn describe_ipa(path: &Path) -> Result<String> {
    ensure!(is_ipa(path), "请选择一个 .ipa 文件。");
    let file = File::open(path).with_context(|| format!("无法打开 {}", path.display()))?;
    let mut archive = ZipArchive::new(file).context("IPA 文件不是有效的 ZIP 归档")?;
    let mut plist_path = None;
    for index in 0..archive.len() {
        let entry = archive.by_index(index).context("无法读取 IPA 内容")?;
        ensure!(
            entry.enclosed_name().is_some(),
            "IPA 内含不安全的文件路径。"
        );
        let mut parts = entry.name().split('/');
        if parts.next() == Some("Payload")
            && parts.next().is_some_and(|folder| folder.ends_with(".app"))
            && parts.next() == Some("Info.plist")
            && parts.next().is_none()
        {
            ensure!(plist_path.is_none(), "IPA 包含多个主应用。");
            plist_path = Some(entry.name().to_owned());
        }
    }
    let plist_path = plist_path.context("IPA 缺少 Payload 下的主应用 Info.plist")?;
    let entry = archive
        .by_name(&plist_path)
        .context("无法打开应用 Info.plist")?;
    let mut bytes = Vec::new();
    entry
        .take(1_048_577)
        .read_to_end(&mut bytes)
        .context("无法读取应用 Info.plist")?;
    ensure!(bytes.len() <= 1_048_576, "应用 Info.plist 过大。");
    let value = plist::Value::from_reader(Cursor::new(bytes)).context("应用 Info.plist 无效")?;
    let info = value
        .as_dictionary()
        .context("应用 Info.plist 缺少元数据")?;
    let text = |key| info.get(key).and_then(plist::Value::as_string);
    let bundle = text("CFBundleIdentifier")
        .filter(|id| !id.is_empty())
        .context("IPA 缺少 Bundle ID")?;
    let executable = text("CFBundleExecutable").context("IPA 缺少主程序名称")?;
    ensure!(
        !executable.is_empty() && !executable.contains(['/', '\\']),
        "IPA 主程序名称无效。"
    );
    let folder = plist_path
        .strip_suffix("Info.plist")
        .context("应用路径无效")?;
    ensure!(
        archive
            .by_name(&format!("{folder}{executable}"))
            .is_ok_and(|file| file.size() > 0),
        "IPA 缺少应用主程序。"
    );
    let version = text("CFBundleShortVersionString").unwrap_or("未知");
    let minimum = text("MinimumOSVersion").unwrap_or("未知");
    Ok(format!("{bundle} · {version} · 最低 iOS {minimum}"))
}

async fn choose_device() -> Result<(UsbmuxdAddr, UsbmuxdDevice)> {
    winui::ensure_apple_usb_ready().await?;
    let (address, mut devices) = loop {
        match pairing::devices().await {
            Ok(found) => break found,
            Err(err) => {
                eprintln!("{err:#}");
                if !io::stdin().is_terminal() {
                    return Err(err);
                }
                let next = prompt("请用数据线连接并解锁 iPhone，准备好后按 Enter 重试（输入 q 取消）")?;
                ensure!(!next.eq_ignore_ascii_case("q"), "已取消操作。");
            }
        }
    };
    let index = if devices.len() == 1 {
        0
    } else {
        for (index, device) in devices.iter().enumerate() {
            let label = pairing::name(&address, device)
                .await
                .unwrap_or_else(|| "未配对的 USB 设备".to_owned());
            println!("{} · {label}", index + 1);
        }
        let answer = prompt("请输入设备编号")?;
        selection(&answer, devices.len()).context("设备编号无效")?
    };
    Ok((address, devices.swap_remove(index)))
}

async fn list_devices() -> Result<()> {
    let (address, devices) = pairing::devices().await?;
    let mut ready = 0;
    for (index, device) in devices.iter().enumerate() {
        match pairing::check(&address, device).await {
            Some((provider, info)) => {
                ready += 1;
                println!(
                    "{} · {} · {}",
                    index + 1,
                    info.name,
                    pairing::developer_mode(&provider).await
                );
            }
            None => println!("{} · 需要重新配对", index + 1),
        }
    }
    ensure!(
        ready > 0,
        "USB 设备已连接。请运行 --pair-device 发起重新配对。"
    );
    Ok(())
}

async fn request_developer_mode(provider: &pairing::UsbSession) -> Result<()> {
    pairing::show_developer_mode(provider).await?;
    println!("iPhone 已接受显示 Developer Mode 入口的请求。");
    println!("请重新打开「设置 → 隐私与安全性 → 开发者模式」，开启并重启。解锁后点「打开」。");
    Ok(())
}

async fn install(path: PathBuf) -> Result<()> {
    ensure!(
        io::stdin().is_terminal(),
        "请在交互式窗口运行，以便本机输入密码和验证码。"
    );
    let path = path
        .canonicalize()
        .with_context(|| format!("无法找到 {}", path.display()))?;
    println!("待安装：{}", describe_ipa(&path)?);
    println!("正在检查 USB 设备...");
    let (address, selected) = choose_device().await?;
    let (mut provider, mut device) = pairing::connect(&address, &selected).await?;
    let mut developer_mode = pairing::developer_mode(&provider).await;
    println!("{developer_mode}");
    if developer_mode == pairing::DeveloperMode::Disabled {
        loop {
            if developer_mode == pairing::DeveloperMode::Disabled {
                if let Err(error) = request_developer_mode(&provider).await {
                    eprintln!("{error:#}。请保持手机解锁并连接后重试。");
                }
            }
            ensure!(
                prompt("在手机上开启开发者模式并解锁后，按 Enter 继续安装。输入其他字符取消")?
                    .is_empty(),
                "已取消安装。"
            );
            match pairing::reconnect(&selected.udid).await {
                Ok(connection) => {
                    (provider, device) = connection;
                    developer_mode = pairing::developer_mode(&provider).await;
                    println!("{developer_mode}");
                    if developer_mode == pairing::DeveloperMode::Enabled {
                        break;
                    }
                }
                Err(error) => {
                    eprintln!("{error:#}");
                    developer_mode = pairing::DeveloperMode::Unknown;
                }
            }
        }
    }
    ensure!(
        prompt(&format!(
            "安装到 {}？按 Enter 确认，输入其他字符取消",
            device.name
        ))?
        .is_empty(),
        "已取消安装。"
    );
    let (email, session) = apple_login().await?;
    let mut installer =
        SideloaderBuilder::<MaxCertsCallbackBox, ExtensionsBehaviorCallbackBox>::new(
            session, email,
        )
        .team_selection(TeamSelection::PromptOnce(choose_team))
        .max_certs_behavior(MaxCertsBehavior::Error)
        .storage(Box::new(KeyringStorage::new(CREDENTIAL_STORE.to_owned())))
        .machine_name(APP_NAME.to_owned())
        .build();

    println!("正在选择开发团队...");
    let team = installer
        .get_team()
        .await
        .map_err(|error| apple_error("选择开发团队", &error))?;
    println!("正在登记 iPhone...");
    installer
        .get_dev_session()
        .ensure_device_registered(&team, &device.name, &device.udid, None)
        .await
        .map_err(|error| apple_error("登记 iPhone", &error))?;

    println!("正在准备开发证书及签名...");
    let (signed, _) = installer
        .sign_app(
            path,
            Some(team),
            false,
            Some(|progress: f32| async move {
                match progress {
                    p if p == 0.1 => println!("开发证书已就绪，正在登记应用标识..."),
                    p if p == 0.15 => println!("应用标识已就绪，正在获取描述文件..."),
                    p if p == 0.3 => println!("描述文件已就绪，正在给 IPA 签名..."),
                    _ => {}
                }
            }),
            None,
            Some(&device.udid),
        )
        .await
        .map_err(|error| apple_error("IPA 签名", &error))?;

    println!("IPA 签名成功，正在打包签名后的 IPA...");
    let installed = install::signed_ipa(&selected.udid, &signed).await;
    if isideload_vfs::fs::remove_dir_all(&signed).is_err() {
        eprintln!("无法清理本次签名的临时文件。");
    }
    installed?;
    println!("iPhone 已报告安装成功。请在手机主屏幕打开应用。");
    println!("若提示「Untrusted Developer（不受信任的开发者）」，请在 iPhone 上确认信任。");
    println!("打开 Settings → General → VPN & Device Management（设置 → 通用 → VPN 与设备管理）。");
    println!("在 Developer App（开发者 App）下选择用于签名的 Apple 账户，点 Trust（信任）。");
    println!("若出现 Allow & Restart（允许并重新启动），请按提示确认，重启后再打开应用。");
    Ok(())
}

fn write_login_result(summary: &str) -> Result<()> {
    let directory = PathBuf::from(env::var_os("LOCALAPPDATA").context("无法定位应用数据目录")?)
        .join("UnityIpaWorkflow")
        .join("Diagnostics");
    fs::create_dir_all(&directory)?;
    fs::write(directory.join("apple-login-result.txt"), summary)?;
    Ok(())
}

async fn prepare_apple_account(email: &str) -> Result<AppleAccount> {
    let anisette = RemoteV3AnisetteProvider::default()
        .map_err(|_| anyhow!("无法初始化 Anisette 服务。"))?
        .set_storage(Box::new(KeyringStorage::new(format!(
            "{CREDENTIAL_STORE}.iload"
        ))));
    let mut account = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        AppleAccount::builder(email)
            .anisette_provider(anisette)
            .build(),
    )
    .await
    .context("连接 Apple 登录服务超时")?
    .map_err(|error| apple_error("获取 Apple 登录配置", &error))?;
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        account
            .anisette_generator
            .get_anisette_data(account.grandslam_client.clone()),
    )
    .await
    .context("连接 Anisette 签名服务超时")?
    .map_err(|error| apple_error("获取 Anisette 签名数据", &error))?;
    Ok(account)
}

async fn apple_login() -> Result<(String, DeveloperSession)> {
    ensure!(
        io::stdin().is_terminal(),
        "请在本机交互式终端输入 Apple 账户。"
    );
    println!("正在连接 Apple 登录服务...");
    let email = prompt("Apple 账户邮箱")?;
    ensure!(email.contains('@'), "请输入有效的 Apple 账户邮箱。");
    let mut account = prepare_apple_account(&email).await?;
    let password = password::prompt().context("无法读取 Apple 账户密码")?;
    ensure!(!password.is_empty(), "Apple 账户密码为空。");
    println!("正在登录 Apple...");
    account
        .login(&password, |params| async move { Ok(two_factor(params)) })
        .await
        .map_err(|error| apple_error("Apple 账户登录", &error))?;
    drop(password);
    println!("Apple 账户登录通过，正在验证开发者会话...");
    let session = DeveloperSession::from_account(&mut account)
        .await
        .map_err(|error| apple_error("建立 Apple 开发者会话", &error))?;
    Ok((email, session))
}

fn apple_error(stage: &str, error: &rootcause::Report) -> anyhow::Error {
    let mut phase = None;
    for entry in error.iter_reports() {
        match entry.downcast_current_context::<isideload::SideloadError>() {
            Some(isideload::SideloadError::AuthWithMessage(code, _)) => {
                return anyhow!("{stage}失败（Apple 错误码 {code}）。");
            }
            Some(isideload::SideloadError::IdeviceError(
                idevice::IdeviceError::DeveloperModeNotEnabled,
            )) => {
                return anyhow!("{stage}失败：iPhone 报告开发者模式未开启。");
            }
            Some(isideload::SideloadError::IdeviceError(
                idevice::IdeviceError::InstallationProxy(
                    idevice::installation_proxy::InstallationProxyError::OperationFailed(code),
                ),
            )) => {
                if !code.is_empty()
                    && code.len() <= 80
                    && code
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                {
                    return anyhow!("{stage}失败（iPhone 安装错误 {code}）。");
                }
                return anyhow!("{stage}失败：iPhone 的安装服务拒绝了应用。");
            }
            Some(isideload::SideloadError::DeveloperError(code, _)) => {
                return anyhow!("{stage}失败（Apple 开发者错误码 {code}）。");
            }
            _ => {}
        }
        if let Some(request) = entry.downcast_current_context::<reqwest::Error>() {
            return request_error(stage, request);
        }
        if let Some(reqwest_middleware::Error::Reqwest(request)) =
            entry.downcast_current_context::<reqwest_middleware::Error>()
        {
            return request_error(stage, request);
        }
        let context = entry.format_current_context_unhooked().to_string();
        let description = match context.as_str() {
            "Failed to get anisette data for login" | "Failed to get anisette data for 2FA" => {
                Some("Anisette 签名数据不可用")
            }
            "Failed to provision" => Some("Anisette 初始化失败"),
            "Failed to connect to provisioning socket" => Some("Anisette WebSocket 连接失败"),
            "Failed to parse initial login response" => Some("Apple 初始登录响应无效"),
            "Failed to parse proof login response" => Some("Apple 密码验证响应无效"),
            "Failed to compute SRP proof" => Some("无法计算 Apple 登录证明"),
            "Failed to retrieve certificate identity" => Some("无法读取或申请开发签名证书"),
            "Failed to sign app" => Some("无法签署应用或内嵌组件"),
            "No 2FA code provided, aborting" | "No 2FA method selected, aborting" => {
                Some("双重验证已取消")
            }
            "Failed to parse URL Bag plist" => Some("Apple 登录配置响应无效，尚未验证密码"),
            _ => None,
        };
        if description.is_some() {
            phase = description;
        }
    }
    if let Some(description) = phase {
        return anyhow!("{stage}失败：{description}。");
    }
    anyhow!("{stage}失败。服务没有返回可显示的错误码。")
}

fn request_error(stage: &str, error: &reqwest::Error) -> anyhow::Error {
    if let Some(status) = error.status() {
        return anyhow!("{stage}失败（HTTP {}）。", status.as_u16());
    }
    if error.is_timeout() {
        return anyhow!("{stage}失败（网络连接超时）。");
    }
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        let detail = cause.to_string().to_ascii_lowercase();
        if [
            "untrustedroot",
            "unknownissuer",
            "0x80090325",
            "untrusted root",
        ]
        .iter()
        .any(|needle| detail.contains(needle))
        {
            return anyhow!("{stage}失败（TLS 证书根不受信任）。");
        }
        source = cause.source();
    }
    anyhow!("{stage}失败（网络或 TLS 连接错误）。")
}

fn show_prompt(label: &str) -> io::Result<()> {
    print!("{label}：");
    io::stdout().flush()
}

fn read_prompt_line(label: &str, input: &mut String) -> io::Result<()> {
    show_prompt(label)?;
    if io::stdin().read_line(input)? == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "输入已关闭"));
    }
    Ok(())
}

fn prompt(label: &str) -> io::Result<String> {
    let mut input = String::new();
    read_prompt_line(label, &mut input)?;
    Ok(input.trim().to_owned())
}

fn selection(input: &str, count: usize) -> Option<usize> {
    let index = input.parse::<usize>().ok()?.checked_sub(1)?;
    (index < count).then_some(index)
}

fn choose_team(teams: &Vec<DeveloperTeam>) -> Option<String> {
    println!("请选择签名使用的开发团队：");
    for (index, team) in teams.iter().enumerate() {
        println!(
            "{} · {}",
            index + 1,
            team.name.as_deref().unwrap_or("未命名团队")
        );
    }
    let input = prompt("开发团队编号").ok()?;
    let index = selection(&input, teams.len())?;
    Some(teams[index].team_id.clone())
}

fn two_factor(params: TwoFactorCallbackParams) -> TwoFactorCallbackResponse {
    use TwoFactorCallbackResponse::{Abort, ResendCode, SendSms, SendToDevices, SubmitCode};

    let destination = if params.sms {
        "短信"
    } else {
        "受信任的 Apple 设备"
    };
    println!("请输入从{destination}收到的六位验证码。输入 d 改用设备通知，r 重发，q 取消。");
    for (index, number) in params.numbers.iter().enumerate() {
        println!(
            "输入 s{} 发送短信到尾号 {}。",
            index + 1,
            number.last_two_digits
        );
    }
    loop {
        let mut input = Zeroizing::new(String::new());
        if read_prompt_line("验证码或选项（输入时可见）", &mut input).is_err() {
            return Abort;
        }
        let input = input.trim();
        match input {
            "q" => return Abort,
            "d" => return SendToDevices,
            "r" if !params.unknown => return ResendCode,
            code if code.len() == 6 && code.bytes().all(|byte| byte.is_ascii_digit()) => {
                return SubmitCode(code.to_owned());
            }
            other => {
                if let Some(index) = other
                    .strip_prefix('s')
                    .and_then(|value| selection(value, params.numbers.len()))
                {
                    return SendSms(params.numbers[index].id);
                }
                eprintln!("输入无效，请输入六位数字或上方选项。");
            }
        }
    }
}
