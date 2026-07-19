use std::{
    fs,
    path::{Component, Path, PathBuf},
};

use crate::{StateError, StateResult};

/// Creates a directory when necessary and restricts it to the current identity and
/// operating-system administrators.
pub fn ensure_private_directory(path: &Path) -> StateResult<()> {
    reject_symlink_components(path)?;
    fs::create_dir_all(path).map_err(|error| StateError::io(path, error))?;
    reject_symlink_components(path)?;
    set_directory_permissions(path)
}

/// Restricts a file to the current identity and operating-system administrators.
pub fn ensure_private_file(path: &Path) -> StateResult<()> {
    reject_symlink_components(path)?;
    set_file_permissions(path)
}

/// Verifies that an existing input file is private without changing its access policy.
pub fn verify_private_file(path: &Path) -> StateResult<()> {
    reject_symlink_components(path)?;
    let metadata = fs::metadata(path).map_err(|error| StateError::io(path, error))?;
    if !metadata.is_file() {
        return Err(StateError::InvalidRecord(format!(
            "private input is not a regular file: {}",
            path.display()
        )));
    }
    verify_file_permissions(path, &metadata)
}

/// Rejects every existing symbolic-link component in a state path. Missing trailing
/// components are allowed so callers can validate paths before creation.
pub fn reject_symlink_components(path: &Path) -> StateResult<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => current.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(StateError::UnsafeArtifactPath(path.display().to_string()));
            }
            Component::Normal(part) => {
                current.push(part);
                match fs::symlink_metadata(&current) {
                    Ok(metadata) if is_link_like(&metadata) => {
                        return Err(StateError::SymlinkEscape(current));
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(StateError::io(&current, error)),
                }
            }
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn is_link_like(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn is_link_like(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;

    // Junctions and other reparse points can redirect ACL operations even when Rust
    // does not classify them as symbolic links.
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(unix)]
fn set_directory_permissions(path: &Path) -> StateResult<()> {
    use std::os::unix::fs::PermissionsExt as _;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| StateError::io(path, error))
}

#[cfg(windows)]
fn set_directory_permissions(path: &Path) -> StateResult<()> {
    set_windows_permissions(path, WindowsArtifactKind::Directory)
}

#[cfg(all(not(unix), not(windows)))]
fn set_directory_permissions(path: &Path) -> StateResult<()> {
    fs::metadata(path)
        .map(|_| ())
        .map_err(|error| StateError::io(path, error))
}

#[cfg(unix)]
fn set_file_permissions(path: &Path) -> StateResult<()> {
    use std::os::unix::fs::PermissionsExt as _;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| StateError::io(path, error))
}

#[cfg(windows)]
fn set_file_permissions(path: &Path) -> StateResult<()> {
    set_windows_permissions(path, WindowsArtifactKind::File)
}

#[cfg(all(not(unix), not(windows)))]
fn set_file_permissions(path: &Path) -> StateResult<()> {
    fs::metadata(path)
        .map(|_| ())
        .map_err(|error| StateError::io(path, error))
}

#[cfg(unix)]
fn verify_file_permissions(path: &Path, metadata: &fs::Metadata) -> StateResult<()> {
    use std::os::unix::fs::PermissionsExt as _;

    if metadata.permissions().mode().trailing_zeros() >= 6 {
        Ok(())
    } else {
        Err(StateError::InvalidRecord(format!(
            "private input is readable or writable by group/other users: {}",
            path.display()
        )))
    }
}

#[cfg(windows)]
fn verify_file_permissions(path: &Path, _metadata: &fs::Metadata) -> StateResult<()> {
    let _acl_guard = windows_acl_guard()?;
    let target = canonical_acl_target(path)?;
    let icacls = trusted_system_utility("icacls.exe")?;
    let descriptor = load_dacl(&icacls, &target)?;
    verify_private_dacl(
        &descriptor,
        &current_windows_identity()?,
        WindowsArtifactKind::File,
    )
}

