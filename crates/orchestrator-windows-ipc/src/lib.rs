//! Audited Windows FFI for current-user-only Tokio named pipes.
#![cfg(windows)]
#![allow(clippy::missing_errors_doc)]

use std::{
    ffi::{OsStr, OsString, c_void},
    io, iter,
    os::windows::{
        ffi::{OsStrExt as _, OsStringExt as _},
        io::AsRawHandle as _,
    },
    path::{Component, Path, PathBuf},
    ptr,
};

use tokio::net::windows::named_pipe::{NamedPipeClient, NamedPipeServer, ServerOptions};
#[cfg(test)]
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_ALREADY_EXISTS, ERROR_SUCCESS, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
        WAIT_ABANDONED, WAIT_FAILED, WAIT_OBJECT_0,
    },
    Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
        Authorization::{
            ConvertSecurityDescriptorToStringSecurityDescriptorW,
            ConvertStringSecurityDescriptorToSecurityDescriptorW, ConvertStringSidToSidW,
            GetSecurityInfo, SDDL_REVISION_1, SE_FILE_OBJECT, SE_KERNEL_OBJECT, SE_OBJECT_TYPE,
        },
        CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation,
        GetLengthSid, GetSecurityDescriptorControl, GetSecurityDescriptorDacl,
        GetSecurityDescriptorOwner, OBJECT_INHERIT_ACE, OWNER_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED, SECURITY_ATTRIBUTES,
    },
    Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, CreateDirectoryW, CreateFileW, FILE_ALL_ACCESS,
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
        GetFileInformationByHandle, GetShortPathNameW, OPEN_EXISTING, READ_CONTROL,
    },
    System::Threading::{
        CreateMutexW, INFINITE, MUTEX_ALL_ACCESS, ReleaseMutex, WaitForSingleObject,
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

pub struct CurrentUserMutex {
    handle: HANDLE,
}

impl Drop for CurrentUserMutex {
    fn drop(&mut self) {
        // SAFETY: `handle` is a live mutex handle owned by this guard. The guard acquired the
        // mutex exactly once and closes the handle exactly once after releasing ownership.
        unsafe {
            let _ = ReleaseMutex(self.handle);
            let _ = CloseHandle(self.handle);
        }
    }
}

struct OwnedWindowsHandle(HANDLE);

impl Drop for OwnedWindowsHandle {
    fn drop(&mut self) {
        // SAFETY: The handle is owned by this guard and is closed exactly once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Keeps every verified directory component open without delete sharing while bootstrap runs.
pub struct CurrentUserDirectoryTree {
    _handles: Vec<OwnedWindowsHandle>,
}

/// Creates each missing directory component with an explicit protected current-user DACL.
///
/// Components created by a concurrent process are accepted only after the same owner, DACL,
/// directory-type, and reparse-point checks. Existing ancestors are opened without delete sharing
/// and checked for reparse points, preventing their replacement until this guard is dropped.
pub fn ensure_current_user_only_directory_tree(
    path: &Path,
    current_user_sid: &str,
) -> io::Result<CurrentUserDirectoryTree> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "secure directory tree path must be absolute",
        ));
    }
    if !valid_sid_text(current_user_sid) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "current-user SID is not canonical SID text",
        ));
    }

    let mut current = PathBuf::new();
    let mut handles = Vec::new();
    let mut creating = false;
    let mut saw_directory = false;
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => current.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "secure directory tree path contains parent traversal",
                ));
            }
            Component::Normal(part) => {
                saw_directory = true;
                current.push(part);
                if creating {
                    handles.push(create_and_verify_current_user_directory(
                        &current,
                        current_user_sid,
                    )?);
                    continue;
                }
                match std::fs::symlink_metadata(&current) {
                    Ok(_) => handles.push(open_verified_directory(&current, None)?),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        creating = true;
                        handles.push(create_and_verify_current_user_directory(
                            &current,
                            current_user_sid,
                        )?);
                    }
                    Err(error) => return Err(path_error(&current, &error)),
                }
            }
        }
    }
    if !saw_directory {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "secure directory tree path has no directory component",
        ));
    }
    Ok(CurrentUserDirectoryTree { _handles: handles })
}

