// SPDX-License-Identifier: GPL-3.0-or-later
//! Read the installed ASIO drivers from the Windows registry.
//!
//! ASIO drivers register one subkey each under `HKLM\SOFTWARE\ASIO`, named after the
//! driver, carrying a `Description` (human readable name) and a `CLSID` (the COM object
//! the driver is instantiated from). The panel only lists them, so this module never
//! touches COM.

use serde::Serialize;

/// The slice of advapi32 this crate needs, declared straight from the local Windows SDK
/// headers (`um/winreg.h`, `um/winnt.h`, `shared/winerror.h`) so every signature and
/// constant below is checkable against the SDK that ships with the toolchain.
#[link(name = "advapi32")]
extern "system" {
    fn RegOpenKeyExW(hkey: Handle, sub_key: *const u16, options: u32, sam: REGSAM, out: *mut Handle) -> LStatus;
    fn RegEnumKeyExW(hkey: Handle, index: u32, name: *mut u16, name_len: *mut u32, reserved: *mut u32, class: *mut u16, class_len: *mut u32, last_write: *mut core::ffi::c_void) -> LStatus;
    fn RegQueryValueExW(hkey: Handle, value_name: *const u16, reserved: *mut u32, value_type: *mut u32, data: *mut u8, data_len: *mut u32) -> LStatus;
    fn RegCloseKey(hkey: Handle) -> LStatus;
}

/// A Win32 `HKEY`: a pointer-sized handle.
type Handle = isize;
/// A Win32 `LSTATUS`: a signed 32-bit error code.
type LStatus = i32;
/// A Win32 `REGSAM`.
type REGSAM = u32;

/// `HKEY_LOCAL_MACHINE` = `((HKEY)(ULONG_PTR)((LONG)0x80000002))`; the cast must go via
/// `i32` because the header sign-extends the 32-bit literal.
const HKEY_LOCAL_MACHINE: Handle = 0x8000_0002u32 as i32 as Handle;
/// `KEY_READ` = `(READ_CONTROL | KEY_QUERY_VALUE | KEY_ENUMERATE_SUB_KEYS | KEY_NOTIFY)`
/// `& ~SYNCHRONIZE` = `(0x20000 | 0x1 | 0x8 | 0x10) & !0x100000`.
const KEY_READ: REGSAM = 0x0002_0019;
/// `REG_SZ`: a NUL-terminated UTF-16 string.
const REG_SZ: u32 = 1;
/// `ERROR_SUCCESS`, `ERROR_MORE_DATA` and `ERROR_NO_MORE_ITEMS` from `shared/winerror.h`.
const ERROR_SUCCESS: LStatus = 0;
const ERROR_MORE_DATA: LStatus = 234;
const ERROR_NO_MORE_ITEMS: LStatus = 259;

/// One ASIO driver registered under `HKLM\SOFTWARE\ASIO`.
/// Serialized to the frontend over IPC, so it carries `Serialize` (design.md 9.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AsioDevice {
    /// The registry subkey name under `HKLM\SOFTWARE\ASIO`.
    pub key_name: String,
    /// The `Description` value, or `key_name` when that value is missing or empty.
    pub description: String,
    /// The `CLSID` value (e.g. `{232685C6-...}`), or an empty string when missing.
    pub clsid: String,
}