#[cfg(all(not(unix), not(windows)))]
fn verify_file_permissions(_path: &Path, _metadata: &fs::Metadata) -> StateResult<()> {
    Ok(())
}

#[cfg(windows)]
#[derive(Clone, Copy)]
enum WindowsArtifactKind {
    Directory,
    File,
}

#[cfg(windows)]
const SYSTEM_SID: &str = "S-1-5-18";
#[cfg(windows)]
const ADMINISTRATORS_SID: &str = "S-1-5-32-544";
#[cfg(windows)]
const BROAD_ACCESS_SIDS: [&str; 3] = ["S-1-1-0", "S-1-5-11", "S-1-5-32-545"];
#[cfg(windows)]
const WINDOWS_TOOL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
#[cfg(windows)]
const MAX_WINDOWS_TOOL_OUTPUT: u64 = 64 * 1024;

#[cfg(windows)]
struct WindowsIdentity {
    sid: String,
    alias: Option<&'static str>,
}

#[cfg(windows)]
fn set_windows_permissions(path: &Path, kind: WindowsArtifactKind) -> StateResult<()> {
    use std::ffi::OsString;

    // `icacls` applies one mutation per process invocation. Serializing the complete
    // reset/grant/protect/verify sequence prevents another thread from observing or
    // overwriting an intentional intermediate DACL on the same state tree.
    let _acl_guard = windows_acl_guard()?;
    let target = canonical_acl_target(path)?;
    let icacls = trusted_system_utility("icacls.exe")?;
    let current_identity = current_windows_identity()?;
    let trusted_sids = [
        current_identity.sid.as_str(),
        SYSTEM_SID,
        ADMINISTRATORS_SID,
    ];

    let mut remove_denials = vec![OsString::from("/remove:d")];
    remove_denials.extend(
        trusted_sids
            .iter()
            .map(|sid| OsString::from(format!("*{sid}"))),
    );
    remove_denials.push(OsString::from("/q"));
    run_icacls(&icacls, &target, &remove_denials)?;

    let permission = match kind {
        WindowsArtifactKind::Directory => "(OI)(CI)F",
        WindowsArtifactKind::File => "F",
    };
    let mut grants = vec![OsString::from("/grant:r")];
    grants.extend(
        trusted_sids
            .iter()
            .map(|sid| OsString::from(format!("*{sid}:{permission}"))),
    );
    grants.push(OsString::from("/q"));
    run_icacls(&icacls, &target, &grants)?;

    // `/reset` replaces every arbitrary explicit ACE with the parent defaults. The
    // preliminary trusted grants above make the reset reachable even when the old DACL
    // contains a deny for one of the trusted principals. Re-apply those grants before
    // removing inheritance so the resulting protected DACL never relies on a broad
    // inherited principal.
    run_icacls(
        &icacls,
        &target,
        &[OsString::from("/reset"), OsString::from("/q")],
    )?;
    run_icacls(&icacls, &target, &grants)?;
    run_icacls(
        &icacls,
        &target,
        &[OsString::from("/inheritance:r"), OsString::from("/q")],
    )?;

    let mut remove_broad_grants = vec![OsString::from("/remove:g")];
    remove_broad_grants.extend(
        BROAD_ACCESS_SIDS
            .iter()
            .map(|sid| OsString::from(format!("*{sid}"))),
    );
    remove_broad_grants.push(OsString::from("/q"));
    run_icacls(&icacls, &target, &remove_broad_grants)?;

    run_icacls(
        &icacls,
        &target,
        &[OsString::from("/verify"), OsString::from("/q")],
    )?;
    let descriptor = load_dacl(&icacls, &target)?;
    verify_private_dacl(&descriptor, &current_identity, kind)
}

#[cfg(windows)]
fn windows_acl_guard() -> StateResult<std::sync::MutexGuard<'static, ()>> {
    use std::sync::{Mutex, OnceLock};

    static WINDOWS_ACL_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    WINDOWS_ACL_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| permission_error("Windows ACL hardening lock was poisoned"))
}

