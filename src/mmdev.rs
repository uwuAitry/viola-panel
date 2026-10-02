// SPDX-License-Identifier: GPL-3.0-or-later
//! Friendly name of the Windows **default audio render endpoint**, read-only, so the
//! panel can show what the system is currently playing to and tell the user to change
//! it in Windows sound settings. Nothing here mutates system state: the property store
//! is opened `STGM_READ` and only `GetValue` is ever called.
//!
//! Hand-written COM (no `windows` crate). Every layout, constant and vtable slot is
//! taken from the local SDK headers: `um/mmdeviceapi.h`, `um/propsys.h`, `um/propidl.h`,
//! `um/combaseapi.h`, `um/functiondiscoverykeys_devpkey.h`, `shared/wtypes.h`,
//! `shared/WTypesbase.h`, `um/coml2api.h`, `um/objbase.h`, `shared/winerror.h`.
//! Windows COM methods are `__stdcall`, i.e. `extern "system"` on every slot.

use core::ffi::c_void;
use std::ptr::null_mut;

/// `HRESULT` / Win32 `long` — 32-bit under LLP64.
type HResult = i32;

/// `S_OK` (winerror.h:31208).
const S_OK: HResult = 0;
/// `S_FALSE` (winerror.h:31209) — `CoInitializeEx` succeeding on an already-initialised
/// apartment we do *not* own.
const S_FALSE: HResult = 1;
/// `RPC_E_CHANGED_MODE` (winerror.h:35729): this thread already holds a COM apartment in
/// the other threading model — usable, but we must not pair it with `CoUninitialize`.
const RPC_E_CHANGED_MODE: HResult = 0x8001_0106u32 as i32;
/// `COINIT_APARTMENTTHREADED` (objbase.h:33).
const COINIT_APARTMENTTHREADED: u32 = 0x2;
/// `CLSCTX_INPROC_SERVER` (WTypesbase.h:369).
const CLSCTX_INPROC_SERVER: u32 = 0x1;
/// `STGM_READ` (coml2api.h:42). Read-only access; the store blocks every write.
const STGM_READ: u32 = 0;
/// `VT_LPWSTR` (wtypes.h:865) — a `LPWSTR` is the active PROPVARIANT arm.
const VT_LPWSTR: u16 = 31;
/// `E_NOTFOUND` = `HRESULT_FROM_WIN32(ERROR_NOT_FOUND)` = `HRESULT_FROM_WIN32(1168)`
/// (mmdeviceapi.h:147, winerror.h:7563): no such endpoint exists.
const E_NOTFOUND: HResult = 0x8007_0490u32 as i32;
/// `eRender` (mmdeviceapi.h:196) — the output side.
const E_RENDER: i32 = 0;
/// `eConsole` (mmdeviceapi.h:205) — the role the panel reports.
const E_CONSOLE: i32 = 0;
/// `PKEY_Device_FriendlyName` pid (functiondiscoverykeys_devpkey.h:62).
const PKEY_DEVICE_FRIENDLY_NAME_PID: u32 = 14;
/// Upper bound on a friendly-name length, so a malformed or unterminated pointer errors
/// out instead of walking off into unmapped memory.
const MAX_NAME_UNITS: usize = 32768;

/// `GUID` (`shared/guiddef.h`), mirrored locally so no COM binding crate is needed.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

/// `MIDL_INTERFACE("BCDE0395-E52F-467C-8E3D-C4579291692E")` on the
/// `MMDeviceEnumerator` coclass (mmdeviceapi.idl:770, mmdeviceapi.h:1540).
const CLSID_MMDEVICE_ENUMERATOR: Guid = Guid {
    data1: 0xBCDE_0395,
    data2: 0xE52F,
    data3: 0x467C,
    data4: [0x8E, 0x3D, 0xC4, 0x57, 0x92, 0x91, 0x69, 0x2E],
};

/// `MIDL_INTERFACE("A95664D2-9614-4F35-A746-DE8DB63617E6")` (mmdeviceapi.h:756).
const IID_IMMDEVICE_ENUMERATOR: Guid = Guid {
    data1: 0xA956_64D2,
    data2: 0x9614,
    data3: 0x4F35,
    data4: [0xA7, 0x46, 0xDE, 0x8D, 0xB6, 0x36, 0x17, 0xE6],
};

