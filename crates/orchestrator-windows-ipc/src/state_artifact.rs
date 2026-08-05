use std::{
    ffi::c_void, io, mem::size_of, os::windows::ffi::OsStrExt as _, path::Path, ptr, slice,
    sync::Mutex,
};

use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, INVALID_HANDLE_VALUE, LocalFree},
    Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_REVISION, ACL_SIZE_INFORMATION,
        AclSizeInformation, AddAccessAllowedAceEx,
        Authorization::{GetSecurityInfo, SE_FILE_OBJECT, SetSecurityInfo},
        CONTAINER_INHERIT_ACE, CreateWellKnownSid, DACL_SECURITY_INFORMATION, GetAclInformation,
        GetLengthSid, GetSecurityDescriptorControl, GetSecurityDescriptorDacl,
        GetSecurityDescriptorLength, GetSecurityDescriptorOwner, InitializeAcl, IsValidAcl,
        IsValidSid, OBJECT_INHERIT_ACE, OWNER_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED,
        SE_SELF_RELATIVE, SECURITY_MAX_SID_SIZE, WinBuiltinAdministratorsSid, WinLocalSystemSid,
    },
    Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ALL_ACCESS, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle, OPEN_EXISTING, READ_CONTROL,
        WRITE_DAC,
    },
    System::SystemServices::ACCESS_ALLOWED_ACE_TYPE,
};

use crate::process_identity::current_process_user;

const MAX_SECURITY_DESCRIPTOR_BYTES: usize = 128 * 1024;
static STATE_ARTIFACT_REPAIR: Mutex<()> = Mutex::new(());

#[cfg(test)]
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[cfg(test)]
static SET_SECURITY_INFO_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static FORCE_POST_WRITE_FAILURE: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static FORCE_DESCRIPTOR_STRUCTURE_FAILURE: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StateArtifactKind {
    File,
    Directory,
}

struct AlignedBuffer {
    words: Box<[usize]>,
    byte_len: usize,
}

impl AlignedBuffer {
    fn zeroed(byte_len: usize) -> io::Result<Self> {
        if byte_len == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "aligned buffer length must be nonzero",
            ));
        }
        let width = size_of::<usize>();
        let words = byte_len.checked_add(width - 1).ok_or_else(bounds_error)? / width;
        Ok(Self {
            words: vec![0; words].into_boxed_slice(),
            byte_len,
        })
    }

    fn as_bytes(&self) -> &[u8] {
        // SAFETY: `words` is initialized and suitably aligned; `byte_len` is bounded by the
        // rounded-up allocation performed in `zeroed`.
        unsafe { slice::from_raw_parts(self.words.as_ptr().cast(), self.byte_len) }
    }

    fn as_mut_ptr<T>(&mut self) -> *mut T {
        self.words.as_mut_ptr().cast()
    }
}

struct OwnedSid {
    storage: AlignedBuffer,
    length: usize,
}

impl OwnedSid {
    fn as_bytes(&self) -> &[u8] {
        &self.storage.as_bytes()[..self.length]
    }
}

struct OwnedAcl {
    storage: AlignedBuffer,
}

impl OwnedAcl {
    fn as_ptr(&self) -> *const ACL {
        self.storage.words.as_ptr().cast()
    }
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: This guard owns a successful, non-pseudo `CreateFileW` result.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

struct LocalDescriptor(PSECURITY_DESCRIPTOR);

impl Drop for LocalDescriptor {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `GetSecurityInfo` returned this LocalAlloc-owned descriptor to this guard.
            unsafe {
                let _ = LocalFree(self.0.cast());
            }
        }
    }
}

struct VerifiedDescriptor {
    owner_sid: Box<[u8]>,
}

struct DescriptorState {
    verified: VerifiedDescriptor,
    acl_result: io::Result<()>,
}

pub fn ensure_private_state_artifact(path: &Path, kind: StateArtifactKind) -> io::Result<()> {
    let _repair = STATE_ARTIFACT_REPAIR
        .lock()
        .map_err(|_| io::Error::other("state-artifact ACL repair lock was poisoned"))?;
    let principals = expected_principals()?;
    let handle = open_target(path, kind, true)?;
    let before = read_descriptor_state(handle.0, kind, &principals)
        .map_err(|error| stage_error("descriptor read", &error))?;
    if before.acl_result.is_ok() {
        return Ok(());
    }

    let acl = OwnedAcl::build(kind, &principals)
        .map_err(|error| stage_error("ACL construction", &error))?;
    install_acl(handle.0, &acl).map_err(|error| stage_error("descriptor write", &error))?;
    let after = read_descriptor_state(handle.0, kind, &principals)
        .map_err(|error| stage_error("post-write verification", &error))?;
    after
        .acl_result
        .map_err(|error| stage_error("post-write verification", &error))?;
    if before.verified.owner_sid != after.verified.owner_sid {
        return Err(stage_invalid_data(
            "post-write verification",
            "artifact owner changed during DACL repair",
        ));
    }
    #[cfg(test)]
    if FORCE_POST_WRITE_FAILURE.load(Ordering::SeqCst) {
        return Err(stage_invalid_data(
            "post-write verification",
            "forced verification mismatch",
        ));
    }
    Ok(())
}

pub fn verify_private_state_artifact(path: &Path, kind: StateArtifactKind) -> io::Result<()> {
    let principals = expected_principals()?;
    let handle = open_target(path, kind, false)?;
    let descriptor = read_descriptor_state(handle.0, kind, &principals)
        .map_err(|error| stage_error("descriptor read", &error))?;
    descriptor
        .acl_result
        .map_err(|error| stage_error("descriptor read", &error))
}

fn expected_principals() -> io::Result<OwnedExpectedPrincipals<'static>> {
    let user = current_process_user()?;
    let system = create_well_known_sid(WinLocalSystemSid)
        .map_err(|error| stage_error("ACL construction", &error))?;
    let administrators = create_well_known_sid(WinBuiltinAdministratorsSid)
        .map_err(|error| stage_error("ACL construction", &error))?;
    Ok(OwnedExpectedPrincipals {
        user: user.sid_bytes(),
        system,
        administrators,
    })
}

struct OwnedExpectedPrincipals<'a> {
    user: &'a [u8],
    system: OwnedSid,
    administrators: OwnedSid,
}

impl OwnedExpectedPrincipals<'_> {
    fn borrowed(&self) -> ExpectedPrincipals<'_> {
        ExpectedPrincipals {
            user: self.user,
            system: self.system.as_bytes(),
            administrators: self.administrators.as_bytes(),
        }
    }
}

struct ExpectedPrincipals<'a> {
    user: &'a [u8],
    system: &'a [u8],
    administrators: &'a [u8],
}

enum AclCheckError {
    Structural(io::Error),
    Policy(io::Error),
}

impl AclCheckError {
    fn into_io(self) -> io::Error {
        match self {
            Self::Structural(error) | Self::Policy(error) => error,
        }
    }
}

fn verify_acl_bytes(
    acl: Option<&[u8]>,
    protected: bool,
    kind: StateArtifactKind,
    principals: &ExpectedPrincipals<'_>,
) -> io::Result<()> {
    check_acl_bytes(acl, protected, kind, principals).map_err(AclCheckError::into_io)
}

