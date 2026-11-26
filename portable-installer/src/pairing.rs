use anyhow::{Context, Result, anyhow, bail, ensure};
use idevice::{
    Idevice, IdeviceError, IdeviceService,
    amfi::AmfiClient,
    lockdown::LockdownClient,
    pairing_file::PairingFile,
    provider::{IdeviceProvider, UsbmuxdProvider},
    usbmuxd::{Connection, UsbmuxdAddr, UsbmuxdDevice},
};
use isideload::util::device::IdeviceInfo;
use std::{
    env, fmt, fs,
    future::Future,
    io::{self, Write},
    path::PathBuf,
    pin::Pin,
    ptr,
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::LocalFree,
    Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    },
};
use zeroize::{Zeroize, Zeroizing};

pub struct UsbSession {
    usb: UsbmuxdProvider,
    record: Option<PairingFile>,
}

impl fmt::Debug for UsbSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("USB session")
    }
}

impl IdeviceProvider for UsbSession {
    fn connect(
        &self,
        port: u16,
    ) -> Pin<Box<dyn Future<Output = Result<Idevice, IdeviceError>> + Send>> {
        self.usb.connect(port)
    }

    fn label(&self) -> &str {
        self.usb.label()
    }

    fn get_pairing_file(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<PairingFile, IdeviceError>> + Send>> {
        match &self.record {
            Some(record) => Box::pin(std::future::ready(Ok(record.clone()))),
            None => self.usb.get_pairing_file(),
        }
    }
}

pub async fn is_usbmuxd_available() -> bool {
    let address = UsbmuxdAddr::default();
    tokio::time::timeout(Duration::from_secs(3), address.connect(0))
        .await
        .is_ok_and(|res| res.is_ok())
}

pub async fn devices() -> Result<(UsbmuxdAddr, Vec<UsbmuxdDevice>)> {
    let address = UsbmuxdAddr::default();
    let mut mux = address.connect(0).await.map_err(|_| {
        anyhow!("无法连接 Apple Mobile Device Service。请检查 Windows 上的 Apple USB 驱动。")
    })?;
    let devices: Vec<_> = mux
        .get_devices()
        .await
        .map_err(|_| anyhow!("无法读取 USB 设备列表。"))?
        .into_iter()
        .filter(|device| device.connection_type == Connection::Usb)
        .collect();
    ensure!(
        !devices.is_empty(),
        "未找到 USB iPhone。请连接数据线并解锁手机。"
    );
    Ok((address, devices))
}

pub async fn check(
    address: &UsbmuxdAddr,
    device: &UsbmuxdDevice,
) -> Option<(UsbSession, IdeviceInfo)> {
    let mut session = UsbSession {
        usb: device.to_provider(address.clone(), super::APP_NAME),
        record: None,
    };
    if let Ok(info) = IdeviceInfo::from_device(&session).await {
        return Some((session, info));
    }
    session.record = Some(load_record(device)?);
    let info = IdeviceInfo::from_device(&session).await.ok()?;
    Some((session, info))
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DeveloperMode {
    Enabled,
    Disabled,
    Unknown,
}

impl fmt::Display for DeveloperMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Enabled => "开发者模式已开启",
            Self::Disabled => "开发者模式未开启",
            Self::Unknown => "开发者模式状态未能读取",
        })
    }
}

pub async fn developer_mode(provider: &UsbSession) -> DeveloperMode {
    let status = async {
        let mut connection = LockdownClient::connect(provider).await.ok()?;
        let record = provider.get_pairing_file().await.ok()?;
        connection.start_session(&record).await.ok()?;
        connection
            .get_value(
                Some("DeveloperModeStatus"),
                Some("com.apple.security.mac.amfi"),
            )
            .await
            .ok()?
            .as_boolean()
    }
    .await;
    match status {
        Some(true) => DeveloperMode::Enabled,
        Some(false) => DeveloperMode::Disabled,
        None => DeveloperMode::Unknown,
    }
}

pub async fn show_developer_mode(provider: &UsbSession) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut client = AmfiClient::connect(provider).await?;
        client.reveal_developer_mode_option_in_ui().await
    })
    .await
    .context("等待 iPhone 响应 Developer Mode 显示请求超时")?
    .context("无法请求 iPhone 显示 Developer Mode 入口")
}

pub async fn name(address: &UsbmuxdAddr, device: &UsbmuxdDevice) -> Option<String> {
    let usb = device.to_provider(address.clone(), super::APP_NAME);
    let mut connection = LockdownClient::connect(&usb).await.ok()?;
    let value = connection.get_value(Some("DeviceName"), None).await.ok()?;
    value.as_string().map(str::to_owned)
}