#[cfg(windows)]
fn canonical_acl_target(path: &Path) -> StateResult<PathBuf> {
    reject_symlink_components(path)?;
    let target = fs::canonicalize(path).map_err(|error| StateError::io(path, error))?;
    reject_symlink_components(&target)?;
    let metadata = fs::symlink_metadata(&target).map_err(|error| StateError::io(&target, error))?;
    if is_link_like(&metadata) {
        return Err(StateError::SymlinkEscape(target));
    }
    Ok(target)
}

#[cfg(windows)]
fn trusted_system_utility(file_name: &str) -> StateResult<PathBuf> {
    let root = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| permission_error("SystemRoot is absent or is not absolute"))?;
    reject_symlink_components(&root)?;
    let root = fs::canonicalize(&root).map_err(|error| StateError::io(&root, error))?;
    let system32_input = root.join("System32");
    reject_symlink_components(&system32_input)?;
    let system32 = fs::canonicalize(&system32_input)
        .map_err(|error| StateError::io(&system32_input, error))?;
    if system32.parent() != Some(root.as_path()) {
        return Err(permission_error(
            "System32 escaped the canonical Windows root",
        ));
    }

    let utility_input = system32.join(file_name);
    reject_symlink_components(&utility_input)?;
    let utility =
        fs::canonicalize(&utility_input).map_err(|error| StateError::io(&utility_input, error))?;
    let has_expected_name = utility
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case(file_name));
    if utility.parent() != Some(system32.as_path()) || !has_expected_name {
        return Err(permission_error(format!(
            "Windows utility escaped the trusted System32 directory: {}",
            utility.display()
        )));
    }
    let metadata = fs::metadata(&utility).map_err(|error| StateError::io(&utility, error))?;
    if !metadata.is_file() {
        return Err(permission_error(format!(
            "Windows utility is not a regular file: {}",
            utility.display()
        )));
    }
    Ok(utility)
}

#[cfg(windows)]
fn current_windows_identity() -> StateResult<WindowsIdentity> {
    use std::ffi::OsString;

    let whoami = trusted_system_utility("whoami.exe")?;
    let output = run_windows_utility(
        &whoami,
        &[
            OsString::from("/user"),
            OsString::from("/fo"),
            OsString::from("csv"),
            OsString::from("/nh"),
        ],
    )?;
    let sid = extract_sid(&output.stdout)
        .ok_or_else(|| permission_error("whoami returned no valid current-user SID"))?;
    let alias = if has_account_rid(&sid, "500") {
        let account_authority = extract_account_authority(&output.stdout).ok_or_else(|| {
            permission_error("whoami returned no valid current-user account authority")
        })?;
        let hostname = trusted_system_utility("hostname.exe")?;
        let hostname = run_windows_utility(&hostname, &[])?;
        local_administrator_alias(&sid, account_authority, &hostname.stdout)
    } else {
        None
    };
    Ok(WindowsIdentity { sid, alias })
}

#[cfg(windows)]
fn extract_account_authority(output: &[u8]) -> Option<&[u8]> {
    let field_start = output.iter().position(|byte| *byte == b'"')? + 1;
    let field_end = output[field_start..]
        .iter()
        .position(|byte| *byte == b'"')?
        + field_start;
    let separator = output[field_start..field_end]
        .iter()
        .position(|byte| *byte == b'\\')?
        + field_start;
    (separator > field_start).then_some(&output[field_start..separator])
}

#[cfg(windows)]
fn local_administrator_alias(
    sid: &str,
    account_authority: &[u8],
    hostname: &[u8],
) -> Option<&'static str> {
    (has_account_rid(sid, "500") && account_authority.eq_ignore_ascii_case(hostname.trim_ascii()))
        .then_some("LA")
}

#[cfg(windows)]
fn has_account_rid(sid: &str, expected_rid: &str) -> bool {
    sid.strip_prefix("S-1-5-21-")
        .and_then(|suffix| suffix.rsplit_once('-'))
        .is_some_and(|(authority, rid)| !authority.is_empty() && rid == expected_rid)
}