fn check_acl_bytes(
    acl: Option<&[u8]>,
    protected: bool,
    kind: StateArtifactKind,
    principals: &ExpectedPrincipals<'_>,
) -> Result<(), AclCheckError> {
    if !protected {
        return Err(policy_acl("DACL is not protected"));
    }
    let bytes = acl.ok_or_else(|| policy_acl("DACL must be present and non-null"))?;
    if bytes.len() < size_of::<ACL>() {
        return Err(structural_acl("ACL header is truncated"));
    }

    let declared_acl_size =
        read_u16(bytes, 2, "ACL size field is truncated").map_err(AclCheckError::Structural)?;
    if usize::from(declared_acl_size) < bytes.len() {
        return Err(structural_acl(
            "ACL contains trailing bytes after its declared size",
        ));
    }
    if usize::from(declared_acl_size) > bytes.len() {
        return Err(structural_acl("ACL size does not match bytes in use"));
    }
    let ace_count = read_u16(bytes, 4, "ACL ACE-count field is truncated")
        .map_err(AclCheckError::Structural)?;
    if ace_count != 3 {
        return Err(policy_acl("DACL must contain exactly three trustee ACEs"));
    }

    let required_flags = match kind {
        StateArtifactKind::File => 0,
        StateArtifactKind::Directory => u8::try_from(OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE)
            .map_err(|_| structural_acl("directory ACE flags overflow"))?,
    };
    let mut found = [false; 3];
    let mut cursor = size_of::<ACL>();
    for _ in 0..usize::from(ace_count) {
        let (ace_end, principal) = verify_ace_record(bytes, cursor, required_flags, principals)?;
        if found[principal] {
            return Err(policy_acl("DACL contains a duplicate trustee"));
        }
        found[principal] = true;
        cursor = ace_end;
    }

    if cursor != bytes.len() {
        return Err(structural_acl("ACL contains trailing bytes after its ACEs"));
    }
    if !found.into_iter().all(|present| present) {
        return Err(policy_acl("DACL is missing a required trustee"));
    }
    Ok(())
}

fn verify_ace_record(
    bytes: &[u8],
    cursor: usize,
    required_flags: u8,
    principals: &ExpectedPrincipals<'_>,
) -> Result<(usize, usize), AclCheckError> {
    if !cursor.is_multiple_of(size_of::<u32>()) {
        return Err(structural_acl("ACE address is not DWORD-aligned"));
    }
    let header_end = cursor
        .checked_add(size_of::<ACE_HEADER>())
        .ok_or_else(|| structural_acl("ACE header range overflow"))?;
    let header = bytes
        .get(cursor..header_end)
        .ok_or_else(|| structural_acl("ACE header extends past ACL bytes in use"))?;
    let allowed_type = u8::try_from(ACCESS_ALLOWED_ACE_TYPE)
        .map_err(|_| structural_acl("allow ACE type does not fit in its header"))?;
    if header[0] != allowed_type {
        return Err(policy_acl("DACL contains a non-allow ACE"));
    }
    if header[1] != required_flags {
        return Err(policy_acl("ACE flags do not match the artifact kind"));
    }
    let record_size = usize::from(u16::from_le_bytes([header[2], header[3]]));
    let sid_offset = size_of::<ACCESS_ALLOWED_ACE>()
        .checked_sub(size_of::<u32>())
        .ok_or_else(|| structural_acl("ACE SidStart offset underflow"))?;
    if record_size < sid_offset {
        return Err(structural_acl("ACE is truncated before SidStart"));
    }
    let record_end = cursor
        .checked_add(record_size)
        .ok_or_else(|| structural_acl("ACE range overflow"))?;
    let ace = bytes
        .get(cursor..record_end)
        .ok_or_else(|| structural_acl("ACE range extends past ACL bytes in use"))?;
    if !record_size.is_multiple_of(size_of::<u32>()) {
        return Err(structural_acl("ACE size is not DWORD-aligned"));
    }
    if read_u32(ace, 4, "ACE is truncated before its access mask")
        .map_err(AclCheckError::Structural)?
        != FILE_ALL_ACCESS
    {
        return Err(policy_acl("ACE access mask is not exact full control"));
    }

    let sid = ace
        .get(sid_offset..)
        .ok_or_else(|| structural_acl("ACE is truncated before its trustee SID"))?;
    if sid.len() < 8 {
        return Err(structural_acl("trustee SID header is truncated"));
    }
    let sid_length = usize::from(sid[1])
        .checked_mul(size_of::<u32>())
        .and_then(|bytes| 8_usize.checked_add(bytes))
        .ok_or_else(|| structural_acl("trustee SID length overflow"))?;
    if sid_length > sid.len() {
        return Err(structural_acl(
            "trustee SID extends past its containing ACE",
        ));
    }
    if sid_length != sid.len() {
        return Err(structural_acl(
            "trustee SID must end exactly at the ACE end",
        ));
    }
    validate_sid_prefix(sid).map_err(AclCheckError::Structural)?;

    let principal = if sid == principals.user {
        0
    } else if sid == principals.system {
        1
    } else if sid == principals.administrators {
        2
    } else {
        return Err(policy_acl("DACL contains an unexpected trustee"));
    };
    Ok((record_end, principal))
}

fn structural_acl(detail: &'static str) -> AclCheckError {
    AclCheckError::Structural(invalid_acl(detail))
}

fn policy_acl(detail: &'static str) -> AclCheckError {
    AclCheckError::Policy(invalid_acl(detail))
}

fn read_u16(bytes: &[u8], offset: usize, detail: &'static str) -> io::Result<u16> {
    let end = offset
        .checked_add(size_of::<u16>())
        .ok_or_else(|| invalid_acl(detail))?;
    let field = bytes.get(offset..end).ok_or_else(|| invalid_acl(detail))?;
    Ok(u16::from_le_bytes([field[0], field[1]]))
}

fn read_u32(bytes: &[u8], offset: usize, detail: &'static str) -> io::Result<u32> {
    let end = offset
        .checked_add(size_of::<u32>())
        .ok_or_else(|| invalid_acl(detail))?;
    let field = bytes.get(offset..end).ok_or_else(|| invalid_acl(detail))?;
    Ok(u32::from_le_bytes([field[0], field[1], field[2], field[3]]))
}

fn invalid_acl(detail: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, detail)
}

fn create_well_known_sid(kind: i32) -> io::Result<OwnedSid> {
    let capacity = usize::try_from(SECURITY_MAX_SID_SIZE).map_err(|_| bounds_error())?;
    let mut storage = AlignedBuffer::zeroed(capacity)?;
    let mut length = SECURITY_MAX_SID_SIZE;
    // SAFETY: `storage` is initialized and SID-aligned with `length` writable bytes; a null domain
    // SID requests the machine-independent well-known SID selected by `kind`.
    let created = unsafe {
        CreateWellKnownSid(
            kind,
            ptr::null_mut(),
            storage.as_mut_ptr::<c_void>(),
            &raw mut length,
        )
    };
    if created == 0 {
        return Err(io::Error::last_os_error());
    }
    let length = usize::try_from(length).map_err(|_| bounds_error())?;
    if length > capacity {
        return Err(invalid_acl(
            "well-known SID length exceeds its bounded storage",
        ));
    }
    validate_sid_prefix(&storage.as_bytes()[..length])?;
    Ok(OwnedSid { storage, length })
}

