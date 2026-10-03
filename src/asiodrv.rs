// SPDX-License-Identifier: GPL-3.0-or-later
//! Minimal in-process ASIO driver access: open one selected driver (design.md §5,
//! decision 26), read its capabilities, optionally show its own control panel.
//! `ASIOStart` is **never** called, nor `createBuffers`/`disposeBuffers`/
//! `setSampleRate` — `Vtbl` keeps those slots for layout but never invokes them.
//! Sources: `iasiodrv.h`, `asio.h` (`#pragma pack(push,4)`), `asiolist.cpp:184`.

use core::ffi::{c_char, c_void};
use serde::Serialize;
use std::ptr::null_mut;

/// `ASIOBool` / `ASIOError` / `HRESULT` — all a Win32 `long` (32-bit under LLP64).
type AsioBool = i32;
type AsioError = i32;
type HResult = i32;

/// `ASIOTrue` (asio.h) — **1**, not 0, so `init` success is `== 1`.
const ASIO_TRUE: AsioBool = 1;
/// `ASE_OK` (asio.h).
const ASE_OK: AsioError = 0;
/// `S_OK` (winerror.h).
const S_OK: HResult = 0;
/// `S_FALSE` (winerror.h) — a successful `CoInitializeEx` that was already done.
const S_FALSE: HResult = 1;
/// `RPC_E_CHANGED_MODE` (winerror.h): this thread already holds a COM apartment in
/// the other threading model — usable, but we must not pair it with `CoUninitialize`.
const RPC_E_CHANGED_MODE: HResult = 0x8001_0106u32 as i32;
/// `COINIT_APARTMENTTHREADED` (`objbase.h:33`) — what `asiolist.cpp`'s
/// `CoInitialize(0)` yields.
const COINIT_APARTMENTTHREADED: u32 = 0x2;
/// `CLSCTX_INPROC_SERVER` (`WTypesbase.h:369`) — the driver DLL loads into *this*
/// process, which is why `controlPanel()` opens its window here.
const CLSCTX_INPROC_SERVER: u32 = 0x1;

/// `GUID` (`shared/guiddef.h`), mirrored locally so no COM binding crate is needed.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

/// Parse a `{8-4-4-4-12}` CLSID string as written to the ASIO registry. Braces are
/// optional and case does not matter (`CLSIDFromString` accepts both forms).
fn parse_guid(s: &str) -> Result<Guid, String> {
    let t = s.trim();
    let body = t.strip_prefix('{').unwrap_or(t);
    let body = body.strip_suffix('}').unwrap_or(body);
    let parts: Vec<&str> = body.split('-').collect();
    let want = [8usize, 4, 4, 4, 12];
    if parts.len() != want.len() {
        return Err(format!("not a GUID ({} groups, want 5): {s}", parts.len()));
    }
    // Width + hex check first: non-hex is rejected before any byte slicing.
    let ok = parts
        .iter()
        .zip(want)
        .all(|(p, n)| p.len() == n && p.chars().all(|c| c.is_ascii_hexdigit()));
    if !ok {
        return Err(format!("not a GUID: {s}"));
    }
    let byte = |p: &str| u8::from_str_radix(p, 16);
    // `Data4[8]` is copied verbatim, so the two tail groups are split as-is.
    let mut data4 = [0u8; 8];
    for i in 0..2 {
        data4[i] = byte(&parts[3][2 * i..2 * i + 2]).map_err(|_| format!("bad GUID: {s}"))?;
    }
    for i in 0..6 {
        data4[2 + i] = byte(&parts[4][2 * i..2 * i + 2]).map_err(|_| format!("bad GUID: {s}"))?;
    }
    Ok(Guid {
        data1: u32::from_str_radix(parts[0], 16).map_err(|_| format!("bad GUID: {s}"))?,
        data2: u16::from_str_radix(parts[1], 16).map_err(|_| format!("bad GUID: {s}"))?,
        data3: u16::from_str_radix(parts[2], 16).map_err(|_| format!("bad GUID: {s}"))?,
        data4,
    })
}

/// `ASIODriverInfo` (asio.h:477) — `errorMessage[124]` is `getErrorMessage`'s target.
#[repr(C, packed(4))]
#[allow(dead_code)]
struct AsioDriverInfo {
    asio_version: i32,
    driver_version: i32,
    name: [c_char; 32],
    error_message: [c_char; 124],
    sys_ref: *mut c_void,
}

