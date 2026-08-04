use std::{
    ffi::c_void,
    io,
    mem::size_of,
    ptr, slice,
    sync::{Mutex, OnceLock},
};

use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_INSUFFICIENT_BUFFER, HANDLE, LocalFree},
    Security::{
        Authorization::ConvertSidToStringSidW, GetLengthSid, GetTokenInformation, IsValidSid,
        TOKEN_QUERY, TOKEN_USER, TokenUser,
    },
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};

const MAX_TOKEN_USER_BYTES: u32 = 64 * 1024;
const SID_HEADER_BYTES: usize = 8;

struct AlignedBuffer {
    words: Box<[usize]>,
    byte_len: usize,
}

impl AlignedBuffer {
    fn zeroed(byte_len: usize) -> io::Result<Self> {
        let width = size_of::<usize>();
        let words = byte_len
            .checked_add(width - 1)
            .ok_or_else(|| io::Error::other("aligned buffer length overflow"))?
            / width;
        Ok(Self {
            words: vec![0; words].into_boxed_slice(),
            byte_len,
        })
    }

    fn as_bytes(&self) -> &[u8] {
        // SAFETY: `words` is initialized, suitably aligned storage and `byte_len` is bounded by it.
        unsafe { slice::from_raw_parts(self.words.as_ptr().cast(), self.byte_len) }
    }

    fn as_mut_void_ptr(&mut self) -> *mut c_void {
        self.words.as_mut_ptr().cast()
    }
}

pub(crate) struct ProcessUser {
    sid: AlignedBuffer,
    sid_text: String,
}

impl ProcessUser {
    pub(crate) fn sid_bytes(&self) -> &[u8] {
        self.sid.as_bytes()
    }
}

struct SuccessCache<T> {
    value: OnceLock<T>,
    resolve: Mutex<()>,
}

impl<T> SuccessCache<T> {
    const fn new() -> Self {
        Self {
            value: OnceLock::new(),
            resolve: Mutex::new(()),
        }
    }

    fn get_or_resolve(&self, f: impl FnOnce() -> io::Result<T>) -> io::Result<&T> {
        if let Some(value) = self.value.get() {
            return Ok(value);
        }
        let _guard = self
            .resolve
            .lock()
            .map_err(|_| io::Error::other("process identity cache lock was poisoned"))?;
        if let Some(value) = self.value.get() {
            return Ok(value);
        }
        let resolved = f()?;
        let _ = self.value.set(resolved);
        self.value
            .get()
            .ok_or_else(|| io::Error::other("process identity cache publication failed"))
    }
}

static PROCESS_USER: SuccessCache<ProcessUser> = SuccessCache::new();

pub fn current_process_user_sid() -> io::Result<String> {
    let process_user = current_process_user()?;
    if process_user.sid_bytes().is_empty() {
        return Err(stage_invalid_data(
            "token-user bounds",
            "cached SID is empty",
        ));
    }
    Ok(process_user.sid_text.clone())
}

pub(crate) fn current_process_user() -> io::Result<&'static ProcessUser> {
    PROCESS_USER.get_or_resolve(resolve_process_user)
}

struct OwnedToken(HANDLE);

impl Drop for OwnedToken {
    fn drop(&mut self) {
        // SAFETY: This guard owns the successful `OpenProcessToken` result and closes it once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

struct LocalWideString(*mut u16);

impl Drop for LocalWideString {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: This guard owns the allocation returned by `ConvertSidToStringSidW`.
            unsafe {
                let _ = LocalFree(self.0.cast());
            }
        }
    }
}

fn resolve_process_user() -> io::Result<ProcessUser> {
    let token = open_process_token()?;
    let (token_user_bytes, returned) = query_token_user(token.0)?;
    let (sid, sub_authority_count) = copy_bounded_sid(&token_user_bytes, returned)?;
    drop(token_user_bytes);
    drop(token);
    let sid_text = sid_to_string(&sid, sub_authority_count)?;
    Ok(ProcessUser { sid, sid_text })
}

