use anyhow::{Context, Result, anyhow, ensure};
use std::{
    ffi::OsString,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    ptr,
};
use windows_sys::Win32::{
    Foundation::{RPC_E_CHANGED_MODE, S_FALSE, S_OK},
    System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize},
    UI::{
        Shell::{
            BIF_EDITBOX, BIF_NEWDIALOGSTYLE, BIF_RETURNONLYFSDIRS, BROWSEINFOW, SHBrowseForFolderW,
            SHGetPathFromIDListW, ShellExecuteW,
        },
        WindowsAndMessaging::SW_SHOWNORMAL,
    },
};

const APPLE_DEVICES_STORE_URI: &str = "ms-windows-store://pdp/?productid=9NP83LWLPZ9K";
const APPLE_DEVICES_WEB_URL: &str = "https://apps.microsoft.com/detail/9np83lwlpz9k";

/// Opens a native Windows folder picker dialog on a dedicated STA thread.
pub fn choose_folder(title: &str) -> Result<Option<PathBuf>> {
    let title = title.to_owned();
    std::thread::spawn(move || choose_folder_sta(&title))
        .join()
        .map_err(|_| anyhow!("文件夹选择窗口线程异常退出。"))?
}

fn choose_folder_sta(title: &str) -> Result<Option<PathBuf>> {
    let hr = unsafe { CoInitializeEx(ptr::null(), COINIT_APARTMENTTHREADED as u32) };
    let should_uninit = hr == S_OK || hr == S_FALSE;
    if hr != S_OK && hr != S_FALSE && hr != RPC_E_CHANGED_MODE {
        return Err(anyhow!("无法初始化 Windows 文件夹选择组件（{hr:#x}）。"));
    }

    let result = (|| {
        let title_wide: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
        let mut display_name = vec![0u16; 260];
        let bi = BROWSEINFOW {
            hwndOwner: ptr::null_mut(),
            pidlRoot: ptr::null_mut(),
            pszDisplayName: display_name.as_mut_ptr(),
            lpszTitle: title_wide.as_ptr(),
            ulFlags: BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE | BIF_EDITBOX,
            lpfn: None,
            lParam: 0,
            iImage: 0,
        };
        let pidl = unsafe { SHBrowseForFolderW(&bi) };
        if pidl.is_null() {
            return Ok(None);
        }
        let mut path_buf = vec![0u16; 32_768];
        let ok = unsafe { SHGetPathFromIDListW(pidl, path_buf.as_mut_ptr()) };
        unsafe { CoTaskMemFree(pidl.cast()) };
        if ok == 0 {
            return Err(anyhow!("未能读取所选文件夹路径。"));
        }
        let len = path_buf
            .iter()
            .position(|ch| *ch == 0)
            .unwrap_or(path_buf.len());
        Ok(Some(PathBuf::from(OsString::from_wide(&path_buf[..len]))))
    })();

    if should_uninit {
        unsafe { CoUninitialize() };
    }
    result
}

/// Launches a URL or Windows protocol URI in the default browser / shell handler.
pub fn open_url(target: &str) -> Result<()> {
    let verb: Vec<u16> = "open\0".encode_utf16().collect();
    let target_wide: Vec<u16> = target.encode_utf16().chain(Some(0)).collect();
    let ret = unsafe {
        ShellExecuteW(
            ptr::null_mut(),
            verb.as_ptr(),
            target_wide.as_ptr(),
            ptr::null(),
            ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    ensure!(
        (ret as isize) > 32,
        "无法自动打开链接（{target}），请手动复制到浏览器打开。"
    );
    Ok(())
}

/// Runs a Windows installer executable (handling UAC elevation via PowerShell Start-Process) and waits for exit.
pub fn run_installer_and_wait(installer: &Path, args: Option<&str>) -> Result<i32> {
    let mut cmd = std::process::Command::new("powershell.exe");
    cmd.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command"]);
    if let Some(arg_str) = args.filter(|s| !s.trim().is_empty()) {
        cmd.arg("$p = Start-Process -FilePath $env:UNITY_IPA_EXEC -ArgumentList $env:UNITY_IPA_EXEC_ARGS -Wait -PassThru; exit $p.ExitCode");
        cmd.env("UNITY_IPA_EXEC_ARGS", arg_str);
    } else {
        cmd.arg("$p = Start-Process -FilePath $env:UNITY_IPA_EXEC -Wait -PassThru; exit $p.ExitCode");
    }
    cmd.env("UNITY_IPA_EXEC", installer);
    let status = cmd
        .status()
        .with_context(|| format!("无法启动安装程序 {}", installer.display()))?;
    Ok(status.code().unwrap_or(1))
}

/// Launches a desktop application without blocking.
pub fn launch_app(exe: &Path) -> Result<()> {
    let verb: Vec<u16> = "open\0".encode_utf16().collect();
    let file_wide: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
    let ret = unsafe {
        ShellExecuteW(
            ptr::null_mut(),
            verb.as_ptr(),
            file_wide.as_ptr(),
            ptr::null(),
            ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    ensure!(
        (ret as isize) > 32,
        "无法启动程序 {}。",
        exe.display()
    );
    Ok(())
}

/// Opens the Microsoft Store page for Apple Devices (or web fallback).
pub fn open_apple_devices_store() -> Result<()> {
    if open_url(APPLE_DEVICES_STORE_URI).is_ok() {
        return Ok(());
    }
    open_url(APPLE_DEVICES_WEB_URL)
}

/// Checks whether Apple Mobile Device Service / usbmuxd is reachable, guiding the user to install Apple Devices if missing.
pub async fn ensure_apple_usb_ready() -> Result<()> {
    loop {
        if crate::pairing::is_usbmuxd_available().await {
            println!("✅ Apple USB 驱动服务已就绪。");
            return Ok(());
        }
        println!();
        println!("未检测到 Windows 上的 Apple USB 驱动服务（Apple Mobile Device Service）。");
        println!("在 Windows 上给 iPhone 装应用，需要先安装微软商店官方的「Apple Devices（Apple 设备）」。");
        let choice = crate::prompt(
            "按 Enter 自动打开微软商店安装 Apple Devices，输入 s 暂时跳过检测，输入 q 取消",
        )?;
        match choice.to_ascii_lowercase().as_str() {
            "" => {
                if let Err(err) = open_apple_devices_store() {
                    eprintln!("{err:#}");
                    println!("请打开微软商店搜索并安装「Apple Devices」，或访问：{APPLE_DEVICES_WEB_URL}");
                } else {
                    println!("已打开 Apple Devices 商店页面，请点击「获取 / 安装」。");
                }
                let next = crate::prompt("安装完成后请按 Enter 重新检测（输入 s 跳过，q 取消）")?;
                match next.to_ascii_lowercase().as_str() {
                    "s" => return Ok(()),
                    "q" => anyhow::bail!("已取消操作。"),
                    _ => {}
                }
            }
            "s" => return Ok(()),
            "q" => anyhow::bail!("已取消操作。"),
            _ => println!("请输入 Enter、s 或 q。"),
        }
    }
}
