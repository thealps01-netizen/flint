//! "Launch at startup": a value in the user's
//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` key that starts
//! Flint hidden in the tray when the user signs in.

/// The key Windows reads at sign-in.
pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE: &str = "Flint";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// Turns the start-at-sign-in entry under `HKCU\<key>` on (pointing at
/// `exe`) or off. Turning off an entry that is not there is not an error.
#[cfg(windows)]
pub fn set(key: &str, on: bool, exe: &std::path::Path) -> std::io::Result<()> {
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        HKEY_CURRENT_USER, REG_SZ, RegDeleteKeyValueW, RegSetKeyValueW,
    };
    let (k, v) = (wide(key), wide(VALUE));
    let rc = if on {
        let data = wide(&format!("\"{}\"", exe.display()));
        // SAFETY: key, value and data are NUL-terminated UTF-16 buffers that
        // outlive the call; the byte count covers data including its NUL.
        unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                k.as_ptr(),
                v.as_ptr(),
                REG_SZ,
                data.as_ptr().cast(),
                (data.len() * 2) as u32,
            )
        }
    } else {
        // SAFETY: key and value are NUL-terminated UTF-16 buffers.
        match unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), v.as_ptr()) } {
            ERROR_FILE_NOT_FOUND => ERROR_SUCCESS,
            rc => rc,
        }
    };
    if rc == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(std::io::Error::from_raw_os_error(rc as i32))
    }
}

/// The program the entry under `HKCU\<key>` starts, if there is one.
#[cfg(all(test, windows))]
pub fn get(key: &str) -> Option<String> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_SZ, RegGetValueW};
    let (k, v) = (wide(key), wide(VALUE));
    let mut buf = vec![0u16; 1024];
    let mut len = (buf.len() * 2) as u32;
    // SAFETY: key and value are NUL-terminated; buf has len writable bytes.
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            k.as_ptr(),
            v.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut len,
        )
    };
    (rc == ERROR_SUCCESS).then(|| {
        buf.truncate((len as usize / 2).saturating_sub(1));
        String::from_utf16_lossy(&buf)
    })
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn startup_entry_turns_on_and_off() {
        // a key of the test's own, never the real Run key
        let key = format!(r"Software\Flint-test-autostart-{}", std::process::id());
        let exe = std::path::Path::new(r"C:\Program Files\Flint\flint.exe");
        assert_eq!(get(&key), None);
        set(&key, true, exe).unwrap();
        assert_eq!(
            get(&key).as_deref(),
            Some(r#""C:\Program Files\Flint\flint.exe""#)
        );
        set(&key, false, exe).unwrap();
        assert_eq!(get(&key), None);
        // off again is fine
        set(&key, false, exe).unwrap();
        // remove the test key itself
        let k = wide(&key);
        // SAFETY: k is a NUL-terminated UTF-16 buffer.
        unsafe {
            windows_sys::Win32::System::Registry::RegDeleteKeyW(
                windows_sys::Win32::System::Registry::HKEY_CURRENT_USER,
                k.as_ptr(),
            )
        };
    }
}