fn query_token_user(token: HANDLE) -> io::Result<(AlignedBuffer, usize)> {
    let mut required = 0_u32;
    // SAFETY: `token` is live, the null buffer and zero length request the required byte count,
    // and `required` is a writable output.
    let sized =
        unsafe { GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &raw mut required) };
    if sized != 0 {
        return Err(stage_invalid_data(
            "token-user size query",
            "unexpectedly succeeded with a null buffer",
        ));
    }
    let size_error = io::Error::last_os_error();
    if size_error.raw_os_error()
        != Some(i32::try_from(ERROR_INSUFFICIENT_BUFFER).unwrap_or(i32::MAX))
    {
        return Err(stage_error("token-user size query", &size_error));
    }
    let minimum = u32::try_from(size_of::<TOKEN_USER>()).unwrap_or(u32::MAX);
    if required < minimum || required > MAX_TOKEN_USER_BYTES {
        return Err(stage_invalid_data(
            "token-user size query",
            "returned length is outside the accepted range",
        ));
    }

    let required_usize = usize::try_from(required)
        .map_err(|error| stage_error("token-user size query", &io::Error::other(error)))?;
    let mut token_user_bytes = AlignedBuffer::zeroed(required_usize)
        .map_err(|error| stage_error("token-user size query", &error))?;
    let mut returned = required;
    // SAFETY: `token` is live and `token_user_bytes` provides initialized, suitably aligned,
    // writable storage for exactly `required` bytes. `returned` is a writable output.
    let loaded = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            token_user_bytes.as_mut_void_ptr(),
            required,
            &raw mut returned,
        )
    };
    if loaded == 0 {
        return Err(stage_error("token-user query", &io::Error::last_os_error()));
    }
    if returned < minimum || returned > required {
        return Err(stage_invalid_data(
            "token-user bounds",
            "returned length is outside the supplied buffer",
        ));
    }

    let returned_usize = usize::try_from(returned)
        .map_err(|error| stage_error("token-user bounds", &io::Error::other(error)))?;
    Ok((token_user_bytes, returned_usize))
}

fn copy_bounded_sid(
    token_user_bytes: &AlignedBuffer,
    returned: usize,
) -> io::Result<(AlignedBuffer, usize)> {
    let bytes = &token_user_bytes.as_bytes()[..returned];
    // SAFETY: The buffer is aligned for `TOKEN_USER`, initialized, and `returned` was checked to
    // contain the complete structure before this read.
    let token_user = unsafe { ptr::read(token_user_bytes.words.as_ptr().cast::<TOKEN_USER>()) };
    let base = bytes.as_ptr() as usize;
    let buffer_end = base.checked_add(bytes.len()).ok_or_else(|| {
        stage_invalid_data("token-user bounds", "buffer address range overflowed")
    })?;
    let sid_start = token_user.User.Sid as usize;
    if sid_start < base || sid_start >= buffer_end {
        return Err(stage_invalid_data(
            "token-user bounds",
            "SID pointer is outside the returned bytes",
        ));
    }
    let sid_offset = sid_start.checked_sub(base).ok_or_else(|| {
        stage_invalid_data("token-user bounds", "SID offset calculation underflowed")
    })?;
    let sid_header_end = sid_offset
        .checked_add(SID_HEADER_BYTES)
        .ok_or_else(|| stage_invalid_data("token-user bounds", "SID header range overflowed"))?;
    if sid_header_end > bytes.len() {
        return Err(stage_invalid_data(
            "token-user bounds",
            "SID header extends past the returned bytes",
        ));
    }
    let sub_authority_count = usize::from(bytes[sid_offset + 1]);
    let sub_authority_bytes = sub_authority_count
        .checked_mul(size_of::<u32>())
        .ok_or_else(|| {
            stage_invalid_data("token-user bounds", "SID sub-authority length overflowed")
        })?;
    let structural_length = SID_HEADER_BYTES
        .checked_add(sub_authority_bytes)
        .ok_or_else(|| stage_invalid_data("token-user bounds", "SID length overflowed"))?;
    let sid_end = sid_offset
        .checked_add(structural_length)
        .ok_or_else(|| stage_invalid_data("token-user bounds", "SID byte range overflowed"))?;
    if sid_end > bytes.len() {
        return Err(stage_invalid_data(
            "token-user bounds",
            "SID extends past the returned bytes",
        ));
    }

    let sid_pointer = token_user.User.Sid;
    // SAFETY: The complete SID structure implied by its bounded header is inside the initialized
    // token information buffer, and the token keeps that buffer's source data live for this call.
    if unsafe { IsValidSid(sid_pointer) } == 0 {
        return Err(stage_invalid_data(
            "token-user bounds",
            "Windows rejected the bounded SID",
        ));
    }
    // SAFETY: `IsValidSid` accepted this in-bounds SID and the source buffer remains live.
    let windows_length = unsafe { GetLengthSid(sid_pointer) };
    if usize::try_from(windows_length).ok() != Some(structural_length) {
        return Err(stage_invalid_data(
            "token-user bounds",
            "Windows SID length disagrees with the bounded structural length",
        ));
    }

    let mut sid = AlignedBuffer::zeroed(structural_length)
        .map_err(|error| stage_error("token-user bounds", &error))?;
    // SAFETY: Both ranges contain `structural_length` bytes, do not overlap, and the destination
    // is initialized, suitably aligned owned storage.
    unsafe {
        ptr::copy_nonoverlapping(
            bytes[sid_offset..sid_end].as_ptr(),
            sid.as_mut_void_ptr().cast::<u8>(),
            structural_length,
        );
    }
    Ok((sid, sub_authority_count))
}