/// `ASIOChannelInfo` (asio.h:767). Unused here (probe reads channel *counts*), but
/// declared so its assertion pins the pack-4 rule for the whole struct family.
#[repr(C, packed(4))]
#[allow(dead_code)]
struct AsioChannelInfo {
    channel: i32,
    is_input: AsioBool,
    is_active: AsioBool,
    channel_group: i32,
    sample_type: i32,
    name: [c_char; 32],
}

/// IUnknown's 3 slots then the 21 ASIO methods, in `iasiodrv.h` order. Slots this
/// module never calls are `*const c_void`: only their **presence** fixes the
/// layout. COM's Windows default is `__stdcall`, so every slot is `extern "system"`.
#[repr(C)]
#[allow(dead_code)] // read only through a raw vtable pointer, never field-wise
struct Vtbl {
    _query_interface: *const c_void,                                 // 0
    _add_ref: *const c_void,                                        // 1
    release: unsafe extern "system" fn(*mut c_void) -> u32,          // 2
    init: unsafe extern "system" fn(*mut c_void, *mut c_void) -> AsioBool, // 3
    get_driver_name: unsafe extern "system" fn(*mut c_void, *mut c_char), // 4
    get_driver_version: unsafe extern "system" fn(*mut c_void) -> i32, // 5
    get_error_message: unsafe extern "system" fn(*mut c_void, *mut c_char), // 6
    _start: *const c_void,                                          // 7  never called
    _stop: *const c_void,                                           // 8
    get_channels: unsafe extern "system" fn(*mut c_void, *mut i32, *mut i32) -> AsioError, // 9
    get_latencies: unsafe extern "system" fn(*mut c_void, *mut i32, *mut i32) -> AsioError, // 10
    get_buffer_size: // 11
        unsafe extern "system" fn(*mut c_void, *mut i32, *mut i32, *mut i32, *mut i32) -> AsioError,
    can_sample_rate: unsafe extern "system" fn(*mut c_void, f64) -> AsioError, // 12
    get_sample_rate: unsafe extern "system" fn(*mut c_void, *mut f64) -> AsioError, // 13
    _set_sample_rate: *const c_void,                                // 14 never called
    _get_clock_sources: *const c_void,                              // 15
    _set_clock_source: *const c_void,                               // 16
    _get_sample_position: *const c_void,                            // 17
    _get_channel_info: *const c_void,                               // 18
    _create_buffers: *const c_void,                                 // 19 never called
    _dispose_buffers: *const c_void,                                // 20 never called
    control_panel: unsafe extern "system" fn(*mut c_void) -> AsioError, // 21
    _future: *const c_void,                                         // 22
    _output_ready: *const c_void,                                   // 23
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
}

/// Balances `CoInitializeEx` — but only when this thread's init was ours.
struct ComGuard {
    uninit: bool,
}

impl ComGuard {
    /// `RPC_E_CHANGED_MODE` counts as success-without-ownership: COM was already
    /// initialised on this thread (by the window event loop, typically), so the driver can
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

/// An owned `IASIO` instance; `Release()`s on drop, so error paths still clean up.
struct Driver {
    ptr: *mut c_void,
}

impl Driver {
    /// Create the in-process instance. `rclsid == riid` is what the SDK does
    /// (`asiolist.cpp:184`) — the driver CLSID doubles as its interface IID.
    fn open(guid: &Guid) -> Result<Self, String> {
        let mut ptr: *mut c_void = null_mut();
        // SAFETY: `guid` serves as both class and interface id; `ptr` is valid.
        let hr = unsafe {
            CoCreateInstance(guid, null_mut(), CLSCTX_INPROC_SERVER, guid, &mut ptr)
        };
        if hr < 0 {
            return Err(format!("CoCreateInstance failed: HRESULT 0x{:08X}", hr as u32));
        }
        if ptr.is_null() {
            return Err("CoCreateInstance reported success but returned null".into());
        }
        Ok(Driver { ptr })
    }

    /// The vtable the instance's first machine word points at.
    fn vtbl(&self) -> &Vtbl {
        // SAFETY: a COM interface pointer's first word is its vtable, laid out by
        // `Vtbl` in the SDK's method order.
        unsafe { &**self.ptr.cast::<*const Vtbl>() }
    }

