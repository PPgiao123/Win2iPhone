use std::io::{self, Write};
use windows_sys::Win32::{
    Foundation::HANDLE,
    System::Console::{
        ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
        ENABLE_VIRTUAL_TERMINAL_INPUT, GetConsoleMode, GetStdHandle, INPUT_RECORD, KEY_EVENT,
        ReadConsoleInputW, STD_INPUT_HANDLE, SetConsoleMode,
    },
};
use zeroize::Zeroizing;

struct ConsoleInput {
    handle: HANDLE,
    original_mode: u32,
}

impl ConsoleInput {
    fn masked() -> io::Result<Self> {
        // The standard input handle is borrowed from Windows and stays open.
        let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        let mut original_mode = 0;
        if unsafe { GetConsoleMode(handle, &mut original_mode) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let mode = original_mode
            & !(ENABLE_ECHO_INPUT
                | ENABLE_LINE_INPUT
                | ENABLE_PROCESSED_INPUT
                | ENABLE_VIRTUAL_TERMINAL_INPUT);
        if unsafe { SetConsoleMode(handle, mode) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            handle,
            original_mode,
        })
    }

    fn key(&self) -> io::Result<(u16, u16)> {
        loop {
            let mut record = INPUT_RECORD::default();
            let mut count = 0;
            if unsafe { ReadConsoleInputW(self.handle, &mut record, 1, &mut count) } == 0 {
                return Err(io::Error::last_os_error());
            }
            if count != 0 && u32::from(record.EventType) == KEY_EVENT {
                // EventType identifies the active union member.
                let key = unsafe { record.Event.KeyEvent };
                if key.bKeyDown != 0 {
                    return Ok((unsafe { key.uChar.UnicodeChar }, key.wRepeatCount));
                }
            }
        }
    }
}

impl Drop for ConsoleInput {
    fn drop(&mut self) {
        unsafe { SetConsoleMode(self.handle, self.original_mode) };
    }
}

pub fn prompt() -> io::Result<Zeroizing<String>> {
    let input = ConsoleInput::masked()?;
    super::show_prompt("Apple 账户密码（输入显示为 *）")?;
    let mut output = io::stdout().lock();
    let result = read_masked(|| input.key(), &mut output);
    drop(input);
    writeln!(output)?;
    result
}

fn read_masked(
    mut read_key: impl FnMut() -> io::Result<(u16, u16)>,
    output: &mut impl Write,
) -> io::Result<Zeroizing<String>> {
    let mut password = Zeroizing::new(String::new());
    let mut leading_surrogate = Zeroizing::new(0u16);
    loop {
        let (unit, repeat) = read_key()?;
        for _ in 0..repeat {
            match unit {
                0x0A | 0x0D => return Ok(password),
                0x03 | 0x1B => {
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "已取消密码输入"));
                }
                0x08 => {
                    *leading_surrogate = 0;
                    if password.pop().is_some() {
                        output.write_all(b"\x08 \x08")?;
                    }
                }
                0xD800..=0xDBFF => *leading_surrogate = unit,
                _ => {
                    let rune = if (0xDC00..=0xDFFF).contains(&unit) {
                        char::decode_utf16([*leading_surrogate, unit])
                            .next()
                            .and_then(|c| c.ok())
                            .filter(|_| *leading_surrogate != 0)
                    } else {
                        char::from_u32(u32::from(unit))
                    };
                    *leading_surrogate = 0;
                    if let Some(rune) = rune.filter(|c| !c.is_control()) {
                        password.push(rune);
                        output.write_all(b"*")?;
                    }
                }
            }
        }
        output.flush()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn echoes_stars_and_backspaces_without_changing_the_remaining_password() {
        let mut keys = "甲🧪\u{0008}z\r".encode_utf16();
        let mut output = Vec::new();
        let password = read_masked(|| Ok((keys.next().unwrap(), 1)), &mut output).unwrap();
        assert_eq!(&*password, "甲z");
        assert_eq!(output, b"**\x08 \x08*");
    }

    #[test]
    fn ctrl_c_cancels_masked_input() {
        let mut keys = [u16::from(b'x'), 0x03].into_iter();
        let mut output = Vec::new();
        let error = read_masked(|| Ok((keys.next().unwrap(), 1)), &mut output).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(output, b"*");
    }
}