#[cfg(windows)]
fn extract_sid(output: &[u8]) -> Option<String> {
    let mut offset = 0;
    let mut found = None;
    while offset + 4 <= output.len() {
        let Some(relative) = output[offset..]
            .windows(4)
            .position(|window| window.eq_ignore_ascii_case(b"S-1-"))
        else {
            break;
        };
        let start = offset + relative;
        let end = output[start..]
            .iter()
            .position(|byte| {
                !byte.is_ascii_digit() && *byte != b'-' && !byte.eq_ignore_ascii_case(&b'S')
            })
            .map_or(output.len(), |length| start + length);
        let candidate = std::str::from_utf8(&output[start..end]).ok()?;
        if valid_sid(candidate) {
            found = Some(candidate.to_ascii_uppercase());
        }
        offset = end.max(start + 1);
    }
    found
}

#[cfg(windows)]
fn valid_sid(candidate: &str) -> bool {
    if candidate.len() > 184 || !candidate.starts_with("S-1-") {
        return false;
    }
    let parts = candidate[2..].split('-').collect::<Vec<_>>();
    parts.len() >= 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

#[cfg(windows)]
struct WindowsToolOutput {
    stdout: Vec<u8>,
}

#[cfg(windows)]
fn run_windows_utility(
    executable: &Path,
    args: &[std::ffi::OsString],
) -> StateResult<WindowsToolOutput> {
    use std::{
        io::{Seek as _, SeekFrom},
        process::{Command, Stdio},
        thread,
        time::Instant,
    };

    let mut stdout = tempfile::tempfile().map_err(|error| StateError::io(executable, error))?;
    let mut stderr = tempfile::tempfile().map_err(|error| StateError::io(executable, error))?;
    let child_stdout = stdout
        .try_clone()
        .map_err(|error| StateError::io(executable, error))?;
    let child_stderr = stderr
        .try_clone()
        .map_err(|error| StateError::io(executable, error))?;
    let system_root =
        std::env::var_os("SystemRoot").ok_or_else(|| permission_error("SystemRoot is absent"))?;
    let mut child = Command::new(executable)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(child_stdout))
        .stderr(Stdio::from(child_stderr))
        .env_clear()
        .env("SystemRoot", &system_root)
        .env("WINDIR", &system_root)
        .spawn()
        .map_err(|error| StateError::io(executable, error))?;

    let started = Instant::now();
    let status = loop {
        match child
            .try_wait()
            .map_err(|error| StateError::io(executable, error))?
        {
            Some(status) => break status,
            None if started.elapsed() < WINDOWS_TOOL_TIMEOUT => {
                thread::sleep(std::time::Duration::from_millis(10));
            }
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(permission_error(format!(
                    "Windows permission utility timed out: {}",
                    executable.display()
                )));
            }
        }
    };

    stdout
        .seek(SeekFrom::Start(0))
        .map_err(|error| StateError::io(executable, error))?;
    stderr
        .seek(SeekFrom::Start(0))
        .map_err(|error| StateError::io(executable, error))?;
    let stdout = read_bounded(&mut stdout, executable)?;
    let stderr = read_bounded(&mut stderr, executable)?;
    if !status.success() {
        let diagnostic = if stderr.is_empty() { &stdout } else { &stderr };
        return Err(permission_error(format!(
            "Windows permission utility {} failed with {}: {}",
            executable.display(),
            status,
            sanitize_diagnostic(diagnostic)
        )));
    }
    Ok(WindowsToolOutput { stdout })
}

#[cfg(windows)]
fn read_bounded(file: &mut fs::File, context: &Path) -> StateResult<Vec<u8>> {
    use std::io::Read as _;

    let length = file
        .metadata()
        .map_err(|error| StateError::io(context, error))?
        .len();
    if length > MAX_WINDOWS_TOOL_OUTPUT {
        return Err(permission_error(format!(
            "Windows permission utility output exceeded {MAX_WINDOWS_TOOL_OUTPUT} bytes"
        )));
    }
    let mut bytes = Vec::new();
    file.take(MAX_WINDOWS_TOOL_OUTPUT + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| StateError::io(context, error))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_WINDOWS_TOOL_OUTPUT {
        return Err(permission_error(format!(
            "Windows permission utility output exceeded {MAX_WINDOWS_TOOL_OUTPUT} bytes"
        )));
    }
    Ok(bytes)
}