/// `MIDL_INTERFACE("886d8eeb-8cf2-4446-8d02-cdba1dbdcf99")` (propsys.h:516). This module
/// never calls `QueryInterface` — it asks the device directly for its store — so the IID
/// is unused in production code. Kept because it is one of the four identities this
/// module had to verify against the headers, and pinned by a test.
#[allow(dead_code)]
const IID_IPROPERTYSTORE: Guid = Guid {
    data1: 0x886D_8EEB,
    data2: 0x8CF2,
    data3: 0x4446,
    data4: [0x8D, 0x02, 0xCD, 0xBA, 0x1D, 0xBD, 0xCF, 0x99],
};

/// `PKEY_Device_FriendlyName`'s fmtid, from `DEFINE_PROPERTYKEY` at
/// functiondiscoverykeys_devpkey.h:62 (the same fmtid as every `PKEY_Device_*`).
const FMTID_DEVICE: Guid = Guid {
    data1: 0xA45C_254E,
    data2: 0xDF1C,
    data3: 0x4EFD,
    data4: [0x80, 0x20, 0x67, 0xD1, 0x46, 0xA8, 0x50, 0xE0],
};

/// `PROPERTYKEY` (`shared/wtypes.h:892`) — a fmtid plus a property id.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct PropertyKey {
    fmtid: Guid,
    pid: u32,
}

/// The `PROPVARIANT` union arm we read (`propidl.h:311-389`). Its largest member is the
/// 16-byte `DECIMAL decVal` (`propidl.h:390`), and one arm is a pointer, so the union is
/// 16 bytes with 8-byte alignment — the same layout the C compiler produces.
#[repr(C)]
#[derive(Clone, Copy)]
union PropVariantValue {
    pwsz_val: *const u16,
    raw: [u8; 16],
}

/// `PROPVARIANT` (`propidl.h:302-392`) on x86_64. The header declares
/// `VARTYPE vt` (a `WORD`) followed by `PROPVAR_PAD1/2/3`, each a `WORD` in the standard
/// C layout (`propidl.h:290-292`, `307-310`) — that is 8 bytes before the union, **not**
/// 2. No `#pragma pack` applies anywhere in `propidl.h`, so the natural x86_64 layout
/// wins: `vt` at 0, the union at 8, total 24.
#[repr(C)]
#[allow(dead_code)] // only `vt` and `value` are read; the pads are pure layout
struct PropVariant {
    vt: u16,
    w_reserved1: u16,
    w_reserved2: u16,
    w_reserved3: u16,
    value: PropVariantValue,
}

impl PropVariant {
    /// The state `PropVariantInit` leaves behind: every byte zero, `vt == VT_EMPTY`.
    fn empty() -> Self {
        PropVariant {
            vt: 0,
            w_reserved1: 0,
            w_reserved2: 0,
            w_reserved3: 0,
            value: PropVariantValue { raw: [0; 16] },
        }
    }
}

/// IUnknown's three slots (`combaseapi.h` / `unknwn.h`), shared by every interface's
/// vtable as its layout-compatible prefix. Used only to `Release` in a `Drop` impl.
#[repr(C)]
#[allow(dead_code)]
struct UnknownVtbl {
    _query_interface: *const c_void, // 0
    _add_ref: *const c_void,         // 1
    release: unsafe extern "system" fn(*mut c_void) -> u32, // 2
}

/// `IMMDeviceEnumeratorVtbl` (mmdeviceapi.h:800-852): IUnknown then the five interface
/// methods in declaration order (mmdeviceapi.h:760, 768, 776, 782, 786). We call slot 4.
#[repr(C)]
#[allow(dead_code)] // read only through a raw vtable pointer, never field-wise
struct IMMDeviceEnumeratorVtbl {
    _query_interface: *const c_void, // 0
    _add_ref: *const c_void,         // 1
    release: unsafe extern "system" fn(*mut c_void) -> u32, // 2
    _enum_audio_endpoints: *const c_void, // 3
    get_default_audio_endpoint: // 4
        unsafe extern "system" fn(*mut c_void, i32, i32, *mut *mut c_void) -> HResult,
    _get_device: *const c_void, // 5
    _register_endpoint_notification_callback: *const c_void, // 6
    _unregister_endpoint_notification_callback: *const c_void, // 7
}

/// `IMMDeviceVtbl` (mmdeviceapi.h:464-508): IUnknown then Activate, OpenPropertyStore,
/// GetId, GetState (mmdeviceapi.h:430, 440, 446, 450). We call slot 4.
#[repr(C)]
#[allow(dead_code)]
struct IMMDeviceVtbl {
    _query_interface: *const c_void, // 0
    _add_ref: *const c_void,         // 1
    release: unsafe extern "system" fn(*mut c_void) -> u32, // 2
    _activate: *const c_void,        // 3
    open_property_store: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> HResult, // 4
    _get_id: *const c_void,  // 5
    _get_state: *const c_void, // 6
}

