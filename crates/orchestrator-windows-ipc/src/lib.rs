//! Audited Windows FFI for current-user-only Tokio named pipes.
#![cfg(windows)]
#![allow(clippy::missing_errors_doc)]

use std::{
    ffi::{OsStr, c_void},
    io, iter,
    os::windows::io::AsRawHandle as _,
    ptr,
};

use tokio::net::windows::named_pipe::{NamedPipeClient, NamedPipeServer, ServerOptions};
use windows_sys::Win32::{
    Foundation::{ERROR_SUCCESS, LocalFree},
    Security::{
        Authorization::{
            ConvertSecurityDescriptorToStringSecurityDescriptorW,
            ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
            SE_KERNEL_OBJECT,
        },
        DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
        SECURITY_ATTRIBUTES,
    },
};

struct LocalAllocation(*mut c_void);

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: The pointer is owned memory returned by a Windows API documented to use
            // LocalAlloc. This guard owns it and calls LocalFree exactly once.
            unsafe {
                let _ = LocalFree(self.0);
            }
        }
    }
}

pub fn create_current_user_only_named_pipe(
    options: &ServerOptions,
    name: impl AsRef<OsStr>,
    current_user_sid: &str,
) -> io::Result<NamedPipeServer> {
    if !valid_sid_text(current_user_sid) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "current-user SID is not canonical SID text",
        ));
    }
    let sddl = format!("O:{current_user_sid}D:P(A;;GA;;;{current_user_sid})");
    let encoded = sddl.encode_utf16().chain(iter::once(0)).collect::<Vec<_>>();
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: `encoded` is NUL-terminated and remains live for the call. `descriptor` points to
    // writable storage and receives a LocalAlloc-owned self-relative security descriptor.
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            encoded.as_ptr(),
            SDDL_REVISION_1,
            &raw mut descriptor,
            ptr::null_mut(),
        )
    };
    if converted == 0 {
        return Err(io::Error::last_os_error());
    }
    let _descriptor = LocalAllocation(descriptor.cast());
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(std::mem::size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(u32::MAX),
        lpSecurityDescriptor: descriptor.cast(),
        bInheritHandle: 0,
    };
    // SAFETY: `attributes` and its security descriptor remain live through this synchronous
    // constructor call. Tokio copies the descriptor into the kernel pipe object.
    unsafe {
        options.create_with_security_attributes_raw(name, (&raw mut attributes).cast::<c_void>())
    }
}

pub fn named_pipe_security_descriptor(client: &NamedPipeClient) -> io::Result<String> {
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    let security_information = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
    // SAFETY: The Tokio client owns a valid live kernel handle. All optional output pointers are
    // null, and `descriptor` is writable storage for the LocalAlloc-owned result.
    let status = unsafe {
        GetSecurityInfo(
            client.as_raw_handle(),
            SE_KERNEL_OBJECT,
            security_information,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &raw mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(
            i32::try_from(status).unwrap_or(i32::MAX),
        ));
    }
    let _descriptor = LocalAllocation(descriptor.cast());
    let mut encoded = ptr::null_mut();
    let mut encoded_len = 0;
    // SAFETY: `descriptor` is live and valid. The output pointers are writable and the returned
    // string is LocalAlloc-owned until `_encoded` drops.
    let converted = unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor,
            SDDL_REVISION_1,
            security_information,
            &raw mut encoded,
            &raw mut encoded_len,
        )
    };
    if converted == 0 {
        return Err(io::Error::last_os_error());
    }
    let _encoded = LocalAllocation(encoded.cast());
    // SAFETY: Windows returned `encoded` with exactly `encoded_len` initialized UTF-16 code units.
    let units = unsafe {
        std::slice::from_raw_parts(encoded, usize::try_from(encoded_len).unwrap_or(usize::MAX))
    };
    let units = units.strip_suffix(&[0]).unwrap_or(units);
    String::from_utf16(units).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn valid_sid_text(sid: &str) -> bool {
    sid.starts_with("S-")
        && sid.len() > 2
        && sid
            .bytes()
            .skip(2)
            .all(|byte| byte.is_ascii_digit() || byte == b'-')
}
