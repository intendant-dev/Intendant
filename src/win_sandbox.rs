//! Windows filesystem sandboxing via restricted tokens — the Windows twin of
//! the Linux Landlock and macOS Seatbelt postures.
//!
//! Windows has no unprivileged, path-based sandbox primitive. What it has is
//! **restricted tokens** (`CreateRestrictedToken`): an access check against a
//! restricted token passes only if BOTH the normal user/group pass AND at
//! least one *restricting SID* is granted the requested access. Combining a
//! restricted token with temporary ACL entries granting the well-known
//! `RESTRICTED` SID (S-1-5-12) access on chosen roots yields a path
//! allowlist:
//!
//! - **Write-only restriction** (the agent-runtime posture — reads open,
//!   writes confined): `WRITE_RESTRICTED` tokens apply the restricting-SID
//!   check to write access only. Stamp `RESTRICTED`-write ACEs on the
//!   allowed write roots; everything else stays readable, nothing else is
//!   writable.
//! - **Full restriction** (the scoped-shell posture — deny-by-default):
//!   restricting SIDs `[RESTRICTED, BUILTIN\Users, Everyone]` apply to
//!   every access. System directories carry `Users` read ACEs so the shell
//!   can start; `Everyone` covers device objects (`NUL`, `CON` — every
//!   shell redirect needs them); user profiles carry neither, so
//!   `%USERPROFILE%` and every other profile read as denied; stamped
//!   `RESTRICTED` ACEs on the scope roots open exactly the granted
//!   subtrees. Accepted surface: `Users`-writable OS spots (directory
//!   creation at `C:\`, parts of `ProgramData`) remain writable — user
//!   data does not.
//!
//! Both postures also pass `DISABLE_MAX_PRIVILEGE`: restricted tokens keep
//! the parent's privileges unless told otherwise, and elevated parents
//! (OpenSSH admin sessions run with everything enabled) would hand the
//! sandbox `SeBackupPrivilege` / `SeRestorePrivilege` — which bypass DACLs
//! entirely for backup-intent opens (`FindFirstFile` enumerates directories
//! that way, so `dir %USERPROFILE%` walks straight through the restricting
//! SIDs) — plus `SeDebugPrivilege` and worse. The flag strips every
//! privilege except `SeChangeNotifyPrivilege`, whose traverse-bypass the
//! allowlist model needs (verified by unit test).
//!
//! ACL stamping mutates the target directories' DACLs, so every grant is
//! paired with removal (exact-ACE match, not trustee-wide revocation) and
//! journaled to disk first — a crash between stamp and removal leaves a
//! journal entry that [`sweep_stale_journals`] replays on the next start.
//!
//! Tokens cannot be applied to a running process, only at `CreateProcess*`
//! time, and neither `std` nor `tokio` `Command` can pass one — so both
//! consumers re-exec: the runtime re-launches itself under the restricted
//! token before reading stdin (`reexec_write_restricted_if_configured`), and
//! scoped shells go through the caller's `--scoped-shell-exec` wrapper. The
//! `CreateProcessAsUserW` call needs no privilege for this: a restricted
//! version of the caller's own primary token is exempt from
//! `SeAssignPrimaryTokenPrivilege`.
//!
//! This module is compiled into BOTH binaries (`src/main.rs` mounts it
//! natively; the caller mounts it via `#[path]`) and is self-contained:
//! plain `Result<_, String>` errors, no crate-local types.

use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, SetHandleInformation, ERROR_SUCCESS, HANDLE,
    HANDLE_FLAG_INHERIT, HLOCAL, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows::Win32::Security::Authorization::{
    GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW, EXPLICIT_ACCESS_W,
    NO_MULTIPLE_TRUSTEE, SET_ACCESS, SE_FILE_OBJECT, TRUSTEE_IS_GROUP, TRUSTEE_IS_SID, TRUSTEE_W,
};
use windows::Win32::Security::{
    AclSizeInformation, CreateRestrictedToken, CreateWellKnownSid, DeleteAce, EqualSid, GetAce,
    GetAclInformation, WinBuiltinUsersSid, WinRestrictedCodeSid, WinWorldSid, ACCESS_ALLOWED_ACE,
    ACE_FLAGS, ACL as WIN_ACL, ACL_SIZE_INFORMATION, CONTAINER_INHERIT_ACE,
    DACL_SECURITY_INFORMATION, DISABLE_MAX_PRIVILEGE, OBJECT_INHERIT_ACE, PSECURITY_DESCRIPTOR,
    PSID, SECURITY_MAX_SID_SIZE, SID_AND_ATTRIBUTES, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE,
    TOKEN_QUERY, WELL_KNOWN_SID_TYPE, WRITE_RESTRICTED,
};
use windows::Win32::Storage::FileSystem::{
    DELETE, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
};
use windows::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows::Win32::System::Environment::GetCommandLineW;
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess, OpenProcessToken,
    WaitForSingleObject, CREATE_UNICODE_ENVIRONMENT, INFINITE, PROCESS_CREATION_FLAGS,
    PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOW,
};

/// Marker env var: present in the restricted re-exec child so it does not
/// re-restrict recursively.
pub const SANDBOX_APPLIED_ENV: &str = "INTENDANT_SANDBOX_APPLIED";

/// `FILE_DELETE_CHILD` is not exported as a constant by the crate feature
/// set we use; its value is stable Win32 ABI (0x40).
const FILE_DELETE_CHILD: u32 = 0x40;