/// `IPropertyStoreVtbl` (propsys.h:519-545 as the C interface): IUnknown then GetCount,
/// GetAt, GetValue, SetValue, Commit (propsys.h:520, 524, 528, 532, 537). We call slot 5.
#[repr(C)]
#[allow(dead_code)]
struct IPropertyStoreVtbl {
    _query_interface: *const c_void, // 0
    _add_ref: *const c_void,         // 1
    release: unsafe extern "system" fn(*mut c_void) -> u32, // 2
    _get_count: *const c_void,       // 3
    _get_at: *const c_void,          // 4
    get_value: // 5
        unsafe extern "system" fn(*mut c_void, *const PropertyKey, *mut PropVariant) -> HResult,
    _set_value: *const c_void, // 6
    _commit: *const c_void,    // 7
}

#[link(name = "ole32")]
extern "system" {
    fn CoInitializeEx(pv_reserved: *mut c_void, dw_co_init: u32) -> HResult;
    fn CoCreateInstance(
        rclsid: *const Guid,
        p_unk_outer: *mut c_void,
        dw_cls_context: u32,
        riid: *const Guid,
        ppv: *mut *mut c_void,
    ) -> HResult;
    fn CoUninitialize();
    fn PropVariantClear(pvar: *mut PropVariant) -> HResult;
}

/// Balances `CoInitializeEx` — but only when this thread's init was ours.
struct ComGuard {
    uninit: bool,
}

impl ComGuard {
    /// `RPC_E_CHANGED_MODE` counts as success-without-ownership: COM was already
    /// initialised on this thread (by winit/eframe, typically), so the enumerator can
    /// still be created, but uninitialising would undo someone else's work.
    fn init() -> Result<Self, String> {
        // SAFETY: no output parameters; the return code is checked exhaustively.
        let hr = unsafe { CoInitializeEx(null_mut(), COINIT_APARTMENTTHREADED) };
        match hr {
            S_OK | S_FALSE => Ok(ComGuard { uninit: true }),
            RPC_E_CHANGED_MODE => Ok(ComGuard { uninit: false }),
            _ => Err(format!("CoInitializeEx failed: HRESULT 0x{:08X}", hr as u32)),
        }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.uninit {
            // SAFETY: paired with our own successful CoInitializeEx on this thread.
            unsafe { CoUninitialize() };
        }
    }
}

/// An owned COM interface pointer; `Release()`s on drop, so every error path after a
/// successful create still balances its reference.
struct ComPtr {
    ptr: *mut c_void,
}

impl Drop for ComPtr {
    fn drop(&mut self) {
        // SAFETY: `ptr` is a live interface; slot 2 is `Release` on every vtable, and the
        // `UnknownVtbl` prefix is layout-compatible with each concrete vtable above.
        unsafe { (vtable::<UnknownVtbl>(self.ptr).release)(self.ptr) };
    }
}

/// The vtable an interface pointer's first machine word points at. The pointer's target
/// lives in the (never unloaded) COM server for the lifetime of the process, hence the
/// `'static`; callers keep the owning `ComPtr` alive across the call.
unsafe fn vtable<T>(ptr: *mut c_void) -> &'static T {
    // SAFETY: a COM interface pointer's first word is its vtable, laid out by `T` in the
    // SDK's method order.
    &**ptr.cast::<*const T>()
}

/// Read a NUL-terminated UTF-16 string from COM-owned memory into an owned `String`.
/// Bounded so a malformed pointer errors rather than walking forever.
unsafe fn read_lpwstr(ptr: *const u16) -> Result<String, String> {
    if ptr.is_null() {
        return Err("PKEY_Device_FriendlyName was VT_LPWSTR with a null pointer".into());
    }
    let mut len = 0usize;
    // SAFETY: `ptr` is the LPWSTR the property store handed us; we stop at the first NUL
    // or at MAX_NAME_UNITS, whichever comes first.
    while len < MAX_NAME_UNITS && *ptr.add(len) != 0 {
        len += 1;
    }
    if len == MAX_NAME_UNITS {
        return Err(format!(
            "friendly name is not NUL-terminated within {MAX_NAME_UNITS} UTF-16 code units"
        ));
    }
    // SAFETY: the loop above proved all `len` code units are initialised.
    let units = core::slice::from_raw_parts(ptr, len);
    Ok(utf16_to_string(units))
}