#[cfg(windows)]
fn sanitize_diagnostic(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .trim()
        .to_owned()
}

#[cfg(windows)]
fn run_icacls(
    executable: &Path,
    target: &Path,
    operation: &[std::ffi::OsString],
) -> StateResult<()> {
    let mut args = Vec::with_capacity(operation.len() + 1);
    args.push(target.as_os_str().to_owned());
    args.extend_from_slice(operation);
    run_windows_utility(executable, &args).map(|_| ())
}

#[cfg(windows)]
fn load_dacl(icacls: &Path, target: &Path) -> StateResult<String> {
    use std::ffi::OsString;

    let temporary = tempfile::Builder::new()
        .prefix("orchestrator-acl-")
        .tempdir()
        .map_err(|error| StateError::io(target, error))?;
    let saved_acl = temporary.path().join("dacl.txt");
    run_icacls(
        icacls,
        target,
        &[
            OsString::from("/save"),
            saved_acl.as_os_str().to_owned(),
            OsString::from("/q"),
        ],
    )?;
    let mut file = fs::File::open(&saved_acl).map_err(|error| StateError::io(&saved_acl, error))?;
    let bytes = read_bounded(&mut file, &saved_acl)?;
    decode_windows_text(&bytes)
}

#[cfg(windows)]
fn decode_windows_text(bytes: &[u8]) -> StateResult<String> {
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return decode_utf16(&bytes[2..], true);
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        return decode_utf16(&bytes[2..], false);
    }
    if bytes.len() >= 4
        && bytes
            .iter()
            .skip(1)
            .step_by(2)
            .filter(|byte| **byte == 0)
            .count()
            * 2
            > bytes.len() / 2
    {
        return decode_utf16(bytes, true);
    }
    String::from_utf8(bytes.to_vec())
        .map_err(|_| permission_error("icacls saved an ACL in an unsupported text encoding"))
}

#[cfg(windows)]
fn decode_utf16(bytes: &[u8], little_endian: bool) -> StateResult<String> {
    if !bytes.len().is_multiple_of(2) {
        return Err(permission_error("icacls saved a truncated UTF-16 ACL"));
    }
    let words = bytes
        .chunks_exact(2)
        .map(|chunk| {
            if little_endian {
                u16::from_le_bytes([chunk[0], chunk[1]])
            } else {
                u16::from_be_bytes([chunk[0], chunk[1]])
            }
        })
        .collect::<Vec<_>>();
    String::from_utf16(&words).map_err(|_| permission_error("icacls saved an invalid UTF-16 ACL"))
}

