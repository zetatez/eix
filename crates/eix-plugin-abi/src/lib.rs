//! Stable C ABI shared by `eixd` and dylib plugins.
//!
//! This crate is deliberately dependency-free and tiny: both sides of the
//! boundary must agree on the exact layout. It exposes:
//!
//! - [`EixStr`] / [`EixBuf`]: borrowed input and owned output byte buffers.
//! - [`HostApiV1`]: the function table the host passes to a plugin.
//! - [`PluginAbiVersionFn`] / [`PluginCreateFn`] / [`PluginDispatchFn`] /
//!   [`PluginFreeFn`] / [`PluginDestroyFn`]: the only functions a plugin
//!   exports.
//! - Status codes for the `i32` return values.
//!
//! The design rule is simple: **the ABI never grows**. New capabilities are
//! new method names on the bidirectional `dispatch` function, not new symbols.
//!
//! # Memory ownership
//!
//! - `EixStr` inputs are borrowed for the duration of the call. The callee must
//!   not retain them.
//! - `EixBuf` returned from a plugin is owned by the plugin. The host must call
//!   `eix_plugin_free`.
//! - `EixBuf` returned from the host is owned by the host. The plugin must call
//!   `HostApiV1::free`.
//!
//! Never free memory across the boundary with the wrong allocator.

#![deny(unsafe_op_in_unsafe_fn)]

use std::ffi::c_void;

/// Current ABI version. Bump only for breaking layout/semantic changes.
pub const ABI_VERSION: u32 = 1;

pub const EIX_OK: i32 = 0;
pub const EIX_ERR_UNKNOWN_METHOD: i32 = -1;
pub const EIX_ERR_INVALID: i32 = -2;
pub const EIX_ERR_DENIED: i32 = -3;
pub const EIX_ERR_PANIC: i32 = -4;
pub const EIX_ERR_VERSION: i32 = -5;
pub const EIX_ERR_REENTRANT: i32 = -6;
pub const EIX_ERR_TIMEOUT: i32 = -7;
pub const EIX_ERR_INTERNAL: i32 = -8;

/// A borrowed UTF-8 byte string.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct EixStr {
    pub ptr: *const u8,
    pub len: usize,
}

impl EixStr {
    pub const EMPTY: Self = Self {
        ptr: std::ptr::null(),
        len: 0,
    };

    /// Borrow the bytes. The caller guarantees the pointer is valid for
    /// `len` bytes for the duration of the call.
    pub fn as_bytes(&self) -> &[u8] {
        if self.ptr.is_null() || self.len == 0 {
            &[]
        } else {
            // SAFETY: upheld by the ABI contract at the call boundary.
            unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
        }
    }

    pub fn as_str(&self) -> Result<&str, std::str::Utf8Error> {
        std::str::from_utf8(self.as_bytes())
    }

    pub fn from_slice(bytes: &[u8]) -> Self {
        Self {
            ptr: bytes.as_ptr(),
            len: bytes.len(),
        }
    }
}

/// An owned byte buffer transferred across the boundary.
///
/// `EixBuf` is intentionally not `Copy`: copying it would create two owners and
/// a double free. Use the matching `free` function exactly once.
#[repr(C)]
#[derive(Debug)]
pub struct EixBuf {
    pub ptr: *mut u8,
    pub len: usize,
    pub cap: usize,
}

impl EixBuf {
    pub const EMPTY: Self = Self {
        ptr: std::ptr::null_mut(),
        len: 0,
        cap: 0,
    };

    pub fn from_vec(mut bytes: Vec<u8>) -> Self {
        if bytes.is_empty() {
            return Self::EMPTY;
        }
        let buf = Self {
            ptr: bytes.as_mut_ptr(),
            len: bytes.len(),
            cap: bytes.capacity(),
        };
        std::mem::forget(bytes);
        buf
    }

    pub fn is_null(&self) -> bool {
        self.ptr.is_null()
    }