fn open_process_token() -> io::Result<OwnedToken> {
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: `GetCurrentProcess` returns the calling process pseudo-handle and `token` is a
    // writable output that receives an owned handle on success.
    let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) };
    if opened == 0 {
        return Err(stage_error(
            "process-token open",
            &io::Error::last_os_error(),
        ));
    }
    if token.is_null() {
        return Err(stage_invalid_data(
            "process-token open",
            "Windows returned a null token handle",
        ));
    }
    Ok(OwnedToken(token))
}

fn sid_to_string(sid: &AlignedBuffer, sub_authority_count: usize) -> io::Result<String> {
    let mut encoded = ptr::null_mut();
    // SAFETY: `sid` owns a suitably aligned SID that `IsValidSid` accepted, and `encoded` is a
    // writable output for a LocalAlloc-owned NUL-terminated UTF-16 string.
    let converted = unsafe {
        ConvertSidToStringSidW(sid.as_bytes().as_ptr().cast_mut().cast(), &raw mut encoded)
    };
    if converted == 0 {
        return Err(stage_error(
            "SID text conversion",
            &io::Error::last_os_error(),
        ));
    }
    if encoded.is_null() {
        return Err(stage_invalid_data(
            "SID text conversion",
            "Windows returned a null string",
        ));
    }
    let encoded = LocalWideString(encoded);
    let max_units = 2_usize
        .checked_add(3)
        .and_then(|length| length.checked_add(1 + 20))
        .and_then(|length| length.checked_add(sub_authority_count.saturating_mul(11)))
        .and_then(|length| length.checked_add(1))
        .ok_or_else(|| stage_invalid_data("SID text conversion", "text bound overflowed"))?;
    let mut length = None;
    for index in 0..max_units {
        // SAFETY: A successful `ConvertSidToStringSidW` returns a readable NUL-terminated string;
        // `max_units` is an upper bound derived from every numeric SID component.
        if unsafe { *encoded.0.add(index) } == 0 {
            length = Some(index);
            break;
        }
    }
    let length = length.ok_or_else(|| {
        stage_invalid_data(
            "SID text conversion",
            "converted SID text is missing a NUL terminator",
        )
    })?;
    // SAFETY: The scan above proved that `length` initialized UTF-16 units precede the NUL.
    let units = unsafe { slice::from_raw_parts(encoded.0, length) };
    String::from_utf16(units).map_err(|error| {
        stage_error(
            "SID text conversion",
            &io::Error::new(io::ErrorKind::InvalidData, error),
        )
    })
}

fn stage_error(stage: &'static str, error: &io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{stage}: {error}"))
}

fn stage_invalid_data(stage: &'static str, detail: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("{stage}: {detail}"))
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        sync::{
            Arc, Barrier,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
        time::Duration,
    };

    use super::{ProcessUser, SuccessCache, current_process_user, current_process_user_sid};

    #[test]
    fn resolver_error_is_not_cached_and_later_success_is_cached() -> io::Result<()> {
        let cache = SuccessCache::new();
        let calls = AtomicUsize::new(0);

        let first = cache.get_or_resolve(|| {
            calls.fetch_add(1, Ordering::SeqCst);
            Err::<usize, _>(io::Error::from_raw_os_error(5))
        });
        assert_eq!(first.err().and_then(|error| error.raw_os_error()), Some(5));

        assert_eq!(
            *cache.get_or_resolve(|| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(42)
            })?,
            42
        );
        assert_eq!(*cache.get_or_resolve(|| Ok(7))?, 42);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        Ok(())
    }

    #[test]
    fn concurrent_callers_execute_one_successful_resolver() -> io::Result<()> {
        const CALLERS: usize = 16;
        let cache = Arc::new(SuccessCache::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(CALLERS + 1));
        let mut callers = Vec::with_capacity(CALLERS);

        for _ in 0..CALLERS {
            let cache = Arc::clone(&cache);
            let calls = Arc::clone(&calls);
            let barrier = Arc::clone(&barrier);
            callers.push(thread::spawn(move || {
                barrier.wait();
                cache
                    .get_or_resolve(|| {
                        calls.fetch_add(1, Ordering::SeqCst);
                        thread::sleep(Duration::from_millis(20));
                        Ok(42)
                    })
                    .copied()
            }));
        }

        barrier.wait();
        for caller in callers {
            assert_eq!(
                caller
                    .join()
                    .map_err(|_| io::Error::other("identity cache caller panicked"))??,
                42
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[test]
    fn native_process_sid_is_numeric_canonical_and_stable() -> io::Result<()> {
        let first = current_process_user_sid()?;
        let second = current_process_user_sid()?;
        let process_user: &'static ProcessUser = current_process_user()?;

        assert_eq!(first, second);
        assert!(first.starts_with("S-1-"));
        assert!(
            first
                .split('-')
                .skip(1)
                .all(|component| !component.is_empty()
                    && component.bytes().all(|byte| byte.is_ascii_digit()))
        );
        assert!(!process_user.sid_bytes().is_empty());
        Ok(())
    }
}