fn create_and_verify_current_user_directory(
    path: &Path,
    current_user_sid: &str,
) -> io::Result<OwnedWindowsHandle> {
    let sddl = format!("O:{current_user_sid}D:P(A;OICI;GA;;;{current_user_sid})");
    let encoded_sddl = sddl.encode_utf16().chain(iter::once(0)).collect::<Vec<_>>();
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    let converted = unsafe {
        // SAFETY: The SDDL is NUL-terminated and the output receives LocalAlloc-owned memory.
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            encoded_sddl.as_ptr(),
            SDDL_REVISION_1,
            &raw mut descriptor,
            ptr::null_mut(),
        )
    };
    if converted == 0 {
        return Err(io::Error::last_os_error());
    }
    let _descriptor = LocalAllocation(descriptor.cast());
    let attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(std::mem::size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(u32::MAX),
        lpSecurityDescriptor: descriptor.cast(),
        bInheritHandle: 0,
    };
    let encoded_path = encode_path(path)?;
    let created = unsafe {
        // SAFETY: The path is NUL-terminated and the attributes and descriptor live for the call.
        CreateDirectoryW(encoded_path.as_ptr(), &raw const attributes)
    };
    if created == 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(i32::try_from(ERROR_ALREADY_EXISTS).unwrap_or(i32::MAX)) {
            return Err(path_error(path, &error));
        }
    }
    open_verified_directory(path, Some(current_user_sid))
}

fn open_verified_directory(
    path: &Path,
    current_user_sid: Option<&str>,
) -> io::Result<OwnedWindowsHandle> {
    let encoded_path = encode_path(path)?;
    let desired_access = current_user_sid.map_or(0, |_| READ_CONTROL);
    let handle = unsafe {
        // SAFETY: The path is NUL-terminated. No delete sharing pins the component, and opening
        // the reparse point itself allows verification without following it.
        CreateFileW(
            encoded_path.as_ptr(),
            desired_access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        let error = io::Error::last_os_error();
        return Err(path_error(path, &error));
    }
    let handle = OwnedWindowsHandle(handle);
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    let loaded = unsafe {
        // SAFETY: The handle is live and the information output is writable.
        GetFileInformationByHandle(handle.0, &raw mut information)
    };
    if loaded == 0 {
        let error = io::Error::last_os_error();
        return Err(path_error(path, &error));
    }
    if information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "secure path component is not a directory: {}",
                path.display()
            ),
        ));
    }
    if information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "secure path component is a reparse point: {}",
                path.display()
            ),
        ));
    }
    if let Some(current_user_sid) = current_user_sid {
        verify_current_user_only_object(
            handle.0,
            SE_FILE_OBJECT,
            current_user_sid,
            FILE_ALL_ACCESS,
            OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
        )
        .map_err(|error| path_error(path, &error))?;
    }
    Ok(handle)
}

fn encode_path(path: &Path) -> io::Result<Vec<u16>> {
    let mut encoded = path.as_os_str().encode_wide().collect::<Vec<_>>();
    if encoded.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows path contains an interior NUL",
        ));
    }
    encoded.push(0);
    Ok(encoded)
}

/// Returns the path spelling reported by `GetShortPathNameW`.
///
/// When 8.3 name creation is disabled for the containing volume, Windows may return the original
/// long spelling. Callers must compare the result before treating it as an alternate alias.
pub fn short_path_name(path: &Path) -> io::Result<PathBuf> {
    let encoded = encode_path(path)?;
    // SAFETY: `encoded` is NUL-terminated and live for the call. A null output buffer with zero
    // capacity asks Windows for the required UTF-16 buffer length.
    let required = unsafe { GetShortPathNameW(encoded.as_ptr(), ptr::null_mut(), 0) };
    if required == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut short = vec![0_u16; usize::try_from(required).unwrap_or(usize::MAX)];
    // SAFETY: `encoded` remains live and NUL-terminated. `short` exposes `required` writable
    // UTF-16 units, matching the capacity passed to Windows.
    let written = unsafe { GetShortPathNameW(encoded.as_ptr(), short.as_mut_ptr(), required) };
    if written == 0 {
        return Err(io::Error::last_os_error());
    }
    if written >= required {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows short-path length changed between queries",
        ));
    }
    short.truncate(usize::try_from(written).unwrap_or(usize::MAX));
    Ok(PathBuf::from(OsString::from_wide(&short)))
}

fn path_error(path: &Path, error: &io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{}: {error}", path.display()))
}