#[cfg(windows)]
fn verify_private_dacl(
    saved_acl: &str,
    current_identity: &WindowsIdentity,
    kind: WindowsArtifactKind,
) -> StateResult<()> {
    let descriptor = saved_acl
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("D:") && line.contains('('))
        .ok_or_else(|| permission_error("icacls output contained no DACL descriptor"))?;
    let first_ace = descriptor
        .find('(')
        .ok_or_else(|| permission_error("icacls DACL contained no access-control entries"))?;
    if !descriptor[2..first_ace].contains('P') {
        return Err(permission_error("Windows DACL inheritance remains enabled"));
    }
    let aces = parse_aces(&descriptor[first_ace..])?;
    let required = [
        (current_identity.sid.as_str(), current_identity.alias),
        (SYSTEM_SID, Some("SY")),
        (ADMINISTRATORS_SID, Some("BA")),
    ];

    for ace in &aces {
        if is_deny_ace(ace.ace_type) {
            return Err(permission_error(format!(
                "Windows DACL retains a deny ACE for {}",
                ace.trustee
            )));
        }
        if !is_allow_ace(ace.ace_type) {
            return Err(permission_error(format!(
                "Windows DACL contains unsupported ACE type {}",
                ace.ace_type
            )));
        }
        let trusted = required
            .iter()
            .any(|(sid, alias)| trustee_matches(ace.trustee, sid, *alias));
        if !trusted {
            return Err(permission_error(format!(
                "Windows DACL grants access to untrusted principal {}",
                ace.trustee
            )));
        }
    }

    for (sid, alias) in required {
        let full_control = aces.iter().any(|ace| {
            is_allow_ace(ace.ace_type)
                && ace.rights == "FA"
                && trustee_matches(ace.trustee, sid, alias)
                && match kind {
                    WindowsArtifactKind::Directory => {
                        ace.flags.contains("OI") && ace.flags.contains("CI")
                    }
                    WindowsArtifactKind::File => true,
                }
        });
        if !full_control {
            return Err(permission_error(format!(
                "Windows DACL does not preserve verified full control for {sid}"
            )));
        }
    }

    let broad = [
        (BROAD_ACCESS_SIDS[0], "WD"),
        (BROAD_ACCESS_SIDS[1], "AU"),
        (BROAD_ACCESS_SIDS[2], "BU"),
    ];
    for (sid, alias) in broad {
        if aces
            .iter()
            .any(|ace| is_allow_ace(ace.ace_type) && trustee_matches(ace.trustee, sid, Some(alias)))
        {
            return Err(permission_error(format!(
                "Windows DACL still grants access to broad principal {sid}"
            )));
        }
    }
    Ok(())
}

#[cfg(windows)]
struct ParsedAce<'a> {
    ace_type: &'a str,
    flags: &'a str,
    rights: &'a str,
    trustee: &'a str,
}

#[cfg(windows)]
fn parse_aces(descriptor: &str) -> StateResult<Vec<ParsedAce<'_>>> {
    let mut result = Vec::new();
    let mut depth = 0_u32;
    let mut start = None;
    for (offset, character) in descriptor.char_indices() {
        match character {
            '(' => {
                if depth == 0 {
                    start = Some(offset + 1);
                }
                depth = depth.saturating_add(1);
            }
            ')' => {
                if depth == 0 {
                    return Err(permission_error(
                        "icacls DACL has unbalanced ACE delimiters",
                    ));
                }
                depth -= 1;
                if depth == 0 {
                    let body_start = start
                        .take()
                        .ok_or_else(|| permission_error("icacls DACL has an invalid ACE"))?;
                    let fields = descriptor[body_start..offset]
                        .split(';')
                        .collect::<Vec<_>>();
                    if fields.len() < 6 {
                        return Err(permission_error("icacls DACL has a malformed ACE"));
                    }
                    result.push(ParsedAce {
                        ace_type: fields[0],
                        flags: fields[1],
                        rights: fields[2],
                        trustee: fields[5],
                    });
                }
            }
            _ => {}
        }
    }
    if depth != 0 || result.is_empty() {
        return Err(permission_error("icacls DACL has incomplete ACE data"));
    }
    Ok(result)
}

#[cfg(windows)]
fn is_allow_ace(ace_type: &str) -> bool {
    matches!(ace_type, "A" | "OA" | "XA" | "ZA")
}

#[cfg(windows)]
fn is_deny_ace(ace_type: &str) -> bool {
    matches!(ace_type, "D" | "OD" | "XD" | "ZD")
}

#[cfg(windows)]
fn trustee_matches(trustee: &str, sid: &str, alias: Option<&str>) -> bool {
    trustee.eq_ignore_ascii_case(sid)
        || alias.is_some_and(|alias| trustee.eq_ignore_ascii_case(alias))
}