/// Access mask for read-lane grants: read + traverse/execute.
fn read_mask() -> u32 {
    FILE_GENERIC_READ.0 | FILE_GENERIC_EXECUTE.0
}

/// Access mask for write-lane grants: the "Modify" bundle — read, write,
/// execute, delete (self and children).
fn write_mask() -> u32 {
    FILE_GENERIC_READ.0
        | FILE_GENERIC_WRITE.0
        | FILE_GENERIC_EXECUTE.0
        | DELETE.0
        | FILE_DELETE_CHILD
}

fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

fn last_err(context: &str) -> String {
    // SAFETY: GetLastError has no preconditions.
    let code = unsafe { GetLastError() };
    format!("{context}: win32 error {}", code.0)
}

/// An owned well-known SID buffer.
struct Sid {
    buf: Vec<u8>,
}

impl Sid {
    fn well_known(kind: WELL_KNOWN_SID_TYPE) -> Result<Self, String> {
        let mut buf = vec![0u8; SECURITY_MAX_SID_SIZE as usize];
        let mut len = buf.len() as u32;
        // SAFETY: buf is a writable buffer of len bytes; CreateWellKnownSid
        // writes a SID of at most SECURITY_MAX_SID_SIZE into it.
        unsafe { CreateWellKnownSid(kind, None, Some(PSID(buf.as_mut_ptr() as *mut _)), &mut len) }
            .map_err(|e| format!("CreateWellKnownSid: {e}"))?;
        buf.truncate(len as usize);
        Ok(Self { buf })
    }

    fn as_psid(&self) -> PSID {
        PSID(self.buf.as_ptr() as *mut _)
    }
}

/// RAII wrapper closing a raw HANDLE on drop.
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: we own this handle; it is closed exactly once here.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// RAII wrapper for LocalAlloc'd memory returned by security APIs.
struct LocalBuf(HLOCAL);