pub fn acquire_current_user_mutex(
    name: impl AsRef<OsStr>,
    current_user_sid: &str,
) -> io::Result<CurrentUserMutex> {
    if !valid_sid_text(current_user_sid) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "current-user SID is not canonical SID text",
        ));
    }
    let sddl = format!("O:{current_user_sid}D:P(A;;GA;;;{current_user_sid})");
    let encoded_sddl = sddl.encode_utf16().chain(iter::once(0)).collect::<Vec<_>>();
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: `encoded_sddl` is NUL-terminated and live for the call. `descriptor` receives a
    // LocalAlloc-owned self-relative security descriptor.
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            encoded_sddl.as_ptr(),
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
    let encoded_name = name
        .as_ref()
        .encode_wide()
        .chain(iter::once(0))
        .collect::<Vec<_>>();
    // SAFETY: `attributes`, its descriptor, and the NUL-terminated name remain live for this
    // synchronous call. A successful call returns an owned kernel handle.
    let handle = unsafe { CreateMutexW(&raw mut attributes, 0, encoded_name.as_ptr()) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    if let Err(error) = verify_current_user_only_mutex(handle, current_user_sid) {
        // SAFETY: `handle` is owned by this function and has not been waited on.
        unsafe {
            let _ = CloseHandle(handle);
        }
        return Err(error);
    }
    // SAFETY: `handle` is a live waitable mutex handle. `INFINITE` cannot produce a timeout.
    let wait = unsafe { WaitForSingleObject(handle, INFINITE) };
    if let Err(error) = wait_status_result(wait, io::Error::last_os_error) {
        // SAFETY: `handle` is owned by this function and was not acquired on this path.
        unsafe {
            let _ = CloseHandle(handle);
        }
        return Err(error);
    }
    Ok(CurrentUserMutex { handle })
}

fn wait_status_result(status: u32, last_error: impl FnOnce() -> io::Error) -> io::Result<()> {
    match status {
        WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(()),
        WAIT_FAILED => Err(last_error()),
        unexpected => Err(io::Error::other(format!(
            "unexpected mutex wait status {unexpected}"
        ))),
    }
}

struct KernelSecurityDescriptor {
    pointer: PSECURITY_DESCRIPTOR,
    _allocation: LocalAllocation,
}