fn validate_sid_prefix(sid: &[u8]) -> io::Result<()> {
    if sid.len() < 8 {
        return Err(invalid_acl("SID header is truncated"));
    }
    let structural = usize::from(sid[1])
        .checked_mul(size_of::<u32>())
        .and_then(|bytes| 8_usize.checked_add(bytes))
        .ok_or_else(bounds_error)?;
    if structural != sid.len() {
        return Err(invalid_acl(
            "SID length does not match its bounded structure",
        ));
    }
    let pointer = sid.as_ptr().cast_mut().cast();
    // SAFETY: The complete structure implied by the bounded SID header lies in `sid`.
    if unsafe { IsValidSid(pointer) } == 0 {
        return Err(invalid_acl("Windows rejected the bounded SID"));
    }
    // SAFETY: Windows accepted the complete in-bounds SID and its storage remains live.
    let windows_length = unsafe { GetLengthSid(pointer) };
    if usize::try_from(windows_length).ok() != Some(structural) {
        return Err(invalid_acl(
            "Windows SID length disagrees with bounded bytes",
        ));
    }
    Ok(())
}

impl OwnedAcl {
    fn build(
        kind: StateArtifactKind,
        principals: &OwnedExpectedPrincipals<'_>,
    ) -> io::Result<Self> {
        let principals = principals.borrowed();
        for sid in [
            principals.user,
            principals.system,
            principals.administrators,
        ] {
            validate_sid_prefix(sid)?;
        }
        let sid_offset = size_of::<ACCESS_ALLOWED_ACE>()
            .checked_sub(size_of::<u32>())
            .ok_or_else(bounds_error)?;
        let mut ace_bytes = 0_usize;
        for sid in [
            principals.user,
            principals.system,
            principals.administrators,
        ] {
            let ace_len = sid_offset.checked_add(sid.len()).ok_or_else(bounds_error)?;
            ace_bytes = ace_bytes.checked_add(ace_len).ok_or_else(bounds_error)?;
        }
        let acl_len = size_of::<ACL>()
            .checked_add(ace_bytes)
            .ok_or_else(bounds_error)?;
        if acl_len > usize::from(u16::MAX) {
            return Err(bounds_error());
        }
        let acl_len_u32 = u32::try_from(acl_len).map_err(|_| bounds_error())?;
        let mut storage = AlignedBuffer::zeroed(acl_len)?;
        // SAFETY: `storage` is ACL-aligned, initialized, writable, and exactly `acl_len` bytes.
        if unsafe { InitializeAcl(storage.as_mut_ptr(), acl_len_u32, ACL_REVISION) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let flags = match kind {
            StateArtifactKind::File => 0,
            StateArtifactKind::Directory => OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
        };
        for sid in [
            principals.user,
            principals.system,
            principals.administrators,
        ] {
            // SAFETY: The ACL has exact checked capacity for all three ACEs. Every SID is complete,
            // validated, and remains live through this synchronous call.
            let added = unsafe {
                AddAccessAllowedAceEx(
                    storage.as_mut_ptr(),
                    ACL_REVISION,
                    flags,
                    FILE_ALL_ACCESS,
                    sid.as_ptr().cast_mut().cast(),
                )
            };
            if added == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        // SAFETY: The ACL was initialized and populated only by checked Windows ACL APIs.
        if unsafe { IsValidAcl(storage.words.as_ptr().cast()) } == 0 {
            return Err(invalid_acl("Windows rejected the constructed ACL"));
        }
        let mut size = ACL_SIZE_INFORMATION::default();
        // SAFETY: The ACL is live and `size` is writable for its declared structure length.
        let loaded = unsafe {
            GetAclInformation(
                storage.words.as_ptr().cast_mut().cast(),
                (&raw mut size).cast(),
                u32::try_from(size_of::<ACL_SIZE_INFORMATION>()).map_err(|_| bounds_error())?,
                AclSizeInformation,
            )
        };
        if loaded == 0 {
            return Err(io::Error::last_os_error());
        }
        if usize::try_from(size.AclBytesInUse).ok() != Some(acl_len) {
            return Err(invalid_acl(
                "constructed ACL bytes in use do not match its bounded allocation",
            ));
        }
        verify_acl_bytes(Some(storage.as_bytes()), true, kind, &principals)?;
        Ok(Self { storage })
    }
}

fn open_target(path: &Path, kind: StateArtifactKind, writable: bool) -> io::Result<OwnedHandle> {
    let mut encoded = path.as_os_str().encode_wide().collect::<Vec<_>>();
    if encoded.contains(&0) {
        return Err(stage_invalid_data(
            "target open",
            "artifact path contains an interior NUL",
        ));
    }
    encoded.push(0);
    let access = READ_CONTROL | if writable { WRITE_DAC } else { 0 };
    let flags = FILE_FLAG_OPEN_REPARSE_POINT
        | match kind {
            StateArtifactKind::File => 0,
            StateArtifactKind::Directory => FILE_FLAG_BACKUP_SEMANTICS,
        };
    // SAFETY: The path is live and NUL-terminated. Null security/template pointers make the
    // returned handle non-inheritable; no delete sharing pins the opened identity.
    let handle = unsafe {
        CreateFileW(
            encoded.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            flags,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(stage_error("target open", &io::Error::last_os_error()));
    }
    let handle = OwnedHandle(handle);
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: The retained handle is live and `information` is a writable fixed-size output.
    if unsafe { GetFileInformationByHandle(handle.0, &raw mut information) } == 0 {
        return Err(stage_error(
            "object-kind validation",
            &io::Error::last_os_error(),
        ));
    }
    if information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(stage_invalid_data(
            "object-kind validation",
            "artifact is a reparse point",
        ));
    }
    let is_directory = information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    let kind_matches = matches!(kind, StateArtifactKind::Directory) == is_directory;
    if !kind_matches {
        return Err(stage_invalid_data(
            "object-kind validation",
            "artifact kind does not match the requested kind",
        ));
    }
    Ok(handle)
}

fn read_descriptor_state(
    handle: HANDLE,
    kind: StateArtifactKind,
    principals: &OwnedExpectedPrincipals<'_>,
) -> io::Result<DescriptorState> {
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: The handle is retained and live; all optional component outputs are null and the
    // descriptor output receives one LocalAlloc-owned self-relative descriptor.
    let status = unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
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
    if descriptor.is_null() {
        return Err(invalid_acl("Windows returned a null security descriptor"));
    }
    let descriptor = LocalDescriptor(descriptor);
    // SAFETY: `descriptor` owns the live descriptor returned by `GetSecurityInfo`; this call reads
    // its descriptor header and returns the descriptor's logical byte length.
    let descriptor_length = unsafe { GetSecurityDescriptorLength(descriptor.0) };
    let descriptor_length = usize::try_from(descriptor_length).map_err(|_| bounds_error())?;
    if !(20..=MAX_SECURITY_DESCRIPTOR_BYTES).contains(&descriptor_length) {
        return Err(invalid_acl(
            "security descriptor length is outside the accepted bounds",
        ));
    }
    // SAFETY: `GetSecurityInfo` returns a LocalAlloc-owned buffer containing the complete security
    // descriptor. Its bounded logical length is readable while `descriptor` owns that buffer; the
    // bytes remain opaque until control and component-pointer validation below.
    let bytes = unsafe { slice::from_raw_parts(descriptor.0.cast::<u8>(), descriptor_length) };
    let base = bytes.as_ptr() as usize;

    let mut control = 0_u16;
    let mut revision = 0_u32;
    // SAFETY: The descriptor is live and both fixed-size outputs are writable.
    if unsafe { GetSecurityDescriptorControl(descriptor.0, &raw mut control, &raw mut revision) }
        == 0
    {
        return Err(io::Error::last_os_error());
    }
    if control & SE_SELF_RELATIVE == 0 {
        return Err(invalid_acl("security descriptor is not self-relative"));
    }
    let protected = control & SE_DACL_PROTECTED != 0;

    let mut owner: PSID = ptr::null_mut();
    let mut owner_defaulted = 0;
    // SAFETY: The descriptor is live and the outputs are writable.
    if unsafe { GetSecurityDescriptorOwner(descriptor.0, &raw mut owner, &raw mut owner_defaulted) }
        == 0
    {
        return Err(io::Error::last_os_error());
    }
    if owner.is_null() {
        return Err(invalid_acl("security descriptor has no owner SID"));
    }
    let owner_offset = pointer_offset(owner.cast(), base, bytes.len(), "owner SID")?;
    let owner_tail = bytes
        .get(owner_offset..)
        .ok_or_else(|| invalid_acl("owner SID pointer is outside the descriptor"))?;
    let owner_length = bounded_sid_length(owner_tail, owner)?;
    let owner_sid = owner_tail[..owner_length].to_vec().into_boxed_slice();

    let mut dacl_present = 0;
    let mut dacl: *mut ACL = ptr::null_mut();
    let mut dacl_defaulted = 0;
    // SAFETY: The descriptor is live and all fixed-size outputs are writable.
    if unsafe {
        GetSecurityDescriptorDacl(
            descriptor.0,
            &raw mut dacl_present,
            &raw mut dacl,
            &raw mut dacl_defaulted,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }

    let acl_check = if dacl_present == 0 || dacl.is_null() {
        check_acl_bytes(None, protected, kind, &principals.borrowed())
    } else {
        let acl = descriptor_acl_bytes(bytes, base, dacl)?;
        check_acl_bytes(Some(acl), protected, kind, &principals.borrowed())
    };
    let acl_result = match acl_check {
        Ok(()) => Ok(()),
        Err(AclCheckError::Policy(error)) => Err(error),
        Err(AclCheckError::Structural(error)) => return Err(error),
    };
    Ok(DescriptorState {
        verified: VerifiedDescriptor { owner_sid },
        acl_result,
    })
}

fn descriptor_acl_bytes(descriptor: &[u8], base: usize, dacl: *mut ACL) -> io::Result<&[u8]> {
    #[cfg(test)]
    if FORCE_DESCRIPTOR_STRUCTURE_FAILURE.load(Ordering::SeqCst) {
        return Err(invalid_acl("forced structural DACL extraction failure"));
    }
    let offset = pointer_offset(dacl.cast(), base, descriptor.len(), "DACL")?;
    if !(dacl as usize).is_multiple_of(size_of::<u32>()) {
        return Err(invalid_acl("DACL address is not DWORD-aligned"));
    }
    let header_end = offset
        .checked_add(size_of::<ACL>())
        .ok_or_else(bounds_error)?;
    let header = descriptor
        .get(offset..header_end)
        .ok_or_else(|| invalid_acl("DACL header extends past the descriptor"))?;
    let acl_size = usize::from(read_u16(header, 2, "DACL size field is truncated")?);
    if acl_size < size_of::<ACL>() {
        return Err(invalid_acl("DACL size is shorter than its header"));
    }
    let acl_end = offset.checked_add(acl_size).ok_or_else(bounds_error)?;
    descriptor
        .get(offset..acl_end)
        .ok_or_else(|| invalid_acl("DACL size extends past the descriptor"))?;
    // SAFETY: The complete declared ACL range is inside the live descriptor allocation.
    if unsafe { IsValidAcl(dacl) } == 0 {
        return Err(invalid_acl("Windows rejected the bounded DACL"));
    }
    let mut information = ACL_SIZE_INFORMATION::default();
    // SAFETY: The complete ACL is in-bounds and `information` is a writable fixed-size output.
    if unsafe {
        GetAclInformation(
            dacl,
            (&raw mut information).cast(),
            u32::try_from(size_of::<ACL_SIZE_INFORMATION>()).map_err(|_| bounds_error())?,
            AclSizeInformation,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let bytes_in_use = usize::try_from(information.AclBytesInUse).map_err(|_| bounds_error())?;
    if bytes_in_use < size_of::<ACL>() || bytes_in_use > acl_size {
        return Err(invalid_acl("DACL bytes in use exceed the bounded ACL size"));
    }
    let used_end = offset.checked_add(bytes_in_use).ok_or_else(bounds_error)?;
    descriptor
        .get(offset..used_end)
        .ok_or_else(|| invalid_acl("DACL bytes in use extend past the descriptor"))
}

fn pointer_offset(
    pointer: *const c_void,
    base: usize,
    length: usize,
    label: &'static str,
) -> io::Result<usize> {
    let end = base.checked_add(length).ok_or_else(bounds_error)?;
    let address = pointer as usize;
    if address < base || address >= end {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{label} pointer is outside the security descriptor"),
        ));
    }
    address.checked_sub(base).ok_or_else(bounds_error)
}

fn bounded_sid_length(bytes: &[u8], sid: PSID) -> io::Result<usize> {
    if bytes.len() < 8 {
        return Err(invalid_acl("owner SID header is truncated"));
    }
    let structural = usize::from(bytes[1])
        .checked_mul(size_of::<u32>())
        .and_then(|count| 8_usize.checked_add(count))
        .ok_or_else(bounds_error)?;
    if structural > bytes.len() {
        return Err(invalid_acl("owner SID extends past the descriptor"));
    }
    // SAFETY: The bounded SID structure is fully inside the live descriptor allocation.
    if unsafe { IsValidSid(sid) } == 0 {
        return Err(invalid_acl("Windows rejected the bounded owner SID"));
    }
    // SAFETY: Windows accepted this in-bounds SID and the descriptor remains live.
    let windows_length = unsafe { GetLengthSid(sid) };
    if usize::try_from(windows_length).ok() != Some(structural) {
        return Err(invalid_acl(
            "owner SID length disagrees with its bounded structure",
        ));
    }
    Ok(structural)
}

fn install_acl(handle: HANDLE, acl: &OwnedAcl) -> io::Result<()> {
    #[cfg(test)]
    SET_SECURITY_INFO_CALLS.fetch_add(1, Ordering::SeqCst);
    // SAFETY: The retained handle and constructed non-null ACL are live. Only DACL protection is
    // changed; owner, group, and SACL pointers are intentionally null.
    let status = unsafe {
        SetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            acl.as_ptr(),
            ptr::null(),
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(
            i32::try_from(status).unwrap_or(i32::MAX),
        ));
    }
    Ok(())
}

fn bounds_error() -> io::Error {
    invalid_acl("ACL or security descriptor bounds overflow")
}

fn stage_error(stage: &'static str, error: &io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{stage}: {error}"))
}

fn stage_invalid_data(stage: &'static str, detail: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("{stage}: {detail}"))
}

#[cfg(test)]
fn reset_set_security_info_calls_for_test() {
    SET_SECURITY_INFO_CALLS.store(0, Ordering::SeqCst);
}

#[cfg(test)]
fn set_security_info_calls_for_test() -> usize {
    SET_SECURITY_INFO_CALLS.load(Ordering::SeqCst)
}

#[cfg(test)]
fn force_post_write_failure_for_test(force: bool) {
    FORCE_POST_WRITE_FAILURE.store(force, Ordering::SeqCst);
}

#[cfg(test)]
fn force_descriptor_structure_failure_for_test(force: bool) {
    FORCE_DESCRIPTOR_STRUCTURE_FAILURE.store(force, Ordering::SeqCst);
}

#[cfg(test)]
fn read_owner_sid_for_test(path: &Path, kind: StateArtifactKind) -> io::Result<Box<[u8]>> {
    let principals = expected_principals()?;
    let handle = open_target(path, kind, false)?;
    Ok(read_descriptor_state(handle.0, kind, &principals)?
        .verified
        .owner_sid)
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::c_void,
        fs, io,
        mem::size_of,
        os::windows::{ffi::OsStrExt as _, fs::symlink_file},
        path::Path,
        ptr,
        sync::Mutex,
    };

    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, INVALID_HANDLE_VALUE, LocalFree},
        Security::{
            ACCESS_ALLOWED_ACE, ACE_HEADER, ACL,
            Authorization::{
                ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
                SE_FILE_OBJECT, SetSecurityInfo,
            },
            CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl,
            INHERITED_ACE, OBJECT_INHERIT_ACE, PROTECTED_DACL_SECURITY_INFORMATION,
            PSECURITY_DESCRIPTOR, PSID, UNPROTECTED_DACL_SECURITY_INFORMATION,
        },
        Storage::FileSystem::{
            CreateFileW, FILE_ALL_ACCESS, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, READ_CONTROL, WRITE_DAC,
        },
        System::SystemServices::{
            ACCESS_ALLOWED_ACE_TYPE, ACCESS_ALLOWED_CALLBACK_ACE_TYPE,
            ACCESS_ALLOWED_OBJECT_ACE_TYPE, ACCESS_DENIED_ACE_TYPE, SYSTEM_AUDIT_ACE_TYPE,
        },
    };

    use super::{
        AlignedBuffer, ExpectedPrincipals, StateArtifactKind, descriptor_acl_bytes,
        ensure_private_state_artifact, force_descriptor_structure_failure_for_test,
        force_post_write_failure_for_test, read_owner_sid_for_test,
        reset_set_security_info_calls_for_test, set_security_info_calls_for_test, verify_acl_bytes,
        verify_private_state_artifact,
    };

    const USER_SID: [u8; 16] = [1, 2, 0, 0, 0, 0, 0, 5, 21, 0, 0, 0, 232, 3, 0, 0];
    const SYSTEM_SID: [u8; 12] = [1, 1, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0];
    const ADMIN_SID: [u8; 16] = [1, 2, 0, 0, 0, 0, 0, 5, 32, 0, 0, 0, 32, 2, 0, 0];
    const EVERYONE_SID: [u8; 12] = [1, 1, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0];
    static NATIVE_TEST_LOCK: Mutex<()> = Mutex::new(());

    struct TestHandle(HANDLE);

    impl Drop for TestHandle {
        fn drop(&mut self) {
            // SAFETY: This test guard owns a successful `CreateFileW` result.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    struct TestLocalAllocation(*mut c_void);

    impl Drop for TestLocalAllocation {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: This guard owns a successful LocalAlloc-family API result.
                unsafe {
                    let _ = LocalFree(self.0);
                }
            }
        }
    }

    #[derive(Clone)]
    struct TestAce {
        ace_type: u8,
        flags: u8,
        mask: u32,
        sid: Vec<u8>,
        declared_size: Option<u16>,
        suffix: Vec<u8>,
    }

    impl TestAce {
        fn allow(sid: &[u8], flags: u8) -> Self {
            Self {
                ace_type: u8::try_from(ACCESS_ALLOWED_ACE_TYPE).unwrap_or(u8::MAX),
                flags,
                mask: FILE_ALL_ACCESS,
                sid: sid.to_vec(),
                declared_size: None,
                suffix: Vec::new(),
            }
        }
    }

    fn principals() -> ExpectedPrincipals<'static> {
        ExpectedPrincipals {
            user: &USER_SID,
            system: &SYSTEM_SID,
            administrators: &ADMIN_SID,
        }
    }

    fn directory_flags() -> u8 {
        u8::try_from(OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE).unwrap_or(u8::MAX)
    }

    fn acl(aces: &[TestAce]) -> Vec<u8> {
        let mut bytes = vec![0_u8; size_of::<ACL>()];
        bytes[0] = 2;
        for ace in aces {
            let start = bytes.len();
            let sid_start = size_of::<ACCESS_ALLOWED_ACE>() - size_of::<u32>();
            let actual_size = sid_start + ace.sid.len() + ace.suffix.len();
            bytes.resize(start + actual_size, 0);
            bytes[start] = ace.ace_type;
            bytes[start + 1] = ace.flags;
            let declared = ace
                .declared_size
                .unwrap_or_else(|| u16::try_from(actual_size).unwrap_or(u16::MAX));
            bytes[start + 2..start + 4].copy_from_slice(&declared.to_le_bytes());
            bytes[start + 4..start + 8].copy_from_slice(&ace.mask.to_le_bytes());
            bytes[start + sid_start..start + sid_start + ace.sid.len()].copy_from_slice(&ace.sid);
            bytes[start + sid_start + ace.sid.len()..start + actual_size]
                .copy_from_slice(&ace.suffix);
        }
        let acl_size = u16::try_from(bytes.len()).unwrap_or(u16::MAX);
        bytes[2..4].copy_from_slice(&acl_size.to_le_bytes());
        let ace_count = u16::try_from(aces.len()).unwrap_or(u16::MAX);
        bytes[4..6].copy_from_slice(&ace_count.to_le_bytes());
        bytes
    }

    fn exact_file_acl() -> Vec<u8> {
        acl(&[
            TestAce::allow(&USER_SID, 0),
            TestAce::allow(&SYSTEM_SID, 0),
            TestAce::allow(&ADMIN_SID, 0),
        ])
    }

    fn assert_invalid(result: io::Result<()>, fragment: &str) {
        assert!(result.is_err(), "malformed ACL must fail closed");
        let Some(error) = result.err() else {
            return;
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(
            error.to_string().contains(fragment),
            "expected {fragment:?} in {error}"
        );
    }

    fn native_test_guard() -> std::sync::MutexGuard<'static, ()> {
        NATIVE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn encode_path(path: &Path) -> io::Result<Vec<u16>> {
        let mut encoded = path.as_os_str().encode_wide().collect::<Vec<_>>();
        if encoded.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "test path contains an interior NUL",
            ));
        }
        encoded.push(0);
        Ok(encoded)
    }

    fn open_test_target(path: &Path, kind: StateArtifactKind) -> io::Result<TestHandle> {
        let encoded = encode_path(path)?;
        let flags = FILE_FLAG_OPEN_REPARSE_POINT
            | match kind {
                StateArtifactKind::File => 0,
                StateArtifactKind::Directory => FILE_FLAG_BACKUP_SEMANTICS,
            };
        // SAFETY: The encoded path is live and NUL-terminated. The returned handle is owned.
        let handle = unsafe {
            CreateFileW(
                encoded.as_ptr(),
                READ_CONTROL | WRITE_DAC,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null(),
                OPEN_EXISTING,
                flags,
                ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(TestHandle(handle))
    }

    fn set_test_dacl(
        path: &Path,
        kind: StateArtifactKind,
        sddl: &str,
        protected: bool,
    ) -> io::Result<()> {
        let encoded = sddl.encode_utf16().chain([0]).collect::<Vec<_>>();
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: The SDDL is live and NUL-terminated; the output receives LocalAlloc memory.
        let converted = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                encoded.as_ptr(),
                SDDL_REVISION_1,
                &raw mut descriptor,
                ptr::null_mut(),
            )
        };
        if converted == 0 || descriptor.is_null() {
            return Err(io::Error::last_os_error());
        }
        let _descriptor = TestLocalAllocation(descriptor.cast());
        let mut present = 0;
        let mut dacl: *mut ACL = ptr::null_mut();
        let mut defaulted = 0;
        // SAFETY: The converted descriptor is live and all outputs are writable.
        let loaded = unsafe {
            GetSecurityDescriptorDacl(
                descriptor,
                &raw mut present,
                &raw mut dacl,
                &raw mut defaulted,
            )
        };
        if loaded == 0 || present == 0 || dacl.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "test SDDL did not produce a non-null DACL",
            ));
        }
        let handle = open_test_target(path, kind)?;
        let protection = if protected {
            PROTECTED_DACL_SECURITY_INFORMATION
        } else {
            UNPROTECTED_DACL_SECURITY_INFORMATION
        };
        // SAFETY: The handle and non-null DACL are live; owner/group/SACL are intentionally null.
        let status = unsafe {
            SetSecurityInfo(
                handle.0,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | protection,
                ptr::null_mut::<c_void>() as PSID,
                ptr::null_mut::<c_void>() as PSID,
                dacl,
                ptr::null(),
            )
        };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(
                i32::try_from(status).unwrap_or(i32::MAX),
            ));
        }
        Ok(())
    }

    fn test_sddl(kind: StateArtifactKind, extra: &str, protected: bool) -> io::Result<String> {
        let user = crate::current_process_user_sid()?;
        let flags = match kind {
            StateArtifactKind::File => "",
            StateArtifactKind::Directory => "OICI",
        };
        let protection = if protected { "P" } else { "" };
        Ok(format!(
            "D:{protection}{extra}(A;{flags};GA;;;{user})(A;{flags};GA;;;SY)(A;{flags};GA;;;BA)"
        ))
    }

    fn permissive_sddl(kind: StateArtifactKind) -> io::Result<String> {
        let flags = match kind {
            StateArtifactKind::File => "",
            StateArtifactKind::Directory => "OICI",
        };
        test_sddl(kind, &format!("(A;{flags};GA;;;WD)"), true)
    }

    fn deny_sddl(kind: StateArtifactKind) -> io::Result<String> {
        let flags = match kind {
            StateArtifactKind::File => "",
            StateArtifactKind::Directory => "OICI",
        };
        test_sddl(kind, &format!("(D;{flags};0x00000001;;;WD)"), true)
    }

    #[test]
    fn bounded_missing_null_short_and_unprotected_dacls_are_rejected() {
        let expected = principals();
        assert_invalid(
            verify_acl_bytes(None, true, StateArtifactKind::File, &expected),
            "non-null",
        );
        assert_invalid(
            verify_acl_bytes(Some(&[]), true, StateArtifactKind::File, &expected),
            "header",
        );
        assert_invalid(
            verify_acl_bytes(
                Some(&exact_file_acl()),
                false,
                StateArtifactKind::File,
                &expected,
            ),
            "protected",
        );
    }

    #[test]
    fn bounded_acl_size_count_and_trailing_bytes_are_rejected() {
        let expected = principals();
        let mut oversized = exact_file_acl();
        oversized[2..4].copy_from_slice(&u16::MAX.to_le_bytes());
        assert_invalid(
            verify_acl_bytes(Some(&oversized), true, StateArtifactKind::File, &expected),
            "size",
        );

        let mut count = exact_file_acl();
        count[4..6].copy_from_slice(&2_u16.to_le_bytes());
        assert_invalid(
            verify_acl_bytes(Some(&count), true, StateArtifactKind::File, &expected),
            "three",
        );

        let mut trailing = exact_file_acl();
        trailing.push(0);
        assert_invalid(
            verify_acl_bytes(Some(&trailing), true, StateArtifactKind::File, &expected),
            "trailing",
        );
    }

    #[test]
    fn bounded_truncated_ace_header_sid_start_sid_and_ranges_are_rejected() {
        let expected = principals();
        let sid_start = size_of::<ACCESS_ALLOWED_ACE>() - size_of::<u32>();

        let mut header = exact_file_acl();
        header.truncate(size_of::<ACL>() + size_of::<ACE_HEADER>() - 1);
        let len = u16::try_from(header.len()).unwrap_or(u16::MAX);
        header[2..4].copy_from_slice(&len.to_le_bytes());
        assert_invalid(
            verify_acl_bytes(Some(&header), true, StateArtifactKind::File, &expected),
            "header",
        );

        let before_sid = acl(&[
            TestAce {
                declared_size: Some(u16::try_from(sid_start - 1).unwrap_or_default()),
                ..TestAce::allow(&USER_SID, 0)
            },
            TestAce::allow(&SYSTEM_SID, 0),
            TestAce::allow(&ADMIN_SID, 0),
        ]);
        assert_invalid(
            verify_acl_bytes(Some(&before_sid), true, StateArtifactKind::File, &expected),
            "SidStart",
        );

        let mut truncated_sid = USER_SID.to_vec();
        truncated_sid[1] = 4;
        let sid = acl(&[
            TestAce::allow(&truncated_sid, 0),
            TestAce::allow(&SYSTEM_SID, 0),
            TestAce::allow(&ADMIN_SID, 0),
        ]);
        assert_invalid(
            verify_acl_bytes(Some(&sid), true, StateArtifactKind::File, &expected),
            "SID",
        );

        let out_of_range = acl(&[
            TestAce {
                declared_size: Some(u16::MAX),
                ..TestAce::allow(&USER_SID, 0)
            },
            TestAce::allow(&SYSTEM_SID, 0),
            TestAce::allow(&ADMIN_SID, 0),
        ]);
        assert_invalid(
            verify_acl_bytes(
                Some(&out_of_range),
                true,
                StateArtifactKind::File,
                &expected,
            ),
            "range",
        );
    }

    #[test]
    fn bounded_invalid_sid_and_sid_trailing_bytes_are_rejected() {
        let expected = principals();
        let mut invalid = USER_SID;
        invalid[0] = 2;
        let invalid_acl = acl(&[
            TestAce::allow(&invalid, 0),
            TestAce::allow(&SYSTEM_SID, 0),
            TestAce::allow(&ADMIN_SID, 0),
        ]);
        assert_invalid(
            verify_acl_bytes(Some(&invalid_acl), true, StateArtifactKind::File, &expected),
            "SID",
        );

        let with_suffix = acl(&[
            TestAce {
                suffix: vec![0, 0, 0, 0],
                ..TestAce::allow(&USER_SID, 0)
            },
            TestAce::allow(&SYSTEM_SID, 0),
            TestAce::allow(&ADMIN_SID, 0),
        ]);
        assert_invalid(
            verify_acl_bytes(Some(&with_suffix), true, StateArtifactKind::File, &expected),
            "exactly",
        );
    }

    #[test]
    fn bounded_duplicate_missing_unknown_and_broad_trustees_are_rejected() {
        let expected = principals();
        for malformed in [
            acl(&[
                TestAce::allow(&USER_SID, 0),
                TestAce::allow(&USER_SID, 0),
                TestAce::allow(&ADMIN_SID, 0),
            ]),
            acl(&[TestAce::allow(&USER_SID, 0), TestAce::allow(&SYSTEM_SID, 0)]),
            acl(&[
                TestAce::allow(&USER_SID, 0),
                TestAce::allow(&SYSTEM_SID, 0),
                TestAce::allow(&[1, 1, 0, 0, 0, 0, 0, 5, 19, 0, 0, 0], 0),
            ]),
            acl(&[
                TestAce::allow(&USER_SID, 0),
                TestAce::allow(&SYSTEM_SID, 0),
                TestAce::allow(&EVERYONE_SID, 0),
            ]),
        ] {
            assert_invalid(
                verify_acl_bytes(Some(&malformed), true, StateArtifactKind::File, &expected),
                "trustee",
            );
        }
    }

    #[test]
    fn bounded_non_allow_ace_types_are_rejected() {
        let expected = principals();
        for ace_type in [
            ACCESS_DENIED_ACE_TYPE,
            SYSTEM_AUDIT_ACE_TYPE,
            ACCESS_ALLOWED_OBJECT_ACE_TYPE,
            ACCESS_ALLOWED_CALLBACK_ACE_TYPE,
        ] {
            let mut wrong = TestAce::allow(&USER_SID, 0);
            wrong.ace_type = u8::try_from(ace_type).unwrap_or(u8::MAX);
            let malformed = acl(&[
                wrong,
                TestAce::allow(&SYSTEM_SID, 0),
                TestAce::allow(&ADMIN_SID, 0),
            ]);
            assert_invalid(
                verify_acl_bytes(Some(&malformed), true, StateArtifactKind::File, &expected),
                "allow",
            );
        }
    }

    #[test]
    fn bounded_inherited_wrong_mask_and_extra_mask_are_rejected() {
        let expected = principals();
        let inherited = acl(&[
            TestAce::allow(&USER_SID, u8::try_from(INHERITED_ACE).unwrap_or(u8::MAX)),
            TestAce::allow(&SYSTEM_SID, 0),
            TestAce::allow(&ADMIN_SID, 0),
        ]);
        assert_invalid(
            verify_acl_bytes(Some(&inherited), true, StateArtifactKind::File, &expected),
            "flags",
        );

        for mask in [FILE_ALL_ACCESS & !1, FILE_ALL_ACCESS | 0x8000_0000] {
            let mut wrong = TestAce::allow(&USER_SID, 0);
            wrong.mask = mask;
            let malformed = acl(&[
                wrong,
                TestAce::allow(&SYSTEM_SID, 0),
                TestAce::allow(&ADMIN_SID, 0),
            ]);
            assert_invalid(
                verify_acl_bytes(Some(&malformed), true, StateArtifactKind::File, &expected),
                "mask",
            );
        }
    }

    #[test]
    fn bounded_file_and_directory_flags_are_exact() {
        let expected = principals();
        let directory = acl(&[
            TestAce::allow(&USER_SID, directory_flags()),
            TestAce::allow(&SYSTEM_SID, directory_flags()),
            TestAce::allow(&ADMIN_SID, directory_flags()),
        ]);
        assert!(
            verify_acl_bytes(
                Some(&directory),
                true,
                StateArtifactKind::Directory,
                &expected,
            )
            .is_ok(),
            "exact directory ACL must pass"
        );
        assert_invalid(
            verify_acl_bytes(Some(&directory), true, StateArtifactKind::File, &expected),
            "flags",
        );
        assert_invalid(
            verify_acl_bytes(
                Some(&exact_file_acl()),
                true,
                StateArtifactKind::Directory,
                &expected,
            ),
            "flags",
        );
    }

    #[test]
    fn bounded_exact_acl_accepts_any_trustee_order() {
        let expected = principals();
        for exact in [
            exact_file_acl(),
            acl(&[
                TestAce::allow(&ADMIN_SID, 0),
                TestAce::allow(&USER_SID, 0),
                TestAce::allow(&SYSTEM_SID, 0),
            ]),
        ] {
            assert!(
                verify_acl_bytes(Some(&exact), true, StateArtifactKind::File, &expected).is_ok(),
                "order-independent exact ACL must pass"
            );
        }
    }

    #[test]
    fn retained_handle_file_and_directory_fast_paths_are_idempotent() -> io::Result<()> {
        let _guard = native_test_guard();
        let temporary = tempfile::tempdir()?;
        let file = temporary.path().join("state.db");
        let directory = temporary.path().join("imports");
        fs::write(&file, b"state")?;
        fs::create_dir(&directory)?;

        ensure_private_state_artifact(&file, StateArtifactKind::File)?;
        ensure_private_state_artifact(&directory, StateArtifactKind::Directory)?;
        reset_set_security_info_calls_for_test();

        ensure_private_state_artifact(&file, StateArtifactKind::File)?;
        ensure_private_state_artifact(&directory, StateArtifactKind::Directory)?;
        assert_eq!(set_security_info_calls_for_test(), 0);
        verify_private_state_artifact(&file, StateArtifactKind::File)?;
        verify_private_state_artifact(&directory, StateArtifactKind::Directory)
    }

    #[test]
    fn retained_handle_repairs_permissive_and_deny_dacls_once() -> io::Result<()> {
        let _guard = native_test_guard();
        let temporary = tempfile::tempdir()?;
        let file = temporary.path().join("permissive.db");
        let directory = temporary.path().join("denied");
        fs::write(&file, b"state")?;
        fs::create_dir(&directory)?;
        set_test_dacl(
            &file,
            StateArtifactKind::File,
            &permissive_sddl(StateArtifactKind::File)?,
            true,
        )?;
        set_test_dacl(
            &directory,
            StateArtifactKind::Directory,
            &deny_sddl(StateArtifactKind::Directory)?,
            true,
        )?;

        reset_set_security_info_calls_for_test();
        ensure_private_state_artifact(&file, StateArtifactKind::File)?;
        assert_eq!(set_security_info_calls_for_test(), 1);
        reset_set_security_info_calls_for_test();
        ensure_private_state_artifact(&directory, StateArtifactKind::Directory)?;
        assert_eq!(set_security_info_calls_for_test(), 1);
        verify_private_state_artifact(&file, StateArtifactKind::File)?;
        verify_private_state_artifact(&directory, StateArtifactKind::Directory)
    }

    #[test]
    fn retained_handle_verify_detects_tamper_without_mutation() -> io::Result<()> {
        let _guard = native_test_guard();
        let temporary = tempfile::tempdir()?;
        let file = temporary.path().join("tampered.db");
        fs::write(&file, b"state")?;
        set_test_dacl(
            &file,
            StateArtifactKind::File,
            &permissive_sddl(StateArtifactKind::File)?,
            true,
        )?;
        reset_set_security_info_calls_for_test();

        let first = verify_private_state_artifact(&file, StateArtifactKind::File);
        assert!(first.is_err());
        assert_eq!(set_security_info_calls_for_test(), 0);
        let second = verify_private_state_artifact(&file, StateArtifactKind::File);
        assert!(second.is_err());
        assert_eq!(set_security_info_calls_for_test(), 0);
        Ok(())
    }

    #[test]
    fn retained_handle_rejects_wrong_kinds_before_acl_access() -> io::Result<()> {
        let _guard = native_test_guard();
        let temporary = tempfile::tempdir()?;
        let file = temporary.path().join("file");
        let directory = temporary.path().join("directory");
        fs::write(&file, b"state")?;
        fs::create_dir(&directory)?;
        reset_set_security_info_calls_for_test();

        for result in [
            ensure_private_state_artifact(&file, StateArtifactKind::Directory),
            ensure_private_state_artifact(&directory, StateArtifactKind::File),
        ] {
            let error = result
                .err()
                .ok_or_else(|| io::Error::other("wrong artifact kind was accepted"))?;
            let message = error.to_string();
            assert!(
                message.contains("object-kind validation") || message.contains("target open"),
                "wrong kind failed outside target acquisition: {message}"
            );
        }
        assert_eq!(set_security_info_calls_for_test(), 0);
        Ok(())
    }

    #[test]
    fn retained_handle_rejects_reparse_points_before_acl_access() -> io::Result<()> {
        let _guard = native_test_guard();
        let temporary = tempfile::tempdir()?;
        let target = temporary.path().join("target.db");
        let link = temporary.path().join("link.db");
        fs::write(&target, b"state")?;
        symlink_file(&target, &link)?;
        reset_set_security_info_calls_for_test();

        let error = ensure_private_state_artifact(&link, StateArtifactKind::File)
            .err()
            .ok_or_else(|| io::Error::other("reparse point was accepted"))?;
        assert!(error.to_string().contains("object-kind validation"));
        assert!(error.to_string().contains("reparse"));
        assert_eq!(set_security_info_calls_for_test(), 0);
        Ok(())
    }

    #[test]
    fn retained_handle_repair_preserves_owner() -> io::Result<()> {
        let _guard = native_test_guard();
        let temporary = tempfile::tempdir()?;
        let file = temporary.path().join("owner.db");
        fs::write(&file, b"state")?;
        set_test_dacl(
            &file,
            StateArtifactKind::File,
            &permissive_sddl(StateArtifactKind::File)?,
            true,
        )?;
        let before = read_owner_sid_for_test(&file, StateArtifactKind::File)?;

        reset_set_security_info_calls_for_test();
        ensure_private_state_artifact(&file, StateArtifactKind::File)?;
        let after = read_owner_sid_for_test(&file, StateArtifactKind::File)?;
        assert_eq!(before, after);
        assert_eq!(set_security_info_calls_for_test(), 1);
        Ok(())
    }

    #[test]
    fn retained_handle_repair_requires_post_write_verification() -> io::Result<()> {
        let _guard = native_test_guard();
        let temporary = tempfile::tempdir()?;
        let file = temporary.path().join("post-write.db");
        fs::write(&file, b"state")?;
        set_test_dacl(
            &file,
            StateArtifactKind::File,
            &permissive_sddl(StateArtifactKind::File)?,
            true,
        )?;
        reset_set_security_info_calls_for_test();
        force_post_write_failure_for_test(true);
        let result = ensure_private_state_artifact(&file, StateArtifactKind::File);
        force_post_write_failure_for_test(false);

        let error = result
            .err()
            .ok_or_else(|| io::Error::other("forced post-write mismatch was accepted"))?;
        assert!(error.to_string().contains("post-write verification"));
        assert_eq!(set_security_info_calls_for_test(), 1);
        Ok(())
    }

    #[test]
    fn retained_handle_parent_repair_preserves_protected_children() -> io::Result<()> {
        let _guard = native_test_guard();
        let temporary = tempfile::tempdir()?;
        let parent = temporary.path().join("parent");
        let child_directory = parent.join("child");
        let child_file = parent.join("child.db");
        fs::create_dir(&parent)?;
        fs::create_dir(&child_directory)?;
        fs::write(&child_file, b"state")?;
        ensure_private_state_artifact(&parent, StateArtifactKind::Directory)?;
        ensure_private_state_artifact(&child_directory, StateArtifactKind::Directory)?;
        ensure_private_state_artifact(&child_file, StateArtifactKind::File)?;
        set_test_dacl(
            &parent,
            StateArtifactKind::Directory,
            &permissive_sddl(StateArtifactKind::Directory)?,
            true,
        )?;

        reset_set_security_info_calls_for_test();
        ensure_private_state_artifact(&parent, StateArtifactKind::Directory)?;
        assert_eq!(set_security_info_calls_for_test(), 1);
        verify_private_state_artifact(&child_directory, StateArtifactKind::Directory)?;
        verify_private_state_artifact(&child_file, StateArtifactKind::File)
    }

    #[test]
    fn retained_handle_parent_repair_does_not_accept_unprotected_child_propagation()
    -> io::Result<()> {
        let _guard = native_test_guard();
        let temporary = tempfile::tempdir()?;
        let parent = temporary.path().join("parent");
        let child = parent.join("child");
        fs::create_dir(&parent)?;
        fs::create_dir(&child)?;
        ensure_private_state_artifact(&parent, StateArtifactKind::Directory)?;
        let unprotected = test_sddl(StateArtifactKind::Directory, "", false)?;
        set_test_dacl(&child, StateArtifactKind::Directory, &unprotected, false)?;
        set_test_dacl(
            &parent,
            StateArtifactKind::Directory,
            &permissive_sddl(StateArtifactKind::Directory)?,
            true,
        )?;

        reset_set_security_info_calls_for_test();
        ensure_private_state_artifact(&parent, StateArtifactKind::Directory)?;
        assert_eq!(set_security_info_calls_for_test(), 1);
        reset_set_security_info_calls_for_test();
        assert!(
            verify_private_state_artifact(&child, StateArtifactKind::Directory).is_err(),
            "an unprotected child must require target-level hardening"
        );
        assert_eq!(set_security_info_calls_for_test(), 0);
        Ok(())
    }

    #[test]
    fn review_structural_descriptor_failure_does_not_repair() -> io::Result<()> {
        let _guard = native_test_guard();
        let temporary = tempfile::tempdir()?;
        let file = temporary.path().join("structural-failure.db");
        fs::write(&file, b"state")?;
        set_test_dacl(
            &file,
            StateArtifactKind::File,
            &permissive_sddl(StateArtifactKind::File)?,
            true,
        )?;
        reset_set_security_info_calls_for_test();
        force_descriptor_structure_failure_for_test(true);
        let result = ensure_private_state_artifact(&file, StateArtifactKind::File);
        force_descriptor_structure_failure_for_test(false);

        let error = result
            .err()
            .ok_or_else(|| io::Error::other("structural descriptor failure was repaired"))?;
        assert!(error.to_string().contains("structural"));
        assert_eq!(set_security_info_calls_for_test(), 0);
        Ok(())
    }

    #[test]
    fn review_misaligned_dacl_is_rejected_before_native_validation() -> io::Result<()> {
        let mut storage = AlignedBuffer::zeroed(size_of::<ACL>() + 1)?;
        let misaligned_address = (storage.as_mut_ptr::<u8>() as usize)
            .checked_add(1)
            .ok_or_else(|| io::Error::other("test DACL address overflow"))?;
        // No dereference occurs here; this deliberately forms an in-allocation but DWORD-misaligned
        // address to exercise the pre-native validation gate.
        let dacl = ptr::with_exposed_provenance_mut::<ACL>(misaligned_address);
        let descriptor = storage.as_bytes();
        let base = descriptor.as_ptr() as usize;

        let error = descriptor_acl_bytes(descriptor, base, dacl)
            .err()
            .ok_or_else(|| io::Error::other("misaligned DACL was accepted"))?;
        assert!(error.to_string().contains("DWORD-aligned"));
        Ok(())
    }
}