/// Enumerate the immediate subkeys of `HKLM\SOFTWARE\ASIO` as ASIO devices, sorted by
/// `key_name`. Returns an empty vector (never panics) when the key is absent or cannot be
/// opened, e.g. when no ASIO driver is installed.
pub fn list_asio_devices() -> Vec<AsioDevice> {
    let mut devices = Vec::new();
    let path: Vec<u16> = "SOFTWARE\\ASIO\0".encode_utf16().collect();
    let mut root: Handle = 0;
    // SAFETY: `path` is a live NUL-terminated UTF-16 buffer and `root` is a valid out-pointer.
    let status = unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, path.as_ptr(), 0, KEY_READ, &mut root) };
    if status != ERROR_SUCCESS {
        return devices;
    }

    let mut index = 0u32;
    loop {
        let mut name = vec![0u16; 512];
        let mut name_len = name.len() as u32;
        // SAFETY: `root` is open; `name` is a live buffer of `name_len` u16s; the class and
        // last-write out-parameters are optional and passed as null.
        let status = unsafe {
            RegEnumKeyExW(root, index, name.as_mut_ptr(), &mut name_len, core::ptr::null_mut(),
                core::ptr::null_mut(), core::ptr::null_mut(), core::ptr::null_mut())
        };
        if status == ERROR_NO_MORE_ITEMS {
            break; // end of the subkey list
        }
        if status != ERROR_SUCCESS {
            break; // unexpected failure: treat it as "done" rather than loop forever
        }
        index += 1;
        let key_name = String::from_utf16_lossy(&name[..name_len as usize]);
        if key_name.is_empty() {
            continue;
        }

        let sub_path: Vec<u16> = format!("SOFTWARE\\ASIO\\{key_name}").encode_utf16().chain(std::iter::once(0)).collect();
        let mut sub: Handle = 0;
        // SAFETY: `sub_path` is a live NUL-terminated UTF-16 buffer; `sub` is a valid out-pointer.
        let status = unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, sub_path.as_ptr(), 0, KEY_READ, &mut sub) };
        if status != ERROR_SUCCESS {
            continue;
        }
        let description = read_reg_sz(sub, "Description").unwrap_or_default();
        let clsid = read_reg_sz(sub, "CLSID").unwrap_or_default();
        // SAFETY: `sub` is an open key handle that we own and do not use afterwards.
        unsafe { RegCloseKey(sub) };

        devices.push(AsioDevice {
            description: if description.is_empty() { key_name.clone() } else { description },
            key_name,
            clsid,
        });
    }

    // SAFETY: `root` is the open handle from above, owned here and unused afterwards.
    unsafe { RegCloseKey(root) };
    devices.sort_by(|a, b| a.key_name.cmp(&b.key_name));
    devices
}

/// Read a `REG_SZ` value, or `None` when it is missing, has another type, or cannot be
/// read. Retries once if the value grows between the size probe and the actual read.
fn read_reg_sz(hkey: Handle, value: &str) -> Option<String> {
    let name: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
    for _ in 0..2 {
        let mut value_type = 0u32;
        let mut bytes = 0u32;
        // SAFETY: `hkey` is open; `name` is a live NUL-terminated UTF-16 buffer; the null
        // data pointer asks Win32 for the required size only.
        let status = unsafe {
            RegQueryValueExW(hkey, name.as_ptr(), core::ptr::null_mut(), &mut value_type,
                core::ptr::null_mut(), &mut bytes)
        };
        if status != ERROR_SUCCESS || value_type != REG_SZ || bytes == 0 {
            return None;
        }

        // One extra u16 so an odd (malformed) byte length cannot under-allocate.
        let mut buf = vec![0u16; bytes as usize / 2 + 1];
        let mut buf_bytes = (buf.len() * 2) as u32;
        // SAFETY: `hkey` is open; `name` outlives the call; `buf` is a live writable buffer
        // of `buf_bytes` bytes that stays alive for the whole call.
        let status = unsafe {
            RegQueryValueExW(hkey, name.as_ptr(), core::ptr::null_mut(), &mut value_type,
                buf.as_mut_ptr() as *mut u8, &mut buf_bytes)
        };
        if status == ERROR_MORE_DATA {
            continue; // value grew; the probe reports the new size, so try once more
        }
        if status != ERROR_SUCCESS {
            return None;
        }
        let text = String::from_utf16_lossy(&buf);
        return Some(text.trim_end_matches('\0').to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Safe on a CI machine, which normally does have keys under `HKLM\SOFTWARE\ASIO`:
    /// assert only shapes, ordering and the absence of a panic, never a count.
    #[test]
    fn listing_does_not_panic_and_shapes_are_valid() {
        let devices = list_asio_devices();
        for device in &devices {
            assert!(!device.key_name.is_empty(), "key_name must not be empty");
            assert!(!device.description.is_empty(), "description must fall back to key_name");
            assert!(
                device.clsid.is_empty()
                    || (device.clsid.starts_with('{') && device.clsid.ends_with('}')),
                "clsid must be empty or GUID-shaped: {:?}",
                device.clsid
            );
        }
        assert!(
            devices.windows(2).all(|w| w[0].key_name <= w[1].key_name),
            "devices must be sorted by key_name"
        );
    }
}