    /// `IASIO::init(NULL)` — `sysHandle` may be null (design.md §1.5). Succeeds
    /// only on `ASIOTrue`, per asio.h's counter-intuitive convention.
    fn init(&self) -> Result<(), String> {
        // SAFETY: `ptr` is a live IASIO instance.
        let ok = unsafe { (self.vtbl().init)(self.ptr, null_mut()) };
        if ok == ASIO_TRUE {
            Ok(())
        } else {
            Err(format!(
                "driver init failed (returned {ok}): {}",
                self.error_message()
            ))
        }
    }

    /// `getErrorMessage(char[124])`, buffer pre-zeroed so silence yields `""`.
    fn error_message(&self) -> String {
        let mut buf = [0 as c_char; 124];
        // SAFETY: `buf` is exactly the documented `errorMessage` size.
        unsafe { (self.vtbl().get_error_message)(self.ptr, buf.as_mut_ptr()) };
        cstr_latin1(&buf)
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        // SAFETY: balances the reference CoCreateInstance handed us.
        unsafe { (self.vtbl().release)(self.ptr) };
    }
}

/// Read a NUL-terminated `char[]` as latin-1. ASIO names are `char`, never UTF-16,
/// and latin-1 maps every byte to one code point — so this never drops a byte.
fn cstr_latin1(buf: &[c_char]) -> String {
    buf.iter()
        .take_while(|c| **c != 0)
        .map(|c| *c as u8 as char)
        .collect()
}

/// One selected driver's capabilities, as reported by ASIO after `init`: name
/// (latin-1 `char[32]`), version, both channel counts, all four buffer bounds, both
/// latency pairs, and the rate — 48000.0 if the driver accepts that rate but cannot
/// report the current one, 0.0 if even that fails (design.md §6.5).
/// Serialized to the frontend over IPC, so it carries `Serialize` (design.md 9.4).
#[derive(Clone, Debug, Serialize)]
pub struct DriverInfo {
    pub driver_name: String,
    pub driver_version: i32,
    pub input_channels: i32,
    pub output_channels: i32,
    pub current_sample_rate: f64,
    pub buffer_min: i32,
    pub buffer_max: i32,
    pub buffer_preferred: i32,
    pub buffer_granularity: i32,
    /// `getLatencies`, input side, in samples; `(0, 0)` when unavailable.
    pub input_latency: (i32, i32),
    /// `getLatencies`, output side, in samples; `(0, 0)` when unavailable.
    pub output_latency: (i32, i32),
}

/// Open the driver identified by `clsid` (a string like `{...}`), initialise it,
/// read its capabilities, then release. `init` alone starts no audio. `Err` carries
/// a human-readable message, including `getErrorMessage` text when `init` fails.
pub fn probe(clsid: &str) -> Result<DriverInfo, String> {
    let guid = parse_guid(clsid)?;
    let _com = ComGuard::init()?;
    let drv = Driver::open(&guid)?;
    drv.init()?;
    let vt = drv.vtbl();
    // SAFETY below: `drv` is live; each call passes the types iasiodrv.h declares.
    let (mut inputs, mut outputs) = (0i32, 0i32);
    let hr = unsafe { (vt.get_channels)(drv.ptr, &mut inputs, &mut outputs) };
    if hr != ASE_OK {
        return Err(format!("getChannels failed: ASIOError {hr}"));
    }
    let (mut min, mut max, mut preferred, mut granularity) = (0i32, 0i32, 0i32, 0i32);
    let hr = unsafe {
        (vt.get_buffer_size)(drv.ptr, &mut min, &mut max, &mut preferred, &mut granularity)
    };
    if hr != ASE_OK {
        return Err(format!("getBufferSize failed: ASIOError {hr}"));
    }
    // The rate can legitimately be unavailable (ASE_NoClock with no clock present),
    // so it degrades instead of failing the whole probe.
    let mut rate = 0.0f64;
    if unsafe { (vt.get_sample_rate)(drv.ptr, &mut rate) } != ASE_OK || rate <= 0.0 {
        rate = if unsafe { (vt.can_sample_rate)(drv.ptr, 48000.0) } == ASE_OK {
            48000.0
        } else {
            0.0
        };
    }
    // Latencies are informational: a driver whose channels are idle may refuse
    // (ASE_NotPresent). Never fail the probe for them.
    let (mut in_lat, mut out_lat) = (0i32, 0i32);
    let have_latencies =
        unsafe { (vt.get_latencies)(drv.ptr, &mut in_lat, &mut out_lat) } == ASE_OK;
    let mut name_buf = [0 as c_char; 32];
    // SAFETY: `name_buf` is exactly the documented `char name[32]` size, and its
    // first byte is pre-set to 0 as the ASIO convention requires.
    unsafe { (vt.get_driver_name)(drv.ptr, name_buf.as_mut_ptr()) };
    Ok(DriverInfo {
        driver_name: cstr_latin1(&name_buf),
        // SAFETY: a plain value-returning method.
        driver_version: unsafe { (vt.get_driver_version)(drv.ptr) },
        input_channels: inputs,
        output_channels: outputs,
        current_sample_rate: rate,
        buffer_min: min,
        buffer_max: max,
        buffer_preferred: preferred,
        buffer_granularity: granularity,
        input_latency: if have_latencies { (in_lat, 0) } else { (0, 0) },
        output_latency: if have_latencies { (out_lat, 0) } else { (0, 0) },
    })
}

/// Open the driver's own control panel: `init`, then `controlPanel()`. **Blocks the
/// calling thread until the user closes the driver's window** (the panel is created
/// inside this process — `CLSCTX_INPROC_SERVER`), so callers must use a dedicated
/// thread. `Err` for a driver with no panel: asio.h documents `ASE_NotPresent`.
pub fn open_control_panel(clsid: &str) -> Result<(), String> {
    let guid = parse_guid(clsid)?;
    let _com = ComGuard::init()?;
    let drv = Driver::open(&guid)?;
    drv.init()?;
    // SAFETY: live, initialised IASIO instance.
    let rc = unsafe { (drv.vtbl().control_panel)(drv.ptr) };
    if rc == ASE_OK {
        Ok(())
    } else {
        Err(format!(
            "controlPanel returned ASIOError {rc} \
             (ASE_NotPresent means this driver has no control panel)"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::size_of;

    // Pure tests only: nothing here touches COM or a real driver.

    #[test]
    fn parse_guid_accepts_braced_clsid() {
        let g = parse_guid("{6C1E7D94-3A52-4B8F-9E27-5D0B4C8A1F63}").unwrap();
        assert_eq!(g.data1, 0x6C1E_7D94);
        assert_eq!(g.data2, 0x3A52);
        assert_eq!(g.data3, 0x4B8F);
        assert_eq!(g.data4, [0x9E, 0x27, 0x5D, 0x0B, 0x4C, 0x8A, 0x1F, 0x63]);
        // The registry form is unbraced and lower-case; CLSIDFromString accepts
        // both, so we must too.
        assert_eq!(parse_guid("6c1e7d94-3a52-4b8f-9e27-5d0b4c8a1f63").unwrap(), g);
    }

    #[test]
    fn parse_guid_rejects_malformed_input() {
        assert!(parse_guid("not-a-guid").is_err());
        assert!(parse_guid("").is_err());
        assert!(parse_guid("{6C1E7D94-3A52-4B8F-9E27}").is_err()); // too few groups
        assert!(parse_guid("{6C1E7D9-3A52-4B8F-9E27-5D0B4C8A1F63}").is_err()); // short group
        assert!(parse_guid("{6C1E7D9Z-3A52-4B8F-9E27-5D0B4C8A1F63}").is_err()); // non-hex
        assert!(parse_guid("{6C1E7D94_3A52_4B8F_9E27_5D0B4C8A1F63}").is_err()); // separators
    }

    /// Passed to COM by pointer; the size is load-bearing, not cosmetic.
    #[test]
    fn guid_is_windows_guid_sized() {
        assert_eq!(size_of::<Guid>(), 16);
    }

    /// Mirrors viola-bridge `crates/viola_asio/src/ffi.rs`'s own size assertions,
    /// whose numbers are read off asio.h under `#pragma pack(push,4)`.
    #[test]
    fn sdk_struct_sizes_match_msvc_pack_4() {
        // long, ASIOBool, ASIOBool, long, ASIOSampleType, char[32] → 5*4 + 32.
        assert_eq!(size_of::<AsioChannelInfo>(), 52);
        // long, long, char[32], char[124], void* → 4+4+32+124+8, with the pointer
        // kept at offset 164 (not 168) by the pack pragma.
        assert_eq!(size_of::<AsioDriverInfo>(), 172);
    }

    /// One missing entry would shift every later method onto the wrong function.
    #[test]
    fn vtable_has_24_slots() {
        assert_eq!(size_of::<Vtbl>(), 24 * size_of::<*const c_void>());
    }
}