    /// Reclaim a buffer created by [`EixBuf::from_vec`] in this module.
    ///
    /// # Safety
    ///
    /// The buffer must have been created by the same allocator that is now
    /// reclaiming it, and it must not be used afterwards.
    pub unsafe fn into_vec(self) -> Vec<u8> {
        if self.ptr.is_null() {
            return Vec::new();
        }
        // SAFETY: guaranteed by the caller.
        unsafe { Vec::from_raw_parts(self.ptr, self.len, self.cap) }
    }
}

/// Plugin -> host dispatch. `out` may be null for notifications; otherwise the
/// host writes a host-owned buffer that the plugin releases with [`HostApiV1::free`].
pub type HostDispatchFn =
    unsafe extern "C" fn(ctx: *mut c_void, method: EixStr, params: EixStr, out: *mut EixBuf) -> i32;

/// Release a host-owned buffer.
pub type HostFreeFn = unsafe extern "C" fn(ctx: *mut c_void, buf: EixBuf);

/// The function table passed from the host to a plugin during `create`.
///
/// `size` allows the host to append fields in a future minor revision: an old
/// plugin reads only the prefix it knows, a new plugin checks `size` before
/// touching newer fields.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct HostApiV1 {
    pub abi_version: u32,
    pub size: usize,
    pub ctx: *mut c_void,
    pub dispatch: Option<HostDispatchFn>,
    pub free: Option<HostFreeFn>,
}

impl HostApiV1 {
    /// True when this table is safe for a plugin built against ABI v1.
    pub fn is_compatible(&self) -> bool {
        self.abi_version == ABI_VERSION && self.size >= std::mem::size_of::<Self>()
    }
}

pub type PluginAbiVersionFn = unsafe extern "C" fn() -> u32;

pub type PluginCreateFn =
    unsafe extern "C" fn(host: *const HostApiV1, config: EixStr, out: *mut *mut c_void) -> i32;

/// Plugin dispatch. The plugin writes a plugin-owned buffer into `out`; the
/// host releases it with `eix_plugin_free`.
pub type PluginDispatchFn = unsafe extern "C" fn(
    instance: *mut c_void,
    method: EixStr,
    params: EixStr,
    out: *mut EixBuf,
) -> i32;

/// Release a plugin-owned buffer.
pub type PluginFreeFn = unsafe extern "C" fn(instance: *mut c_void, buf: EixBuf);

pub type PluginDestroyFn = unsafe extern "C" fn(instance: *mut c_void);

/// Human-readable name for a status code.
pub fn status_name(code: i32) -> &'static str {
    match code {
        EIX_OK => "ok",
        EIX_ERR_UNKNOWN_METHOD => "unknown_method",
        EIX_ERR_INVALID => "invalid_params",
        EIX_ERR_DENIED => "denied",
        EIX_ERR_PANIC => "panic",
        EIX_ERR_VERSION => "version_mismatch",
        EIX_ERR_REENTRANT => "reentrant",
        EIX_ERR_TIMEOUT => "timeout",
        EIX_ERR_INTERNAL => "internal",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn str_round_trips_borrowed_bytes() {
        let bytes = b"hello";
        let text = EixStr::from_slice(bytes);
        assert_eq!(text.as_bytes(), bytes);
        assert_eq!(text.as_str().unwrap(), "hello");
        assert_eq!(EixStr::EMPTY.as_bytes(), b"");
    }

    #[test]
    fn buf_round_trips_owned_bytes() {
        let buf = EixBuf::from_vec(vec![1, 2, 3]);
        assert_eq!(buf.len, 3);
        // SAFETY: the buffer was created by `from_vec` above.
        assert_eq!(unsafe { buf.into_vec() }, vec![1, 2, 3]);
        assert!(EixBuf::EMPTY.is_null());
    }

    #[test]
    fn host_api_rejects_older_or_foreign_layouts() {
        let api = HostApiV1 {
            abi_version: ABI_VERSION,
            size: std::mem::size_of::<HostApiV1>(),
            ctx: std::ptr::null_mut(),
            dispatch: None,
            free: None,
        };
        assert!(api.is_compatible());

        let mut older = api;
        older.size -= 1;
        assert!(!older.is_compatible());

        let mut foreign = api;
        foreign.abi_version += 1;
        assert!(!foreign.is_compatible());
    }
}