fn kernel_security_descriptor(
    handle: HANDLE,
    object_type: SE_OBJECT_TYPE,
) -> io::Result<KernelSecurityDescriptor> {
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    let status = unsafe {
        // SAFETY: `handle` is live, all optional component outputs are null, and `descriptor`
        // receives the LocalAlloc-owned self-relative descriptor.
        GetSecurityInfo(
            handle,
            object_type,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
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
    Ok(KernelSecurityDescriptor {
        pointer: descriptor,
        _allocation: LocalAllocation(descriptor.cast()),
    })
}

fn expected_sid(sid: &str) -> io::Result<LocalAllocation> {
    let encoded = sid.encode_utf16().chain(iter::once(0)).collect::<Vec<_>>();
    let mut pointer: PSID = ptr::null_mut();
    let converted = unsafe {
        // SAFETY: `encoded` is NUL-terminated and `pointer` receives LocalAlloc-owned SID bytes.
        ConvertStringSidToSidW(encoded.as_ptr(), &raw mut pointer)
    };
    if converted == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(LocalAllocation(pointer))
}

fn verify_current_user_only_mutex(handle: HANDLE, current_user_sid: &str) -> io::Result<()> {
    verify_current_user_only_object(
        handle,
        SE_KERNEL_OBJECT,
        current_user_sid,
        MUTEX_ALL_ACCESS,
        0,
    )
}

fn verify_current_user_only_object(
    handle: HANDLE,
    object_type: SE_OBJECT_TYPE,
    current_user_sid: &str,
    expected_access: u32,
    expected_ace_flags: u32,
) -> io::Result<()> {
    let descriptor = kernel_security_descriptor(handle, object_type)?;
    let expected = expected_sid(current_user_sid)?;
    let expected_sid = expected.0;

    let mut control = 0_u16;
    let mut revision = 0_u32;
    let control_loaded = unsafe {
        // SAFETY: The descriptor is live and both output pointers are writable.
        GetSecurityDescriptorControl(descriptor.pointer, &raw mut control, &raw mut revision)
    };
    if control_loaded == 0 {
        return Err(io::Error::last_os_error());
    }
    if control & SE_DACL_PROTECTED == 0 {
        return Err(invalid_mutex_security(
            "secured object DACL is not protected",
        ));
    }

    let mut owner: PSID = ptr::null_mut();
    let mut owner_defaulted = 0;
    let owner_loaded = unsafe {
        // SAFETY: The descriptor is live and the output pointers are writable.
        GetSecurityDescriptorOwner(descriptor.pointer, &raw mut owner, &raw mut owner_defaulted)
    };
    if owner_loaded == 0 {
        return Err(io::Error::last_os_error());
    }
    if owner.is_null()
        || owner_defaulted != 0
        || unsafe {
            // SAFETY: Both SIDs are live and validated by Windows descriptor conversion/loading.
            EqualSid(owner, expected_sid)
        } == 0
    {
        return Err(invalid_mutex_security(
            "secured object owner does not match the supplied current-user SID",
        ));
    }

    let mut dacl_present = 0;
    let mut dacl = ptr::null_mut();
    let mut dacl_defaulted = 0;
    let dacl_loaded = unsafe {
        // SAFETY: The descriptor is live and the output pointers are writable.
        GetSecurityDescriptorDacl(
            descriptor.pointer,
            &raw mut dacl_present,
            &raw mut dacl,
            &raw mut dacl_defaulted,
        )
    };
    if dacl_loaded == 0 {
        return Err(io::Error::last_os_error());
    }
    if dacl_present == 0 || dacl.is_null() || dacl_defaulted != 0 {
        return Err(invalid_mutex_security(
            "secured object has no explicit protected DACL",
        ));
    }

    verify_single_current_user_ace(dacl, expected_sid, expected_access, expected_ace_flags)
}

fn verify_single_current_user_ace(
    dacl: *mut ACL,
    expected_sid: PSID,
    expected_access: u32,
    expected_ace_flags: u32,
) -> io::Result<()> {
    let mut size = ACL_SIZE_INFORMATION::default();
    let size_loaded = unsafe {
        // SAFETY: `dacl` is live and `size` is writable for the declared structure length.
        GetAclInformation(
            dacl,
            (&raw mut size).cast(),
            u32::try_from(std::mem::size_of::<ACL_SIZE_INFORMATION>()).unwrap_or(u32::MAX),
            AclSizeInformation,
        )
    };
    if size_loaded == 0 {
        return Err(io::Error::last_os_error());
    }
    if size.AceCount != 1 {
        return Err(invalid_mutex_security(
            "secured object DACL must contain exactly one current-user allow entry",
        ));
    }

    let mut ace = ptr::null_mut();
    let ace_loaded = unsafe {
        // SAFETY: `dacl` is live, index zero exists, and `ace` is writable.
        GetAce(dacl, 0, &raw mut ace)
    };
    if ace_loaded == 0 {
        return Err(io::Error::last_os_error());
    }
    let acl_bytes_in_use = usize::try_from(size.AclBytesInUse).unwrap_or(usize::MAX);
    let ace_offset = (ace as usize).checked_sub(dacl as usize).ok_or_else(|| {
        invalid_mutex_security("secured object ACE pointer precedes its containing ACL")
    })?;
    let expected_sid_length = unsafe {
        // SAFETY: `expected_sid` is the live SID returned by ConvertStringSidToSidW.
        GetLengthSid(expected_sid)
    };
    if expected_sid_length == 0 {
        return Err(invalid_mutex_security("expected SID has zero length"));
    }
    let expected_sid_length = usize::try_from(expected_sid_length).unwrap_or(usize::MAX);
    let acl_bytes = unsafe {
        // SAFETY: GetAclInformation reports `AclBytesInUse` initialized bytes for this live ACL.
        std::slice::from_raw_parts(dacl.cast::<u8>(), acl_bytes_in_use)
    };
    let expected_sid_bytes = unsafe {
        // SAFETY: GetLengthSid reports the initialized length of this live expected SID.
        std::slice::from_raw_parts(expected_sid.cast::<u8>(), expected_sid_length)
    };
    validate_current_user_ace_bytes(
        acl_bytes,
        acl_bytes_in_use,
        ace_offset,
        expected_sid_bytes,
        expected_access,
        expected_ace_flags,
    )
}

fn validate_current_user_ace_bytes(
    acl_bytes: &[u8],
    acl_bytes_in_use: usize,
    ace_offset: usize,
    expected_sid: &[u8],
    expected_access: u32,
    expected_ace_flags: u32,
) -> io::Result<()> {
    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;

    let bounded_acl = acl_bytes.get(..acl_bytes_in_use).ok_or_else(|| {
        invalid_mutex_security("ACL bytes-in-use exceeds the available ACL buffer")
    })?;
    let header_end = ace_offset
        .checked_add(std::mem::size_of::<ACE_HEADER>())
        .ok_or_else(|| invalid_mutex_security("ACE header range overflows"))?;
    let header = bounded_acl
        .get(ace_offset..header_end)
        .ok_or_else(|| invalid_mutex_security("ACE header extends past ACL bytes-in-use"))?;
    let ace_type = header[std::mem::offset_of!(ACE_HEADER, AceType)];
    let ace_flags = header[std::mem::offset_of!(ACE_HEADER, AceFlags)];
    let size_offset = std::mem::offset_of!(ACE_HEADER, AceSize);
    let ace_size = usize::from(u16::from_le_bytes([
        header[size_offset],
        header[size_offset + 1],
    ]));
    let sid_offset = std::mem::offset_of!(ACCESS_ALLOWED_ACE, SidStart);
    if ace_size < sid_offset {
        return Err(invalid_mutex_security("ACE is truncated before SidStart"));
    }
    let ace_end = ace_offset
        .checked_add(ace_size)
        .ok_or_else(|| invalid_mutex_security("ACE range overflows"))?;
    let ace_record = bounded_acl
        .get(ace_offset..ace_end)
        .ok_or_else(|| invalid_mutex_security("ACE extends past ACL bytes-in-use"))?;
    let expected_ace_size = sid_offset
        .checked_add(expected_sid.len())
        .ok_or_else(|| invalid_mutex_security("expected ACE size overflows"))?;
    if ace_size != expected_ace_size {
        return Err(invalid_mutex_security(
            "ACE size does not exactly match the expected SID length",
        ));
    }
    if ace_type != ACCESS_ALLOWED_ACE_TYPE || u32::from(ace_flags) != expected_ace_flags {
        return Err(invalid_mutex_security(
            "secured object DACL contains a non-canonical allow entry",
        ));
    }
    let mask_offset = std::mem::offset_of!(ACCESS_ALLOWED_ACE, Mask);
    let mask_end = mask_offset
        .checked_add(std::mem::size_of::<u32>())
        .ok_or_else(|| invalid_mutex_security("ACE mask range overflows"))?;
    let mask_bytes = ace_record
        .get(mask_offset..mask_end)
        .ok_or_else(|| invalid_mutex_security("ACE is truncated before its access mask"))?;
    let mask = u32::from_le_bytes(
        mask_bytes
            .try_into()
            .map_err(|_| invalid_mutex_security("ACE access mask has an invalid length"))?,
    );
    if mask != expected_access {
        return Err(invalid_mutex_security(
            "secured object DACL contains a non-canonical allow entry",
        ));
    }
    let trustee = ace_record
        .get(sid_offset..)
        .ok_or_else(|| invalid_mutex_security("ACE is truncated before its trustee SID"))?;
    if trustee != expected_sid {
        return Err(invalid_mutex_security("unexpected secured object trustee"));
    }
    Ok(())
}

fn invalid_mutex_security(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
fn kernel_mutex_owner_sid(handle: HANDLE) -> io::Result<String> {
    let descriptor = kernel_security_descriptor(handle, SE_KERNEL_OBJECT)?;
    let mut owner: PSID = ptr::null_mut();
    let mut owner_defaulted = 0;
    let loaded = unsafe {
        // SAFETY: The descriptor is live and the output pointers are writable.
        GetSecurityDescriptorOwner(descriptor.pointer, &raw mut owner, &raw mut owner_defaulted)
    };
    if loaded == 0 || owner.is_null() {
        return Err(io::Error::last_os_error());
    }
    let mut encoded = ptr::null_mut();
    let converted = unsafe {
        // SAFETY: `owner` is a live SID and `encoded` receives LocalAlloc-owned UTF-16 text.
        ConvertSidToStringSidW(owner, &raw mut encoded)
    };
    if converted == 0 {
        return Err(io::Error::last_os_error());
    }
    let _encoded = LocalAllocation(encoded.cast());
    let length = (0..256)
        .find(|index| unsafe {
            // SAFETY: ConvertSidToStringSidW returns a short NUL-terminated SID string; the
            // bounded scan refuses an invalid overlong result.
            *encoded.add(*index) == 0
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "SID text is not terminated"))?;
    let units = unsafe {
        // SAFETY: The bounded scan established `length` initialized code units before NUL.
        std::slice::from_raw_parts(encoded, length)
    };
    String::from_utf16(units).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
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

#[cfg(test)]
mod tests {
    use std::{
        ffi::OsString,
        io,
        path::Path,
        sync::atomic::{AtomicU64, Ordering},
        time::Duration,
    };

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_FAILED, WAIT_OBJECT_0};
    use windows_sys::Win32::{
        Security::{ACCESS_ALLOWED_ACE, ACE_HEADER},
        System::Threading::MUTEX_ALL_ACCESS,
    };

    use super::{
        LocalAllocation, acquire_current_user_mutex, create_and_verify_current_user_directory,
        encode_path, ensure_current_user_only_directory_tree, kernel_mutex_owner_sid,
        validate_current_user_ace_bytes, wait_status_result,
    };

    const TEST_ACE_OFFSET: usize = 8;
    const TEST_SID: [u8; 12] = [1, 1, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0];

    fn exact_test_ace() -> Vec<u8> {
        let sid_offset = std::mem::offset_of!(ACCESS_ALLOWED_ACE, SidStart);
        let ace_size = sid_offset + TEST_SID.len();
        let mut bytes = vec![0_u8; TEST_ACE_OFFSET + ace_size];
        bytes[TEST_ACE_OFFSET + std::mem::offset_of!(ACE_HEADER, AceType)] = 0;
        bytes[TEST_ACE_OFFSET + std::mem::offset_of!(ACE_HEADER, AceFlags)] = 0;
        bytes[TEST_ACE_OFFSET + std::mem::offset_of!(ACE_HEADER, AceSize)
            ..TEST_ACE_OFFSET + std::mem::offset_of!(ACE_HEADER, AceSize) + 2]
            .copy_from_slice(&u16::try_from(ace_size).unwrap_or(u16::MAX).to_le_bytes());
        bytes[TEST_ACE_OFFSET + std::mem::offset_of!(ACCESS_ALLOWED_ACE, Mask)
            ..TEST_ACE_OFFSET + std::mem::offset_of!(ACCESS_ALLOWED_ACE, Mask) + 4]
            .copy_from_slice(&MUTEX_ALL_ACCESS.to_le_bytes());
        bytes[TEST_ACE_OFFSET + sid_offset..].copy_from_slice(&TEST_SID);
        bytes
    }

    fn set_test_ace_size(bytes: &mut [u8], size: usize) {
        let offset = TEST_ACE_OFFSET + std::mem::offset_of!(ACE_HEADER, AceSize);
        bytes[offset..offset + 2]
            .copy_from_slice(&u16::try_from(size).unwrap_or(u16::MAX).to_le_bytes());
    }

    #[test]
    fn bounded_ace_verifier_accepts_valid_exact_buffer() -> io::Result<()> {
        let bytes = exact_test_ace();
        validate_current_user_ace_bytes(
            &bytes,
            bytes.len(),
            TEST_ACE_OFFSET,
            &TEST_SID,
            MUTEX_ALL_ACCESS,
            0,
        )
    }

    #[test]
    fn bounded_ace_verifier_rejects_truncated_sid_start() {
        let mut bytes = exact_test_ace();
        set_test_ace_size(
            &mut bytes,
            std::mem::offset_of!(ACCESS_ALLOWED_ACE, SidStart) - 1,
        );

        assert!(
            validate_current_user_ace_bytes(
                &bytes,
                bytes.len(),
                TEST_ACE_OFFSET,
                &TEST_SID,
                MUTEX_ALL_ACCESS,
                0,
            )
            .is_err()
        );
    }

    #[test]
    fn bounded_ace_verifier_rejects_ace_extending_past_acl_bytes_in_use() {
        let bytes = exact_test_ace();

        assert!(
            validate_current_user_ace_bytes(
                &bytes,
                bytes.len() - 1,
                TEST_ACE_OFFSET,
                &TEST_SID,
                MUTEX_ALL_ACCESS,
                0,
            )
            .is_err()
        );
    }

    #[test]
    fn bounded_ace_verifier_rejects_overlong_sid() {
        let mut bytes = exact_test_ace();
        bytes.push(0);
        set_test_ace_size(
            &mut bytes,
            std::mem::offset_of!(ACCESS_ALLOWED_ACE, SidStart) + TEST_SID.len() + 1,
        );

        assert!(
            validate_current_user_ace_bytes(
                &bytes,
                bytes.len(),
                TEST_ACE_OFFSET,
                &TEST_SID,
                MUTEX_ALL_ACCESS,
                0,
            )
            .is_err()
        );
    }

    #[test]
    fn bounded_ace_verifier_rejects_short_sid() {
        let mut bytes = exact_test_ace();
        set_test_ace_size(
            &mut bytes,
            std::mem::offset_of!(ACCESS_ALLOWED_ACE, SidStart) + TEST_SID.len() - 1,
        );

        assert!(
            validate_current_user_ace_bytes(
                &bytes,
                bytes.len(),
                TEST_ACE_OFFSET,
                &TEST_SID,
                MUTEX_ALL_ACCESS,
                0,
            )
            .is_err()
        );
    }

    #[test]
    fn bounded_ace_verifier_rejects_wrong_ace_type() {
        let mut bytes = exact_test_ace();
        bytes[TEST_ACE_OFFSET + std::mem::offset_of!(ACE_HEADER, AceType)] = 1;

        assert!(
            validate_current_user_ace_bytes(
                &bytes,
                bytes.len(),
                TEST_ACE_OFFSET,
                &TEST_SID,
                MUTEX_ALL_ACCESS,
                0,
            )
            .is_err()
        );
    }

    fn create_test_directory(path: &Path, sddl: &str) -> io::Result<()> {
        let encoded_sddl = sddl
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let mut descriptor = std::ptr::null_mut();
        let converted = unsafe {
            // SAFETY: The SDDL is NUL-terminated and the output receives LocalAlloc-owned memory.
            windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW(
                encoded_sddl.as_ptr(),
                windows_sys::Win32::Security::Authorization::SDDL_REVISION_1,
                &raw mut descriptor,
                std::ptr::null_mut(),
            )
        };
        if converted == 0 {
            return Err(io::Error::last_os_error());
        }
        let _descriptor = LocalAllocation(descriptor.cast());
        let attributes = windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
            nLength: u32::try_from(std::mem::size_of::<
                windows_sys::Win32::Security::SECURITY_ATTRIBUTES,
            >())
            .unwrap_or(u32::MAX),
            lpSecurityDescriptor: descriptor.cast(),
            bInheritHandle: 0,
        };
        let encoded_path = encode_path(path)?;
        let created = unsafe {
            // SAFETY: The path and security descriptor remain live for the synchronous call.
            windows_sys::Win32::Storage::FileSystem::CreateDirectoryW(
                encoded_path.as_ptr(),
                &raw const attributes,
            )
        };
        if created == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    struct TestMutex {
        handle: HANDLE,
        owned: bool,
    }

    impl TestMutex {
        fn create(name: &OsString, sddl: &str, owned: bool) -> io::Result<Self> {
            let encoded_sddl = sddl
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect::<Vec<_>>();
            let mut descriptor = std::ptr::null_mut();
            // SAFETY: The SDDL is NUL-terminated and the output pointer is writable.
            let converted = unsafe {
                windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    encoded_sddl.as_ptr(),
                    windows_sys::Win32::Security::Authorization::SDDL_REVISION_1,
                    &raw mut descriptor,
                    std::ptr::null_mut(),
                )
            };
            if converted == 0 {
                return Err(io::Error::last_os_error());
            }
            let _descriptor = LocalAllocation(descriptor.cast());
            let mut attributes = windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
                nLength: u32::try_from(std::mem::size_of::<
                    windows_sys::Win32::Security::SECURITY_ATTRIBUTES,
                >())
                .unwrap_or(u32::MAX),
                lpSecurityDescriptor: descriptor.cast(),
                bInheritHandle: 0,
            };
            let encoded_name = std::os::windows::ffi::OsStrExt::encode_wide(name.as_os_str())
                .chain(std::iter::once(0))
                .collect::<Vec<_>>();
            // SAFETY: The attributes and NUL-terminated name are live for the call.
            let handle = unsafe {
                windows_sys::Win32::System::Threading::CreateMutexW(
                    &raw mut attributes,
                    i32::from(owned),
                    encoded_name.as_ptr(),
                )
            };
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { handle, owned })
        }
    }

    impl Drop for TestMutex {
        fn drop(&mut self) {
            // SAFETY: The test guard owns this live mutex handle and optional initial ownership.
            unsafe {
                if self.owned {
                    let _ = windows_sys::Win32::System::Threading::ReleaseMutex(self.handle);
                }
                let _ = CloseHandle(self.handle);
            }
        }
    }

    fn mutex_name(label: &str) -> OsString {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        OsString::from(format!(
            r"Local\ColayMutexTest-{}-{label}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn current_sid() -> io::Result<String> {
        let seed_name = mutex_name("current-sid");
        let seed = TestMutex::create(&seed_name, "D:P(A;;GA;;;WD)", false)?;
        kernel_mutex_owner_sid(seed.handle)
    }

    #[test]
    fn newly_created_current_user_mutex_passes_validation() -> io::Result<()> {
        let current_sid = current_sid()?;
        let mutex = acquire_current_user_mutex(mutex_name("valid-new"), &current_sid)?;
        drop(mutex);
        Ok(())
    }

    #[test]
    fn secure_directory_tree_creates_every_missing_component() -> io::Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().join("one/two/three");
        let current_sid = current_sid()?;

        let _tree = ensure_current_user_only_directory_tree(&root, &current_sid)?;

        assert!(root.is_dir());
        Ok(())
    }

    #[test]
    fn verified_existing_directory_is_accepted_after_create_race() -> io::Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().join("concurrent");
        let current_sid = current_sid()?;
        let first = create_and_verify_current_user_directory(&root, &current_sid)?;

        let second = create_and_verify_current_user_directory(&root, &current_sid)?;

        drop((first, second));
        Ok(())
    }

    #[test]
    fn permissive_existing_directory_is_rejected_after_create_race() -> io::Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().join("permissive");
        let current_sid = current_sid()?;
        let sddl = format!("O:{current_sid}D:P(A;OICI;GA;;;{current_sid})(A;OICI;GA;;;WD)");
        create_test_directory(&root, &sddl)?;

        let error = create_and_verify_current_user_directory(&root, &current_sid)
            .err()
            .ok_or_else(|| io::Error::other("permissive existing directory was accepted"))?;

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("exactly one"));
        Ok(())
    }

    #[test]
    fn wrong_owner_directory_is_rejected_after_create_race() -> io::Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().join("wrong-owner");
        create_test_directory(&root, "D:P(A;OICI;GA;;;WD)")?;

        let error = create_and_verify_current_user_directory(&root, "S-1-1-0")
            .err()
            .ok_or_else(|| io::Error::other("wrong-owner existing directory was accepted"))?;

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("owner"));
        Ok(())
    }

    #[test]
    fn unexpected_wait_status_does_not_read_stale_last_error() -> io::Result<()> {
        let mut read_last_error = false;
        let error = wait_status_result(7, || {
            read_last_error = true;
            io::Error::from_raw_os_error(123)
        })
        .err()
        .ok_or_else(|| io::Error::other("unexpected wait status succeeded"))?;

        assert!(!read_last_error);
        assert!(error.to_string().contains("unexpected mutex wait status 7"));

        let error = wait_status_result(WAIT_FAILED, || {
            read_last_error = true;
            io::Error::from_raw_os_error(123)
        })
        .err()
        .ok_or_else(|| io::Error::other("WAIT_FAILED unexpectedly succeeded"))?;
        assert!(read_last_error);
        assert_eq!(error.raw_os_error(), Some(123));
        assert!(wait_status_result(WAIT_OBJECT_0, io::Error::last_os_error).is_ok());
        Ok(())
    }

    #[test]
    fn held_permissive_mutex_is_rejected_before_wait() -> io::Result<()> {
        let current_sid = current_sid()?;

        let name = mutex_name("permissive");
        let sddl = format!("O:{current_sid}D:P(A;;GA;;;{current_sid})(A;;GA;;;WD)");
        let held = TestMutex::create(&name, &sddl, true)?;
        let (sender, receiver) = std::sync::mpsc::channel();
        let acquire_name = name.clone();
        let acquire_sid = current_sid.clone();
        let waiter = std::thread::spawn(move || {
            let result = acquire_current_user_mutex(acquire_name, &acquire_sid)
                .map(|_| ())
                .map_err(|error| (error.kind(), error.to_string()));
            let _ = sender.send(result);
        });

        let result = receiver.recv_timeout(Duration::from_secs(2)).map_err(|_| {
            io::Error::other("permissive mutex validation waited on the foreign-held object")
        })?;
        assert!(result.is_err_and(|(kind, message)| {
            kind == io::ErrorKind::InvalidData && message.contains("exactly one")
        }));
        drop(held);
        waiter
            .join()
            .map_err(|_| io::Error::other("waiter panicked"))?;
        Ok(())
    }

    #[test]
    fn wrong_owner_mutex_is_rejected_before_wait() -> io::Result<()> {
        let name = mutex_name("wrong-owner");
        let held = TestMutex::create(&name, "D:P(A;;GA;;;WD)", true)?;
        let (sender, receiver) = std::sync::mpsc::channel();
        let acquire_name = name.clone();
        let waiter = std::thread::spawn(move || {
            let result = acquire_current_user_mutex(acquire_name, "S-1-1-0")
                .map(|_| ())
                .map_err(|error| (error.kind(), error.to_string()));
            let _ = sender.send(result);
        });

        let result = receiver.recv_timeout(Duration::from_secs(2)).map_err(|_| {
            io::Error::other("wrong-owner validation waited on the foreign-held object")
        })?;
        assert!(result.is_err_and(|(kind, message)| {
            kind == io::ErrorKind::InvalidData && message.contains("owner")
        }));
        drop(held);
        waiter
            .join()
            .map_err(|_| io::Error::other("waiter panicked"))?;
        Ok(())
    }

    #[test]
    fn restrictive_existing_mutex_fails_closed() -> io::Result<()> {
        let current_sid = current_sid()?;

        let name = mutex_name("restrictive");
        let sddl = format!("O:{current_sid}D:P(A;;0x00100000;;;{current_sid})");
        let _mutex = TestMutex::create(&name, &sddl, false)?;

        let Err(error) = acquire_current_user_mutex(name, &current_sid) else {
            return Err(io::Error::other(
                "restricted existing mutex unexpectedly opened with full access",
            ));
        };
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        Ok(())
    }
}