#[cfg(windows)]
fn permission_error(message: impl Into<String>) -> StateError {
    StateError::InvalidRecord(format!(
        "Windows permission hardening failed: {}",
        message.into()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn unix_private_permissions_remain_owner_only() -> StateResult<()> {
        use std::os::unix::fs::PermissionsExt as _;

        let temporary = tempfile::tempdir().map_err(|error| StateError::io("tempdir", error))?;
        let directory = temporary.path().join("state");
        ensure_private_directory(&directory)?;
        let file = directory.join("state.json");
        fs::write(&file, b"{}\n").map_err(|error| StateError::io(&file, error))?;
        ensure_private_file(&file)?;
        verify_private_file(&file)?;

        let directory_mode = fs::metadata(&directory)
            .map_err(|error| StateError::io(&directory, error))?
            .permissions()
            .mode()
            & 0o777;
        let file_mode = fs::metadata(&file)
            .map_err(|error| StateError::io(&file, error))?
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(directory_mode, 0o700);
        assert_eq!(file_mode, 0o600);
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn windows_private_file_removes_untrusted_temp_file_grants() -> StateResult<()> {
        use std::ffi::OsString;

        let temporary = tempfile::tempdir().map_err(|error| StateError::io("tempdir", error))?;
        let directory = temporary.path().join("state");
        ensure_private_directory(&directory)?;
        let file = directory.join("state.json");
        fs::write(&file, b"{}\r\n").map_err(|error| StateError::io(&file, error))?;

        let target = canonical_acl_target(&file)?;
        let icacls = trusted_system_utility("icacls.exe")?;
        run_icacls(
            &icacls,
            &target,
            &[
                OsString::from("/grant"),
                OsString::from("*S-1-5-32-545:R"),
                OsString::from("*S-1-5-4:R"),
                OsString::from("/q"),
            ],
        )?;
        ensure_private_file(&file)?;
        verify_private_file(&file)?;

        let descriptor = load_dacl(&icacls, &target)?;
        verify_private_dacl(
            &descriptor,
            &current_windows_identity()?,
            WindowsArtifactKind::File,
        )
    }

    #[cfg(windows)]
    #[test]
    fn windows_private_temp_directory_is_idempotent() -> StateResult<()> {
        let temporary = tempfile::tempdir().map_err(|error| StateError::io("tempdir", error))?;
        let directory = temporary.path().join("state");
        ensure_private_directory(&directory)?;
        ensure_private_directory(&directory)?;

        let target = canonical_acl_target(&directory)?;
        let descriptor = load_dacl(&trusted_system_utility("icacls.exe")?, &target)?;
        verify_private_dacl(
            &descriptor,
            &current_windows_identity()?,
            WindowsArtifactKind::Directory,
        )
    }

    #[cfg(windows)]
    #[test]
    fn windows_local_administrator_alias_requires_local_rid_500() {
        let local_admin_sid = "S-1-5-21-111-222-333-500";
        assert_eq!(
            local_administrator_alias(local_admin_sid, b"GITHUB-RUNNER", b"github-runner\r\n"),
            Some("LA")
        );
        assert_eq!(
            local_administrator_alias(local_admin_sid, b"DOMAIN", b"github-runner\r\n"),
            None
        );
        assert_eq!(
            local_administrator_alias(
                "S-1-5-21-111-222-333-1001",
                b"GITHUB-RUNNER",
                b"github-runner\r\n",
            ),
            None
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_local_administrator_alias_is_accepted_as_current_identity() -> StateResult<()> {
        let descriptor = "D:P(A;;FA;;;LA)(A;;FA;;;SY)(A;;FA;;;BA)";
        let identity = WindowsIdentity {
            sid: "S-1-5-21-111-222-333-500".to_owned(),
            alias: Some("LA"),
        };
        verify_private_dacl(descriptor, &identity, WindowsArtifactKind::File)?;

        let ordinary_identity = WindowsIdentity {
            sid: "S-1-5-21-111-222-333-1001".to_owned(),
            alias: None,
        };
        assert!(
            verify_private_dacl(descriptor, &ordinary_identity, WindowsArtifactKind::File).is_err()
        );
        Ok(())
    }
}