pub async fn connect(
    address: &UsbmuxdAddr,
    device: &UsbmuxdDevice,
) -> Result<(UsbSession, IdeviceInfo)> {
    if let Some(ready) = check(address, device).await {
        return Ok(ready);
    }

    let usb = device.to_provider(address.clone(), super::APP_NAME);
    let mut lockdown = LockdownClient::connect(&usb)
        .await
        .map_err(|_| anyhow!("无法与 iPhone 通信。请检查数据线和手机是否解锁。"))?;
    let mut mux = address
        .connect(0)
        .await
        .context("无法打开 Apple USB 服务")?;
    let system_buid = mux
        .get_buid()
        .await
        .context("Apple USB 服务未返回电脑标识")?;
    let host_id = uuid::Uuid::new_v4().to_string().to_ascii_uppercase();

    println!("正在请求重新配对。请保持 iPhone 解锁，如出现「信任此电脑」请确认。");
    let mut record = match tokio::time::timeout(
        Duration::from_secs(90),
        lockdown.pair(host_id, system_buid, Some(super::APP_NAME)),
    )
    .await
    {
        Ok(Ok(record)) => record,
        Ok(Err(IdeviceError::UserDeniedPairing)) => bail!("iPhone 上的配对请求被拒绝。"),
        Ok(Err(IdeviceError::PasswordProtected)) => bail!("请解锁 iPhone 后重试。"),
        Ok(Err(error)) => bail!("iPhone 拒绝重新配对：{error}"),
        Err(_) => bail!("等待 iPhone 确认信任超时。"),
    };
    drop(lockdown);
    record.udid = Some(device.udid.clone());
    let session = UsbSession {
        usb,
        record: Some(record),
    };
    let info = IdeviceInfo::from_device(&session)
        .await
        .map_err(|_| anyhow!("iPhone 已接受配对，但新的 USB 会话未能建立。"))?;
    if let Some(record) = &session.record {
        if save_record(device, record).is_err() {
            eprintln!("这次配对可继续使用。Windows 未能保存应用的配对记录。");
        }
    }
    println!("iPhone 配对成功。");
    Ok((session, info))
}

pub async fn reconnect(udid: &str) -> Result<(UsbSession, IdeviceInfo)> {
    let (address, devices) = devices().await?;
    let device = devices
        .iter()
        .find(|device| device.udid == udid)
        .context("请重新连接刚才选中的 iPhone")?;
    connect(&address, device).await
}

fn record_path(device: &UsbmuxdDevice) -> Result<PathBuf> {
    let id = &device.udid;
    ensure!(
        !id.is_empty()
            && id
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() || byte == b'-'),
        "USB 设备标识无效。"
    );
    Ok(
        PathBuf::from(env::var_os("LOCALAPPDATA").context("无法定位本机应用数据目录")?)
            .join("UnityIpaWorkflow")
            .join("Pairing")
            .join(format!("{id}.dpapi")),
    )
}

fn load_record(device: &UsbmuxdDevice) -> Option<PairingFile> {
    let encrypted = fs::read(record_path(device).ok()?).ok()?;
    let bytes = crypt(&encrypted, false).ok()?;
    let record = PairingFile::from_bytes(&bytes).ok()?;
    (record.udid.as_ref() == Some(&device.udid)).then_some(record)
}

fn save_record(device: &UsbmuxdDevice, record: &PairingFile) -> Result<()> {
    let path = record_path(device)?;
    let directory = path.parent().context("配对记录目录不存在")?;
    fs::create_dir_all(directory)?;
    let bytes = Zeroizing::new(record.clone().serialize()?);
    let encrypted = crypt(&bytes, true)?;
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    file.write_all(&encrypted)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    Ok(())
}

pub(crate) fn crypt(input: &[u8], protect: bool) -> Result<Zeroizing<Vec<u8>>> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: input.len().try_into()?,
        pbData: input.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    // DPAPI allocates the result with LocalAlloc. The input is read-only for both calls.
    let ok = unsafe {
        if protect {
            CryptProtectData(
                &input,
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptUnprotectData(
                &input,
                ptr::null_mut(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        }
    };
    if ok == 0 {
        return Err(io::Error::last_os_error()).context("无法使用 Windows DPAPI");
    }
    if output.cbData == 0 {
        unsafe { LocalFree(output.pbData.cast()) };
        return Ok(Zeroizing::new(Vec::new()));
    }
    // On success DPAPI returns a valid buffer with cbData bytes.
    let data = unsafe { std::slice::from_raw_parts_mut(output.pbData, output.cbData as usize) };
    let bytes = Zeroizing::new(data.to_vec());
    data.zeroize();
    unsafe { LocalFree(output.pbData.cast()) };
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::crypt;

    #[test]
    fn dpapi_round_trip() {
        let record = b"synthetic pairing data";
        let mut encrypted = crypt(record, true).unwrap();
        assert_ne!(encrypted.as_slice(), record);
        assert_eq!(crypt(&encrypted, false).unwrap().as_slice(), record);
        let middle = encrypted.len() / 2;
        encrypted[middle] ^= 1;
        assert!(crypt(&encrypted, false).is_err());
    }
}