impl Drop for LocalBuf {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: the pointer came from a Win32 API that allocates with
            // LocalAlloc and documents LocalFree as the release path.
            unsafe {
                let _ = LocalFree(Some(self.0));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ACE stamping and removal
// ---------------------------------------------------------------------------

/// One stamped grant: `RESTRICTED` gets `mask` on `path` (inheritable).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StampedAce {
    pub path: PathBuf,
    pub mask: u32,
}

/// Add an inheritable allow-ACE for the RESTRICTED SID on `path`.
fn add_restricted_ace(path: &Path, mask: u32) -> Result<(), String> {
    let restricted = Sid::well_known(WinRestrictedCodeSid)?;
    let wide_path = wide(path.as_os_str());

    let mut old_dacl: *mut WIN_ACL = std::ptr::null_mut();
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: wide_path is a NUL-terminated wide string that outlives the
    // call; out-pointers are valid; the returned security descriptor is
    // freed by LocalBuf below (the DACL points into it).
    let status = unsafe {
        GetNamedSecurityInfoW(
            PCWSTR(wide_path.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut old_dacl),
            None,
            &mut sd,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(format!(
            "GetNamedSecurityInfoW({}): win32 error {}",
            path.display(),
            status.0
        ));
    }
    let _sd_guard = LocalBuf(HLOCAL(sd.0));

    let explicit = EXPLICIT_ACCESS_W {
        grfAccessPermissions: mask,
        grfAccessMode: SET_ACCESS,
        grfInheritance: ACE_FLAGS(OBJECT_INHERIT_ACE.0 | CONTAINER_INHERIT_ACE.0),
        Trustee: TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_GROUP,
            ptstrName: PWSTR(restricted.as_psid().0 as *mut u16),
        },
    };
    let mut new_dacl: *mut WIN_ACL = std::ptr::null_mut();
    // SAFETY: explicit references the restricted SID buffer which outlives
    // the call; old_dacl points into the live security descriptor; new_dacl
    // receives a LocalAlloc'd ACL freed by LocalBuf below.
    let status = unsafe {
        SetEntriesInAclW(
            Some(&[explicit]),
            if old_dacl.is_null() {
                None
            } else {
                Some(old_dacl)
            },
            &mut new_dacl,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(format!(
            "SetEntriesInAclW({}): win32 error {}",
            path.display(),
            status.0
        ));
    }
    let _new_guard = LocalBuf(HLOCAL(new_dacl as *mut _));

    let mut wide_path_mut = wide(path.as_os_str());
    // SAFETY: the path buffer and new_dacl are valid for the duration of the
    // call; we only replace the DACL.
    let status = unsafe {
        SetNamedSecurityInfoW(
            PWSTR(wide_path_mut.as_mut_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(new_dacl),
            None,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(format!(
            "SetNamedSecurityInfoW({}): win32 error {}",
            path.display(),
            status.0
        ));
    }
    Ok(())
}

/// Remove the exact ACE `add_restricted_ace` stamped (RESTRICTED SID, same
/// mask, inheritable allow) — not a trustee-wide revocation, so an operator's
/// own pre-existing RESTRICTED entries survive. Missing path or absent ACE
/// count as success (the goal state is "not granted by us").
fn remove_restricted_ace(path: &Path, mask: u32) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    let restricted = Sid::well_known(WinRestrictedCodeSid)?;
    let wide_path = wide(path.as_os_str());

    let mut dacl: *mut WIN_ACL = std::ptr::null_mut();
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: as in add_restricted_ace.
    let status = unsafe {
        GetNamedSecurityInfoW(
            PCWSTR(wide_path.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut dacl),
            None,
            &mut sd,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(format!(
            "GetNamedSecurityInfoW({}): win32 error {}",
            path.display(),
            status.0
        ));
    }
    let _sd_guard = LocalBuf(HLOCAL(sd.0));
    if dacl.is_null() {
        return Ok(());
    }

    let mut info = ACL_SIZE_INFORMATION::default();
    // SAFETY: dacl points at a valid ACL inside the security descriptor;
    // info is a writable out-struct of the requested class.
    unsafe {
        GetAclInformation(
            dacl,
            &mut info as *mut _ as *mut _,
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    }
    .map_err(|e| format!("GetAclInformation({}): {e}", path.display()))?;

    let mut removed = false;
    // Iterate downward so DeleteAce index shifts don't skip entries.
    for i in (0..info.AceCount).rev() {
        let mut ace_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
        // SAFETY: index is within AceCount for this ACL.
        if unsafe { GetAce(dacl, i, &mut ace_ptr) }.is_err() {
            continue;
        }
        // SAFETY: GetAce returned a pointer to an ACE header inside the ACL.
        let header = unsafe { &*(ace_ptr as *const ACCESS_ALLOWED_ACE) };
        // ACCESS_ALLOWED_ACE_TYPE — stable Win32 ABI value (0x0); the crate
        // does not export it under our feature set.
        if header.Header.AceType != 0u8 {
            continue;
        }
        if header.Mask != mask {
            continue;
        }
        if header.Header.AceFlags & (OBJECT_INHERIT_ACE.0 | CONTAINER_INHERIT_ACE.0) as u8
            != (OBJECT_INHERIT_ACE.0 | CONTAINER_INHERIT_ACE.0) as u8
        {
            continue;
        }
        let sid_ptr = PSID(&header.SidStart as *const _ as *mut _);
        // SAFETY: both SIDs are valid for the comparison; EqualSid reads only.
        let equal = unsafe { EqualSid(sid_ptr, restricted.as_psid()) }.is_ok();
        if equal {
            // SAFETY: index valid; deleting inside the ACL buffer we hold.
            unsafe { DeleteAce(dacl, i) }
                .map_err(|e| format!("DeleteAce({}): {e}", path.display()))?;
            removed = true;
        }
    }
    if !removed {
        return Ok(());
    }

    let mut wide_path_mut = wide(path.as_os_str());
    // SAFETY: as in add_restricted_ace; dacl was modified in place and is
    // still a valid ACL.
    let status = unsafe {
        SetNamedSecurityInfoW(
            PWSTR(wide_path_mut.as_mut_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(dacl),
            None,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(format!(
            "SetNamedSecurityInfoW({}): win32 error {}",
            path.display(),
            status.0
        ));
    }
    Ok(())
}

/// The daemon-wide grant table: (path, mask) → refcount. Stamping is
/// refcounted because grants collide — every sandboxed runtime spawn wants
/// the same project/log/temp paths, and overlapping scoped shells can share
/// roots, while `SetEntriesInAclW(SET_ACCESS)` collapses identical ACEs
/// into one. Only the 0→1 transition stamps and only 1→0 removes, so a
/// grant never disappears under a live holder. The table is process-global;
/// the journal file mirrors the currently-stamped set for crash recovery.
struct GrantTable {
    counts: std::collections::HashMap<(PathBuf, u32), usize>,
}

static GRANTS: std::sync::Mutex<Option<GrantTable>> = std::sync::Mutex::new(None);

fn journal_path() -> PathBuf {
    journal_dir().join(format!("daemon-{}.json", std::process::id()))
}

/// Rewrite this process's journal to mirror the live grant table. Called
/// under the GRANTS lock.
fn rewrite_journal(table: &GrantTable) {
    let journal = journal_path();
    if table.counts.is_empty() {
        let _ = std::fs::remove_file(&journal);
        return;
    }
    let grants: Vec<StampedAce> = table
        .counts
        .keys()
        .map(|(path, mask)| StampedAce {
            path: path.clone(),
            mask: *mask,
        })
        .collect();
    if let Some(parent) = journal.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(payload) = serde_json::to_string(&grants) {
        let _ = std::fs::write(&journal, payload);
    }
}

/// RAII holder for a set of refcounted grants. Dropping releases them
/// (removing each ACE whose refcount reaches zero).
pub struct AceGuard {
    grants: Vec<StampedAce>,
}

impl AceGuard {
    /// Acquire `RESTRICTED` grants: `read_mask` on `read_roots`,
    /// `write_mask` on `write_roots`. Nonexistent roots are skipped (they
    /// allow nothing until they exist — the Landlock behavior). On failure,
    /// grants acquired so far are released.
    pub fn stamp(read_roots: &[PathBuf], write_roots: &[PathBuf]) -> Result<Self, String> {
        let mut wanted = Vec::new();
        for root in read_roots {
            if root.exists() {
                wanted.push(StampedAce {
                    path: root.clone(),
                    mask: read_mask(),
                });
            }
        }
        for root in write_roots {
            if root.exists() {
                wanted.push(StampedAce {
                    path: root.clone(),
                    mask: write_mask(),
                });
            }
        }

        let mut guard = Self { grants: Vec::new() };
        for grant in wanted {
            let mut table = GRANTS.lock().unwrap_or_else(|e| e.into_inner());
            let table = table.get_or_insert_with(|| GrantTable {
                counts: std::collections::HashMap::new(),
            });
            let key = (grant.path.clone(), grant.mask);
            let count = table.counts.entry(key).or_insert(0);
            if *count == 0 {
                // Journal BEFORE stamping so a crash mid-stamp still sweeps.
                *count = 1;
                rewrite_journal(table);
                if let Err(e) = add_restricted_ace(&grant.path, grant.mask) {
                    table.counts.remove(&(grant.path.clone(), grant.mask));
                    rewrite_journal(table);
                    drop(guard);
                    return Err(e);
                }
            } else {
                *count += 1;
            }
            guard.grants.push(grant);
        }
        Ok(guard)
    }

    fn release_all(&mut self) {
        for grant in self.grants.drain(..) {
            let mut table = GRANTS.lock().unwrap_or_else(|e| e.into_inner());
            let Some(table) = table.as_mut() else {
                continue;
            };
            let key = (grant.path.clone(), grant.mask);
            match table.counts.get_mut(&key) {
                Some(count) if *count > 1 => {
                    *count -= 1;
                }
                Some(_) => {
                    if let Err(e) = remove_restricted_ace(&grant.path, grant.mask) {
                        eprintln!(
                            "[win-sandbox] failed to remove ACE on {}: {e}",
                            grant.path.display()
                        );
                        // Keep the table entry (and journal line) so the
                        // next sweep retries.
                        continue;
                    }
                    table.counts.remove(&key);
                    rewrite_journal(table);
                }
                None => {}
            }
        }
    }
}

impl Drop for AceGuard {
    fn drop(&mut self) {
        self.release_all();
    }
}

/// True when `pid` names a live process — used so sweeps never strip ACEs
/// out from under another running daemon's shells. PID reuse can only make
/// this falsely report "alive", which delays the sweep, never corrupts it.
fn pid_alive(pid: u32) -> bool {
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    if pid == std::process::id() {
        return true;
    }
    // SAFETY: querying a pid; a failed open (gone or access denied for a
    // dead pid) is the "not alive" signal. Handle closed by OwnedHandle.
    match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
        Ok(handle) => {
            let handle = OwnedHandle(handle);
            let mut code = 0u32;
            // SAFETY: live handle; code is a valid out-pointer.
            let still_active =
                unsafe { GetExitCodeProcess(handle.0, &mut code) }.is_ok() && code == 259; // STILL_ACTIVE
            still_active
        }
        Err(_) => false,
    }
}

/// Replay and delete ACE journals left by daemons that crashed (or exited
/// without cleanup). Journals belonging to LIVE processes are skipped —
/// their grants are in use. Journals that fail to parse are removed (they
/// cannot be replayed); removal failures keep the journal for the next
/// sweep.
pub fn sweep_stale_journals(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        // Filenames are `daemon-<pid>.json`.
        let owner_pid = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| stem.rsplit('-').next())
            .and_then(|pid| pid.parse::<u32>().ok());
        if let Some(pid) = owner_pid {
            if pid_alive(pid) {
                continue;
            }
        }
        let Ok(payload) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(grants) = serde_json::from_str::<Vec<StampedAce>>(&payload) else {
            let _ = std::fs::remove_file(&path);
            continue;
        };
        let mut all_ok = true;
        for grant in grants {
            if remove_restricted_ace(&grant.path, grant.mask).is_err() {
                all_ok = false;
            }
        }
        if all_ok {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// The directory scoped-shell ACE journals live in (per-user, created on
/// demand): `%USERPROFILE%\.intendant\win-sandbox-journals`.
pub fn journal_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(".intendant")
        .join("win-sandbox-journals")
}

// ---------------------------------------------------------------------------
// Restricted tokens and spawning
// ---------------------------------------------------------------------------

/// Which posture the restricted token enforces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenRestriction {
    /// Restricting SIDs checked on write access only; reads stay open
    /// (agent-runtime parity with Landlock's read-`/` posture).
    WriteOnly,
    /// Restricting SIDs checked on every access — deny-by-default outside
    /// the stamped roots and `Users`-readable system paths (scoped shells).
    Full,
}

fn create_restricted_token(restriction: TokenRestriction) -> Result<OwnedHandle, String> {
    let mut primary = HANDLE::default();
    // SAFETY: current-process pseudo handle needs no close; primary receives
    // a real token handle owned by OwnedHandle below.
    unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY,
            &mut primary,
        )
    }
    .map_err(|e| format!("OpenProcessToken: {e}"))?;
    let primary = OwnedHandle(primary);

    let restricted_sid = Sid::well_known(WinRestrictedCodeSid)?;
    let world_sid = Sid::well_known(WinWorldSid)?;
    let users_sid = Sid::well_known(WinBuiltinUsersSid)?;
    // Everyone (World) rides along in BOTH postures for device objects:
    // without it, opening `NUL`/`CON` fails the restricting-SID pass and
    // every `>nul` redirect in shell commands dies (probed on Server 2022).
    // Everyone-write ACEs on the default filesystem are essentially absent,
    // so this reopens devices, not user data. (ALL APPLICATION PACKAGES
    // would have been a tighter read carrier than Users for the full
    // posture, but the kernel rejects capability SIDs in SidsToRestrict —
    // ERROR_INVALID_PARAMETER.)
    let mut restricting: Vec<SID_AND_ATTRIBUTES> = vec![
        SID_AND_ATTRIBUTES {
            Sid: restricted_sid.as_psid(),
            Attributes: 0,
        },
        SID_AND_ATTRIBUTES {
            Sid: world_sid.as_psid(),
            Attributes: 0,
        },
    ];
    // DISABLE_MAX_PRIVILEGE in both postures: the parent may be elevated
    // (an OpenSSH admin session has SeBackup/SeRestore/SeDebug/… ENABLED),
    // and restricted tokens inherit privileges verbatim. SeBackupPrivilege
    // alone defeats the DACL for reads — FindFirstFile opens directories
    // with backup intent, so profile listing sails past the restricting
    // SIDs; SeRestorePrivilege is the write-side twin. The flag deletes
    // everything except SeChangeNotifyPrivilege (traverse-bypass), which
    // path traversal to the granted roots relies on.
    let flags = match restriction {
        TokenRestriction::WriteOnly => WRITE_RESTRICTED | DISABLE_MAX_PRIVILEGE,
        TokenRestriction::Full => {
            // BUILTIN\Users is the system-read carrier: C:\Windows and
            // Program Files ACL it read+execute, user profiles never do —
            // so the shell starts while %USERPROFILE% and every other
            // profile stay denied. KNOWN, ACCEPTED SURFACE: Users also
            // holds create-directory ACEs on C:\ and write ACEs on parts
            // of ProgramData, so a scoped shell can write those OS-shared
            // spots (which any unsandboxed user code can write anyway).
            // User data stays sealed; path traversal through unreadable
            // intermediate dirs works via SeChangeNotifyPrivilege, which
            // restricted tokens keep.
            restricting.push(SID_AND_ATTRIBUTES {
                Sid: users_sid.as_psid(),
                Attributes: 0,
            });
            DISABLE_MAX_PRIVILEGE
        }
    };

    let mut restricted = HANDLE::default();
    // SAFETY: primary is a live token; restricting entries reference SID
    // buffers that outlive the call; restricted receives a new token handle.
    unsafe {
        CreateRestrictedToken(
            primary.0,
            flags,
            None,
            None,
            Some(&restricting),
            &mut restricted,
        )
    }
    .map_err(|e| format!("CreateRestrictedToken: {e}"))?;
    Ok(OwnedHandle(restricted))
}

fn mark_std_handles_inheritable() {
    for kind in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: GetStdHandle returns a borrowed handle (or invalid, which
        // we skip); setting the inherit flag does not transfer ownership.
        unsafe {
            if let Ok(h) = GetStdHandle(kind) {
                if !h.is_invalid() && h != INVALID_HANDLE_VALUE {
                    let _ = SetHandleInformation(h, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT);
                }
            }
        }
    }
}

/// Build a Windows environment block (UTF-16 `VAR=VAL\0…\0\0`) from the
/// current environment plus `extra` overrides. Used instead of mutating our
/// own environment before spawn — `std::env::set_var` races other threads
/// (the tokio runtime is already up when the runtime re-execs).
fn env_block_with(extra: &[(&str, &str)]) -> Vec<u16> {
    let mut vars: Vec<(std::ffi::OsString, std::ffi::OsString)> = std::env::vars_os().collect();
    for (key, value) in extra {
        let key_os = std::ffi::OsString::from(key);
        vars.retain(|(existing, _)| {
            !existing
                .to_string_lossy()
                .eq_ignore_ascii_case(&key_os.to_string_lossy())
        });
        vars.push((key_os, std::ffi::OsString::from(value)));
    }
    let mut block: Vec<u16> = Vec::new();
    for (key, value) in vars {
        block.extend(key.encode_wide());
        block.push('=' as u16);
        block.extend(value.encode_wide());
        block.push(0);
    }
    block.push(0);
    block
}

/// Spawn `exe` with `cmdline` under `token`, inheriting this process's std
/// handles and console, and bound to a kill-on-close job object so the child
/// cannot outlive the returned handles. `env_block` of `None` inherits our
/// environment verbatim. Returns (process, job).
fn spawn_restricted(
    token: &OwnedHandle,
    exe: &Path,
    cmdline: &std::ffi::OsStr,
    env_block: Option<&[u16]>,
) -> Result<(OwnedHandle, OwnedHandle), String> {
    mark_std_handles_inheritable();

    let job = {
        // SAFETY: creating an anonymous job object; handle owned below.
        let job = unsafe { CreateJobObjectW(None, PCWSTR::null()) }
            .map_err(|e| format!("CreateJobObjectW: {e}"))?;
        let job = OwnedHandle(job);
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: info is a valid extended-limit struct of the stated size.
        unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        }
        .map_err(|e| format!("SetInformationJobObject: {e}"))?;
        job
    };

    let exe_w = wide(exe.as_os_str());
    // lpCommandLine must be mutable (CreateProcessW may rewrite it).
    let mut cmdline_w = wide(cmdline);

    let mut startup = STARTUPINFOW::default();
    startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    startup.dwFlags |= STARTF_USESTDHANDLES;
    // SAFETY: std handles are borrowed for the child to inherit; invalid
    // handles are tolerated by CreateProcess (the child just lacks that
    // stream).
    unsafe {
        startup.hStdInput = GetStdHandle(STD_INPUT_HANDLE).unwrap_or(INVALID_HANDLE_VALUE);
        startup.hStdOutput = GetStdHandle(STD_OUTPUT_HANDLE).unwrap_or(INVALID_HANDLE_VALUE);
        startup.hStdError = GetStdHandle(STD_ERROR_HANDLE).unwrap_or(INVALID_HANDLE_VALUE);
    }

    let mut pi = PROCESS_INFORMATION::default();
    let (creation_flags, env_ptr) = match env_block {
        Some(block) => (
            CREATE_UNICODE_ENVIRONMENT,
            Some(block.as_ptr() as *const std::ffi::c_void),
        ),
        None => (PROCESS_CREATION_FLAGS(0), None),
    };
    // SAFETY: token is a live primary token created from our own token (so
    // no assign-primary privilege is required); exe_w/cmdline_w/env block
    // outlive the call; inheriting handles is intended (std pipes +
    // console).
    unsafe {
        CreateProcessAsUserW(
            Some(token.0),
            PCWSTR(exe_w.as_ptr()),
            Some(PWSTR(cmdline_w.as_mut_ptr())),
            None,
            None,
            true,
            creation_flags,
            env_ptr,
            PCWSTR::null(),
            &startup,
            &mut pi,
        )
    }
    .map_err(|e| format!("CreateProcessAsUserW({}): {e}", exe.display()))?;
    // SAFETY: pi handles are owned by us now; thread handle is unneeded.
    unsafe {
        let _ = CloseHandle(pi.hThread);
    }
    let process = OwnedHandle(pi.hProcess);

    // SAFETY: both handles are live; assignment makes the child die with the
    // job handle (kill-on-close), i.e. with this process.
    unsafe { AssignProcessToJobObject(job.0, process.0) }
        .map_err(|e| format!("AssignProcessToJobObject: {e}"))?;

    Ok((process, job))
}

fn wait_exit_code(process: &OwnedHandle) -> Result<i32, String> {
    // SAFETY: process is a live handle we own.
    let wait = unsafe { WaitForSingleObject(process.0, INFINITE) };
    if wait != WAIT_OBJECT_0 {
        return Err(last_err("WaitForSingleObject"));
    }
    let mut code = 0u32;
    // SAFETY: process is signaled; code is a valid out-pointer.
    unsafe { GetExitCodeProcess(process.0, &mut code) }
        .map_err(|e| format!("GetExitCodeProcess: {e}"))?;
    Ok(code as i32)
}

// ---------------------------------------------------------------------------
// Consumer entry points
// ---------------------------------------------------------------------------

/// Agent-runtime write restriction (the Windows twin of the Linux
/// `apply_sandbox_from_env` Landlock path and the macOS Seatbelt wrap).
///
/// Call FIRST in the runtime's `main`, before stdin is read. When
/// `INTENDANT_SANDBOX_WRITE_PATHS` is set and we are not already the
/// restricted child: re-exec this binary under a `WRITE_RESTRICTED` token
/// with stdin/stdout/stderr inherited, wait, and return the child's exit
/// code for the caller to `exit()` with. (The matching `RESTRICTED` ACEs
/// were stamped daemon-side for the daemon's lifetime.) Returns `Ok(None)`
/// when no restriction applies (child, or sandbox not configured). FAILS
/// CLOSED — an error here must abort the runtime rather than run
/// unsandboxed.
pub fn reexec_write_restricted_if_configured() -> Result<Option<i32>, String> {
    if std::env::var_os(SANDBOX_APPLIED_ENV).is_some() {
        return Ok(None);
    }
    let raw = std::env::var("INTENDANT_SANDBOX_WRITE_PATHS").unwrap_or_default();
    if raw.trim().is_empty() {
        return Ok(None);
    }
    // No ACE work here: the DAEMON stamped the write-path grants for its
    // own lifetime (stamp_daemon_write_grants) — per-spawn stamping would
    // race concurrent runtime spawns sharing the same paths. This parent
    // only creates the token and proxies the child.
    let token = create_restricted_token(TokenRestriction::WriteOnly)?;

    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    // Re-exec with the ORIGINAL command line so the child parses argv
    // identically.
    // SAFETY: GetCommandLineW returns the process's command line; we copy it
    // immediately.
    let cmdline = unsafe { GetCommandLineW() };
    // SAFETY: the returned pointer is a valid NUL-terminated wide string for
    // the life of the process.
    let cmdline = unsafe { cmdline.to_string() }.map_err(|e| format!("command line: {e}"))?;

    // The marker rides an explicit environment block (mutating our own env
    // races tokio worker threads).
    let env = env_block_with(&[(SANDBOX_APPLIED_ENV, "1")]);
    let (process, _job) =
        spawn_restricted(&token, &exe, std::ffi::OsStr::new(&cmdline), Some(&env))?;
    let code = wait_exit_code(&process)?;
    Ok(Some(code))
}

/// Daemon-side, daemon-lifetime grant for the runtime write sandbox: stamp
/// `RESTRICTED`-write ACEs on the configured write paths plus the per-user
/// temp dir (every toolchain assumes a writable temp — parity with `/tmp`
/// in the Linux write set and `TMPDIR` on macOS). Called once from the
/// caller when the sandbox is enabled; the guard lives for the daemon's
/// life, and the journal sweep on the next start covers crashes.
pub fn stamp_daemon_write_grants(write_paths: &[PathBuf]) -> Result<AceGuard, String> {
    let mut paths = write_paths.to_vec();
    paths.push(std::env::temp_dir());
    paths.retain(|p| !p.as_os_str().is_empty());
    AceGuard::stamp(&[], &paths)
}

/// `--scoped-shell-exec` wrapper body: run `shell` fully restricted under
/// the current console (ConPTY), wait, and return the shell's exit code.
/// The daemon already stamped the scope-root grants and scrubbed the
/// environment.
pub fn run_scoped_shell(shell: &str, shell_args: &[String]) -> Result<i32, String> {
    // No ACE work here: the daemon stamped the scope-root grants (held by
    // the PtySession) before spawning this wrapper — per-wrapper stamping
    // would race overlapping scoped shells sharing roots.
    let token = create_restricted_token(TokenRestriction::Full)?;

    // Resolve the shell against PATH so CreateProcess gets a full path.
    let exe = which_shell(shell)?;
    let mut cmdline = format!("\"{}\"", exe.display());
    for arg in shell_args {
        cmdline.push(' ');
        if arg.contains(' ') || arg.contains('"') {
            cmdline.push_str(&format!("\"{}\"", arg.replace('"', "\\\"")));
        } else {
            cmdline.push_str(arg);
        }
    }

    // The wrapper's environment was already scrubbed by the daemon before
    // it spawned us — inherit it verbatim.
    let (process, _job) = spawn_restricted(&token, &exe, std::ffi::OsStr::new(&cmdline), None)?;
    let code = wait_exit_code(&process)?;
    Ok(code)
}

/// Minimal PATH resolution for the shell executable (PATH was scrubbed to
/// system directories by the caller, so this stays predictable).
fn which_shell(shell: &str) -> Result<PathBuf, String> {
    let candidate = Path::new(shell);
    if candidate.is_absolute() {
        return Ok(candidate.to_path_buf());
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        for name in [shell.to_string(), format!("{shell}.exe")] {
            let full = dir.join(&name);
            if full.is_file() {
                return Ok(full);
            }
        }
    }
    Err(format!("shell {shell} not found on PATH"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The grant table, journal file, and temp-dir ACEs are process-global
    /// — these tests must not interleave.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The restricted token must carry exactly one privilege —
    /// `SeChangeNotifyPrivilege`. Anything else surviving from an elevated
    /// parent (SeBackup/SeRestore/SeDebug/…) reopens kernel DACL bypasses
    /// the restricting SIDs never see.
    fn assert_privileges_stripped(restriction: TokenRestriction) {
        use windows::core::w;
        use windows::Win32::Foundation::LUID;
        use windows::Win32::Security::{
            GetTokenInformation, LookupPrivilegeValueW, TokenPrivileges, TOKEN_PRIVILEGES,
        };
        let token = create_restricted_token(restriction).expect("token");
        let mut len = 0u32;
        // SAFETY: sizing call; failure expected (insufficient buffer).
        unsafe {
            let _ = GetTokenInformation(token.0, TokenPrivileges, None, 0, &mut len);
        }
        let mut buf = vec![0u8; len as usize];
        // SAFETY: buf is len bytes, written by the call.
        unsafe {
            GetTokenInformation(
                token.0,
                TokenPrivileges,
                Some(buf.as_mut_ptr() as *mut _),
                len,
                &mut len,
            )
        }
        .expect("query token privileges");
        // SAFETY: buffer holds a TOKEN_PRIVILEGES written by the kernel.
        let privs = unsafe { &*(buf.as_ptr() as *const TOKEN_PRIVILEGES) };
        let mut change_notify = LUID::default();
        // SAFETY: out-pointer valid; the privilege name is a static wide string.
        unsafe {
            LookupPrivilegeValueW(
                PCWSTR::null(),
                w!("SeChangeNotifyPrivilege"),
                &mut change_notify,
            )
        }
        .expect("lookup SeChangeNotifyPrivilege");
        for i in 0..privs.PrivilegeCount as usize {
            // SAFETY: Privileges is a flexible array with PrivilegeCount entries.
            let la = unsafe { *privs.Privileges.as_ptr().add(i) };
            assert!(
                la.Luid.LowPart == change_notify.LowPart
                    && la.Luid.HighPart == change_notify.HighPart,
                "privilege LUID {}:{} survived token restriction",
                la.Luid.HighPart,
                la.Luid.LowPart
            );
        }
    }

    fn run_under(restriction: TokenRestriction, script: &str) -> (i32, String) {
        let token = create_restricted_token(restriction).expect("token");
        let cmd = std::env::var("ComSpec")
            .unwrap_or_else(|_| "C:\\Windows\\System32\\cmd.exe".to_string());
        let out = std::env::temp_dir().join(format!("win-sbx-out-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&out);
        // cmd.exe writes the script's combined output to a file we read
        // back — the restricted child shares our console, so capturing via
        // std handles would interleave with the test harness.
        let cmdline = format!(
            "\"{cmd}\" /d /s /c \"({script}) > \"{}\" 2>&1\"",
            out.display()
        );
        let (process, _job) = spawn_restricted(
            &token,
            Path::new(&cmd),
            std::ffi::OsStr::new(&cmdline),
            None,
        )
        .expect("spawn");
        let code = wait_exit_code(&process).expect("wait");
        let text = std::fs::read_to_string(&out).unwrap_or_default();
        let _ = std::fs::remove_file(&out);
        (code, text)
    }

    #[test]
    fn write_restricted_token_confines_writes_to_granted_paths() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        assert_privileges_stripped(TokenRestriction::WriteOnly);
        let root = std::env::temp_dir().join(format!("win-sbx-wr-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("mkdir");
        // Grant the whole temp dir: the capture file needs it, and the
        // outside probe uses the profile dir.
        let guard = AceGuard::stamp(&[], &[std::env::temp_dir()]).expect("stamp");

        let profile = std::env::var("USERPROFILE").expect("USERPROFILE");
        let script = format!(
            "echo probe > \"{root}\\in.txt\" && echo WRITE_IN_OK & \
             echo probe > \"{profile}\\win-sbx-deny.txt\" 2>nul && echo WRITE_OUT_OK || echo WRITE_OUT_DENIED & \
             type \"%SystemRoot%\\win.ini\" >nul 2>&1 && echo READ_OK || echo READ_DENIED",
            root = root.display(),
        );
        let (_code, text) = run_under(TokenRestriction::WriteOnly, &script);
        drop(guard);
        assert!(text.contains("WRITE_IN_OK"), "{text}");
        assert!(text.contains("WRITE_OUT_DENIED"), "{text}");
        assert!(text.contains("READ_OK"), "{text}");
        assert!(
            !Path::new(&profile).join("win-sbx-deny.txt").exists(),
            "outside write leaked through"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Fully-restricted token: system dirs readable (Users restricting
    /// SID), the user profile denied, a stamped read root readable, and
    /// after the grant drops to refcount zero the root is denied again.
    #[test]
    fn full_restriction_denies_profile_and_honors_scoped_grants() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        assert_privileges_stripped(TokenRestriction::Full);
        // The scope lives INSIDE the denied profile: its stamped grant —
        // not inheritance from the temp-dir grant that keeps the output
        // capture writable — is what opens it, and reaching it exercises
        // traverse-through-denied-parents (SeChangeNotifyPrivilege).
        let profile = std::env::var("USERPROFILE").expect("USERPROFILE");
        let scope = Path::new(&profile).join(format!("win-sbx-full-{}", std::process::id()));
        std::fs::create_dir_all(&scope).expect("mkdir");
        std::fs::write(scope.join("inside.txt"), "inside_ok_9282").expect("write");

        // Overlapping guards: the grant must survive the first drop.
        let g1 = AceGuard::stamp(&[scope.clone()], &[std::env::temp_dir()]).expect("stamp1");
        let g2 = AceGuard::stamp(&[scope.clone()], &[std::env::temp_dir()]).expect("stamp2");
        drop(g1);

        let script = format!(
            "type \"{scope}\\inside.txt\" && echo SCOPE_READ_OK & \
             dir \"{profile}\" >nul 2>&1 && echo PROFILE_OK || echo PROFILE_DENIED & \
             type \"%SystemRoot%\\win.ini\" >nul 2>&1 && echo SYSTEM_READ_OK || echo SYSTEM_READ_DENIED",
            scope = scope.display(),
        );
        let (_code, text) = run_under(TokenRestriction::Full, &script);
        assert!(text.contains("inside_ok_9282"), "{text}");
        assert!(text.contains("SCOPE_READ_OK"), "{text}");
        assert!(text.contains("PROFILE_DENIED"), "{text}");
        assert!(text.contains("SYSTEM_READ_OK"), "{text}");

        // Refcount reached zero: the scope root is denied again. (Keep a
        // temp grant so the capture file stays writable.)
        drop(g2);
        let temp_guard = AceGuard::stamp(&[], &[std::env::temp_dir()]).expect("stamp3");
        let script = format!(
            "type \"{scope}\\inside.txt\" >nul 2>&1 && echo SCOPE_STILL_OPEN || echo SCOPE_CLOSED",
            scope = scope.display(),
        );
        let (_code, text) = run_under(TokenRestriction::Full, &script);
        drop(temp_guard);
        assert!(text.contains("SCOPE_CLOSED"), "{text}");
        let _ = std::fs::remove_dir_all(&scope);
    }

    /// The journal mirrors live grants and the sweep respects live owners:
    /// our own journal (live pid) survives a sweep; a fake dead-pid journal
    /// is replayed and removed.
    #[test]
    fn journal_tracks_grants_and_sweep_skips_live_owners() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let dir = journal_dir();
        let scope = std::env::temp_dir().join(format!("win-sbx-jrnl-{}", std::process::id()));
        std::fs::create_dir_all(&scope).expect("mkdir");

        let guard = AceGuard::stamp(&[scope.clone()], &[]).expect("stamp");
        let own = journal_path();
        assert!(own.exists(), "journal written while grants live");
        sweep_stale_journals(&dir);
        assert!(own.exists(), "sweep must skip the live owner's journal");

        // A dead-pid journal is replayed (harmlessly — the ACE may not
        // exist) and deleted.
        let fake = dir.join("daemon-4294967294.json");
        std::fs::write(
            &fake,
            serde_json::to_string(&vec![StampedAce {
                path: scope.clone(),
                mask: read_mask(),
            }])
            .unwrap(),
        )
        .unwrap();
        sweep_stale_journals(&dir);
        assert!(!fake.exists(), "dead-pid journal should be swept");

        drop(guard);
        assert!(!own.exists(), "journal removed once grants are released");
        assert!(pid_alive(std::process::id()));
        let _ = std::fs::remove_dir_all(&scope);
    }
}