/// UTF-16 to `String`, replacing any unpaired surrogate rather than panicking.
fn utf16_to_string(units: &[u16]) -> String {
    String::from_utf16_lossy(units)
}

/// Friendly name of the default `eRender`/`eConsole` endpoint, e.g.
/// `Speakers (Realtek(R) Audio)`. Read-only: the property store is opened `STGM_READ`
/// and only `GetValue` is called. `Err` carries a human-readable message — including a
/// dedicated one when no default render endpoint exists at all.
pub fn default_render_endpoint() -> Result<String, String> {
    let _com = ComGuard::init()?;

    let mut enumerator: *mut c_void = null_mut();
    // SAFETY: `enumerator` is a valid out-parameter; the CLSID/IID are the constants
    // verified against mmdeviceapi.h above.
    let hr = unsafe {
        CoCreateInstance(
            &CLSID_MMDEVICE_ENUMERATOR,
            null_mut(),
            CLSCTX_INPROC_SERVER,
            &IID_IMMDEVICE_ENUMERATOR,
            &mut enumerator,
        )
    };
    if hr < 0 {
        return Err(format!(
            "CoCreateInstance(MMDeviceEnumerator) failed: HRESULT 0x{:08X}",
            hr as u32
        ));
    }
    if enumerator.is_null() {
        return Err("CoCreateInstance reported success but returned null".into());
    }
    let enumerator = ComPtr { ptr: enumerator };

    let mut device: *mut c_void = null_mut();
    // SAFETY: live enumerator; eRender/eConsole and the IMMDevice out-parameter match
    // `GetDefaultAudioEndpoint`'s declared signature.
    let hr = unsafe {
        (vtable::<IMMDeviceEnumeratorVtbl>(enumerator.ptr).get_default_audio_endpoint)(
            enumerator.ptr,
            E_RENDER,
            E_CONSOLE,
            &mut device,
        )
    };
    if hr == E_NOTFOUND {
        return Err(
            "no default audio render endpoint exists (every output device is disabled or \
             unplugged); pick a playback device in Windows sound settings"
                .into(),
        );
    }
    if hr < 0 {
        return Err(format!(
            "IMMDeviceEnumerator::GetDefaultAudioEndpoint failed: HRESULT 0x{:08X}",
            hr as u32
        ));
    }
    if device.is_null() {
        return Err("GetDefaultAudioEndpoint reported success but returned null".into());
    }
    let device = ComPtr { ptr: device };

    let mut store: *mut c_void = null_mut();
    // SAFETY: live device; STGM_READ is read-only, and the store out-parameter matches
    // `OpenPropertyStore`'s declared signature.
    let hr = unsafe {
        (vtable::<IMMDeviceVtbl>(device.ptr).open_property_store)(
            device.ptr,
            STGM_READ,
            &mut store,
        )
    };
    if hr < 0 {
        return Err(format!(
            "IMMDevice::OpenPropertyStore failed: HRESULT 0x{:08X}",
            hr as u32
        ));
    }
    if store.is_null() {
        return Err("OpenPropertyStore reported success but returned null".into());
    }
    let store = ComPtr { ptr: store };

    let key = PropertyKey {
        fmtid: FMTID_DEVICE,
        pid: PKEY_DEVICE_FRIENDLY_NAME_PID,
    };
    let mut pv = PropVariant::empty();
    // SAFETY: live store; `key` is a valid REFPROPERTYKEY for the duration of the call;
    // `pv` is a zeroed PROPVARIANT the store may fill.
    let hr = unsafe {
        (vtable::<IPropertyStoreVtbl>(store.ptr).get_value)(store.ptr, &key, &mut pv)
    };
    if hr < 0 {
        return Err(format!(
            "IPropertyStore::GetValue(PKEY_Device_FriendlyName) failed: HRESULT 0x{:08X}",
            hr as u32
        ));
    }

    // The name is read out *before* clearing: PropVariantClear frees the very buffer a
    // VT_LPWSTR arm points at, so the copy must already be owned by us.
    let name = if pv.vt == VT_LPWSTR {
        // SAFETY: `vt` says the union holds an LPWSTR.
        let ptr = unsafe { pv.value.pwsz_val };
        // SAFETY: same as above — the arm really is an LPWSTR.
        unsafe { read_lpwstr(ptr) }
    } else {
        Err(format!(
            "PKEY_Device_FriendlyName is not VT_LPWSTR (vt = {})",
            pv.vt
        ))
    };

    // SAFETY: `pv` was filled by GetValue above and is cleared exactly once here, on
    // both the success and the wrong-type path.
    let hr = unsafe { PropVariantClear(&mut pv) };
    if hr < 0 {
        return Err(format!("PropVariantClear failed: HRESULT 0x{:08X}", hr as u32));
    }

    let name = name?;
    if name.is_empty() {
        return Err("default audio render endpoint reported an empty friendly name".into());
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::offset_of;
    use core::mem::size_of;

    // Pure tests only: nothing here touches COM or a real audio device.

    /// The single most dangerous value in this module: `vt` at 0 and the union at 8
    /// (three WORD pads, propidl.h:307-310 / 290-292), 16-byte union, 24 bytes total.
    #[test]
    fn propvariant_layout_matches_header() {
        assert_eq!(size_of::<PropVariant>(), 24);
        assert_eq!(offset_of!(PropVariant, vt), 0);
        assert_eq!(offset_of!(PropVariant, value), 8);
        assert_eq!(size_of::<PropVariantValue>(), 16);
        // The union is pointer-aligned, which is what keeps the LPWSTR at offset 8.
        assert_eq!(core::mem::align_of::<PropVariant>(), 8);
    }

    /// wtypes.h:865 — the arm we read.
    #[test]
    fn vt_lpwstr_is_31() {
        assert_eq!(VT_LPWSTR, 31u16);
    }

    /// A GUID passed to COM by pointer; the byte order is load-bearing.
    #[test]
    fn guid_is_windows_guid_sized_and_bytes_match_headers() {
        assert_eq!(size_of::<Guid>(), 16);
        assert_eq!(size_of::<PropertyKey>(), 20);
        assert_eq!(CLSID_MMDEVICE_ENUMERATOR.data1, 0xBCDE_0395);
        assert_eq!(CLSID_MMDEVICE_ENUMERATOR.data4, [0x8E, 0x3D, 0xC4, 0x57, 0x92, 0x91, 0x69, 0x2E]);
        assert_eq!(IID_IMMDEVICE_ENUMERATOR.data1, 0xA956_64D2);
        assert_eq!(IID_IMMDEVICE_ENUMERATOR.data4, [0xA7, 0x46, 0xDE, 0x8D, 0xB6, 0x36, 0x17, 0xE6]);
        assert_eq!(IID_IPROPERTYSTORE.data1, 0x886D_8EEB);
        assert_eq!(IID_IPROPERTYSTORE.data4, [0x8D, 0x02, 0xCD, 0xBA, 0x1D, 0xBD, 0xCF, 0x99]);
        assert_eq!(FMTID_DEVICE.data1, 0xA45C_254E);
        assert_eq!(FMTID_DEVICE.data4, [0x80, 0x20, 0x67, 0xD1, 0x46, 0xA8, 0x50, 0xE0]);
        assert_eq!(PKEY_DEVICE_FRIENDLY_NAME_PID, 14);
    }

    /// One missing entry would shift every later method onto the wrong function.
    #[test]
    fn vtable_slot_counts_match_the_headers() {
        let slot = size_of::<*const c_void>();
        assert_eq!(size_of::<UnknownVtbl>(), 3 * slot);
        assert_eq!(size_of::<IMMDeviceEnumeratorVtbl>(), 8 * slot); // 3 + 5 methods
        assert_eq!(size_of::<IMMDeviceVtbl>(), 7 * slot); // 3 + 4 methods
        assert_eq!(size_of::<IPropertyStoreVtbl>(), 8 * slot); // 3 + 5 methods
    }

    #[test]
    fn utf16_conversion_handles_empty_and_non_ascii() {
        assert_eq!(utf16_to_string(&[]), "");
        let units: Vec<u16> = "Quäker → 東京".encode_utf16().collect();
        assert_eq!(utf16_to_string(&units), "Quäker → 東京");
        // An unpaired surrogate must degrade, not panic.
        assert_eq!(utf16_to_string(&[0xD800]), "\u{FFFD}");
    }

    #[test]
    fn read_lpwstr_stops_at_nul() {
        let buf = [0x0051u16, 0x00E4, 0x0000, 0x005A];
        // SAFETY: `buf` is a valid, NUL-terminated UTF-16 buffer that outlives the call.
        assert_eq!(unsafe { read_lpwstr(buf.as_ptr()) }, Ok("Qä".to_string()));
        // SAFETY: a null pointer is exactly the input under test.
        assert!(unsafe { read_lpwstr(core::ptr::null()) }.is_err());
    }
}
