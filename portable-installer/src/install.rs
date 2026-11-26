use anyhow::{Context, Result, ensure};
use idevice::{
    IdeviceError, IdeviceService,
    afc::{AfcClient, errors::AfcError, opcode::AfcFopenMode},
    installation_proxy::InstallationProxyClient,
    provider::IdeviceProvider,
    usbmuxd::errors::UsbmuxdError,
};
use isideload_vfs::fs;
use std::{
    fs::File,
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::Path,
    time::Duration,
};
use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

pub async fn signed_ipa(udid: &str, app: &Path) -> Result<()> {
    // Upload one archive so iOS installs the signed bundle from a single package.
    let mut archive = tempfile::tempfile().context("无法创建临时 IPA")?;
    package_app(&mut archive, app).context("无法打包签名后的 IPA")?;
    let remote_path = format!("PublicStaging/unity-ipa-{}.ipa", uuid::Uuid::new_v4());
    let (mut afc, mut proxy) = loop {
        // The USB multiplexer can assign a different device ID during Apple login.
        match super::pairing::reconnect(udid).await {
            Ok((provider, _)) => match stage_ipa(&provider, &mut archive, &remote_path).await {
                Ok(staged) => break staged,
                Err(error) if error.downcast_ref().is_some_and(can_reconnect) => {
                    eprintln!("{error:#}");
                }
                Err(error) => return Err(error),
            },
            Err(error) => eprintln!("{error:#}"),
        }
        ensure!(
            super::prompt("已保留签名后的 IPA。请重新连接并解锁同一台 iPhone，按 Enter 重新上传。输入其他字符取消")?
                .is_empty(),
            "已取消安装。"
        );
    };
    println!("IPA 上传完成，正在安装到 iPhone...");
    let installed = proxy.install(&remote_path, None).await;
    remove_staged(&mut afc, &remote_path).await;
    installed.map_err(|error| {
        if can_reconnect(&error) {
            anyhow::Error::new(error)
                .context("安装过程中 USB 连接中断。请先在 iPhone 上检查应用是否已安装")
        } else {
            let report =
                rootcause::Report::new(isideload::SideloadError::IdeviceError(error)).into();
            super::apple_error("iPhone USB 安装", &report)
        }
    })
}

async fn stage_ipa(
    provider: &impl IdeviceProvider,
    archive: &mut File,
    remote_path: &str,
) -> Result<(AfcClient, InstallationProxyClient)> {
    archive.seek(SeekFrom::Start(0))?;
    let mut afc = AfcClient::connect(provider)
        .await
        .context("无法连接 iPhone 文件服务")?;
    println!("正在通过 USB 上传签名后的 IPA...");
    let result = async {
        let mut remote = afc
            .open(remote_path, AfcFopenMode::WrOnly)
            .await
            .context("无法在 iPhone 创建 IPA 临时文件")?;
        let mut buffer = vec![0; 64 * 1024];
        let uploaded: Result<()> = async {
            loop {
                let count = archive.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                remote
                    .write_entire(&buffer[..count])
                    .await
                    .context("USB 上传 IPA 失败")?;
            }
            Ok(())
        }
        .await;
        let closed = remote.close().await;
        uploaded?;
        closed.context("iPhone 未确认 IPA 写入完成")?;

        InstallationProxyClient::connect(provider)
            .await
            .context("无法连接 iPhone 安装服务")
    }
    .await;
    if result.is_err() {
        remove_staged(&mut afc, remote_path).await;
    }
    Ok((afc, result?))
}

async fn remove_staged(afc: &mut AfcClient, remote_path: &str) {
    let _ = tokio::time::timeout(Duration::from_secs(3), afc.remove(remote_path)).await;
}

fn can_reconnect(error: &IdeviceError) -> bool {
    matches!(
        error,
        IdeviceError::Usbmuxd(UsbmuxdError::BadDevice | UsbmuxdError::ConnectionRefused)
            | IdeviceError::DeviceNotFound
            | IdeviceError::NoEstablishedConnection
            | IdeviceError::SessionInactive
            | IdeviceError::InvalidHostID
            | IdeviceError::DeviceLocked
            | IdeviceError::PasswordProtected
            | IdeviceError::Socket(_)
            | IdeviceError::Timeout
            | IdeviceError::Afc(
                AfcError::ServiceNotConnected | AfcError::OpTimeout | AfcError::MuxError
            )
    )
}

fn package_app<W: Write + Seek>(writer: W, app: &Path) -> Result<()> {
    let app_name = app
        .file_name()
        .and_then(|name| name.to_str())
        .context("签名后的 App 名称无效")?;
    let mut zip = ZipWriter::new(writer);
    add_directory(&mut zip, app, &format!("Payload/{app_name}"))?;
    zip.finish()?;
    Ok(())
}

fn add_directory<W: Write + Seek>(zip: &mut ZipWriter<W>, folder: &Path, path: &str) -> Result<()> {
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    zip.add_directory(format!("{path}/"), options.unix_permissions(0o755))?;
    for entry in fs::read_dir(folder)? {
        let entry = entry?;
        let source = entry.path();
        let name = source
            .file_name()
            .and_then(|name| name.to_str())
            .context("App 中的文件名称无效")?;
        let destination = format!("{path}/{name}");
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            add_directory(zip, &source, &destination)?;
        } else {
            let mut file = BufReader::new(fs::File::open(&source)?);
            let permissions = match file.fill_buf()? {
                [0xcf, 0xfa, 0xed, 0xfe, ..]
                | [0xce, 0xfa, 0xed, 0xfe, ..]
                | [0xca, 0xfe, 0xba, 0xbe | 0xbf, ..] => 0o755,
                _ => 0o644,
            };
            zip.start_file(
                destination,
                options
                    .unix_permissions(permissions)
                    .large_file(metadata.len() >= u32::MAX.into()),
            )?;
            io::copy(&mut file, zip)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use zip::ZipArchive;

    #[test]
    fn reconnects_if_usbmux_loses_the_device() {
        assert!(can_reconnect(&UsbmuxdError::BadDevice.into()));
        assert!(!can_reconnect(&AfcError::NoSpaceLeft.into()));
    }

    #[test]
    fn keeps_the_nested_framework_bytes_and_executable_mode() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let app = temporary.path().join("Test App.app");
        let framework = app.join("Frameworks/Test.framework");
        std::fs::create_dir_all(&framework)?;
        let executable = b"\xcf\xfa\xed\xfetest framework signature bytes";
        std::fs::write(framework.join("Test"), executable)?;
        std::fs::write(app.join("embedded.mobileprovision"), b"test profile")?;

        let mut buffer = Cursor::new(Vec::new());
        package_app(&mut buffer, &app)?;
        let mut ipa = ZipArchive::new(buffer)?;
        let mut binary = ipa.by_name("Payload/Test App.app/Frameworks/Test.framework/Test")?;
        let mut contents = Vec::new();
        binary.read_to_end(&mut contents)?;
        assert_eq!(contents, executable);
        assert_eq!(binary.unix_mode().unwrap() & 0o777, 0o755);
        Ok(())
    }
}
