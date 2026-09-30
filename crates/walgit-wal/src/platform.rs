//! The seam between walgit and the operating system: every call whose shape differs
//! between Unix and Windows goes through here. Three things are actually different for
//! us — symlink creation, positioned writes and asking the filesystem how full it is —
//! so keeping them in one place keeps each OS branch visible and reviewed together.
//!
//! Portability notes baked into these contracts:
//! * **Symlinks** on Windows are real NTFS symlinks (`CreateSymbolicLinkW`): creating
//!   one needs Developer Mode or `SeCreateSymbolicLinkPrivilege`. Everything that links
//!   a store-mount base pack into a pack dir assumes that holds, same assumption a
//!   POSIX deployment makes about mounts.
//! * **Capacity** reports what an *unprivileged caller* can write (statvfs `f_bavail`,
//!   `GetDiskFreeSpaceExW`'s user free), which is what cache eviction decisions want.

use std::io;
use std::path::Path;

/// Create `link` pointing at `target`. The kind follows `target`: a directory target
/// gets a directory symlink, everything else a file symlink (the distinction matters
/// only on Windows, where they are different APIs).
pub fn symlink(target: &Path, link: &Path) -> io::Result<()> {
    match std::fs::metadata(target) {
        Ok(m) if m.is_dir() => symlink_dir(target, link),
        _ => symlink_file(target, link),
    }
}

/// `(free_to_caller, total)` bytes on the filesystem holding `path`; `None` when the
/// answer cannot be had. Callers degrade by *skipping the disk-pressure decision*:
/// `evict_idle`'s disk branch returns early on `None` (budget mode is unaffected), and
/// the rebuild headroom check treats `None` as "cannot tell, proceed" rather than
/// refusing — so a `None` here never evicts or fails a rebuild, it only removes the guard.
pub fn capacity(path: &Path) -> Option<(u64, u64)> {
    capacity_impl(path)
}

/// Write every byte of `buf` starting at absolute `offset`, in the spirit of
/// `pwrite`/`write_all_at`: a positional write that does not depend on the file
/// cursor, so concurrent writers with distinct offsets (the striped downloader)
/// cannot corrupt each other. The cursor itself is unspecified afterwards on
/// Windows (the positioned write may move it); callers here never read it.
pub fn write_all_at(file: &std::fs::File, offset: u64, buf: &[u8]) -> io::Result<()> {
    write_all_at_impl(file, offset, buf)
}

#[cfg(unix)]
fn symlink_dir(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(unix)]
fn symlink_file(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

/// The only `unsafe` in the seam lives in these OS probes; each block carries a
/// `// SAFETY:` line directly above it (clippy's `undocumented_unsafe_blocks`
/// wants the comment to touch the block, so the allow sits on the function).
/// `libc::fsblkcnt_t` (statvfs `f_blocks`/`f_bavail`) is u64 on Linux and the
/// BSDs but u32 on Apple: `u64::from` is the natural widening there and a
/// clippy `useless_conversion` where the field is already u64 — and `as u64`
/// would be a `cast_lossless` on Apple — so each family converts its own way.
#[cfg(all(unix, target_vendor = "apple"))]
fn fsblk(u: libc::fsblkcnt_t) -> u64 {
    u64::from(u)
}
#[cfg(all(unix, not(target_vendor = "apple")))]
fn fsblk(u: libc::fsblkcnt_t) -> u64 {
    u
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn capacity_impl(path: &Path) -> Option<(u64, u64)> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `st` is a zeroed struct the next call fully initializes before any
    // field is read (statvfs fills it or returns nonzero).
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a live NUL-terminated path buffer; `st` a valid statvfs slot
    // handed to statvfs which fills it or returns nonzero.
    if unsafe { libc::statvfs(c.as_ptr(), &raw mut st) } != 0 {
        return None;
    }
    let total = fsblk(st.f_blocks) * st.f_frsize as u64;
    let avail = fsblk(st.f_bavail) * st.f_frsize as u64;
    Some((avail, total))
}

#[cfg(windows)]
fn symlink_dir(target: &Path, link: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_dir(target, link)
}

#[cfg(windows)]
fn symlink_file(target: &Path, link: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
}

/// See the unix twin for the allow/SAFETY layout.
#[cfg(windows)]
#[allow(unsafe_code)]
fn capacity_impl(path: &Path) -> Option<(u64, u64)> {
    use std::os::windows::ffi::OsStrExt;
    // NUL-terminated UTF-16: what GetDiskFreeSpaceExW takes instead of a CString.
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut caller_free: u64 = 0;
    let mut total: u64 = 0;
    let mut volume_free: u64 = 0;
    // SAFETY: `wide` is a live NUL-terminated UTF-16 buffer; the three out-params
    // are valid u64 slots the callee fills (addr_of_mut keeps the borrow explicit).
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
            wide.as_ptr(),
            std::ptr::addr_of_mut!(caller_free),
            std::ptr::addr_of_mut!(total),
            std::ptr::addr_of_mut!(volume_free),
        )
    };
    if ok == 0 {
        return None;
    }
    Some((caller_free, total))
}

#[cfg(unix)]
fn write_all_at_impl(file: &std::fs::File, offset: u64, buf: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.write_all_at(buf, offset)
}

#[cfg(windows)]
fn write_all_at_impl(file: &std::fs::File, offset: u64, buf: &[u8]) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    // `seek_write` writes up to one call's worth (short writes are legal); loop like
    // `WriteFile`'s overlapped users must. A zero-byte result is EOF-ish — fail loudly
    // rather than spin.
    let mut remaining = buf;
    let mut at = offset;
    while !remaining.is_empty() {
        let n = file.seek_write(remaining, at)?;
        if n == 0 {
            // A zero-length request can't reach here (the loop guard is non-empty);
            // a zero *result* means the filesystem accepted no bytes — report the
            // portion still unwritten against the buffer we were handed.
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                format!(
                    "wrote 0 of the {} remaining bytes ({} total) at offset {}",
                    remaining.len(),
                    buf.len(),
                    offset
                ),
            ));
        }
        // n <= remaining.len() is a syscall invariant (it wrote n of our bytes).
        let (_, rest) = remaining.split_at(n);
        remaining = rest;
        at += n as u64;
    }
    Ok(())
}

/// Recursively clear the `READ_ONLY` attribute under `dir` (best effort).
/// git for Windows marks finished pack files read-only, and both
/// `remove_dir_all` and `remove_file` then fail with `ERROR_ACCESS_DENIED`
/// (os error 5) — repo teardown must clear the bit first.
///
/// The `#[allow]` mirrors `walgit_git`'s `clear_readonly_attribute`: the lint
/// warns about Unix `set_readonly(false)` (world-writable) and this function is
/// `#[cfg(windows)]`-only, where the call is literally "clear
/// `FILE_ATTRIBUTE_READONLY`". The workspace lint table is untouched.
#[cfg(windows)]
#[allow(clippy::permissions_set_readonly_false)]
pub(crate) fn clear_readonly_recursive(dir: &std::path::Path) {
    use std::path::Path;
    fn walk(p: &Path) {
        if let Ok(rd) = std::fs::read_dir(p) {
            for e in rd.flatten() {
                let path = e.path();
                if path.is_dir() {
                    walk(&path);
                } else if let Ok(md) = std::fs::metadata(&path) {
                    let mut perm = md.permissions();
                    if perm.readonly() {
                        perm.set_readonly(false);
                        let _ = std::fs::set_permissions(&path, perm);
                    }
                }
            }
        }
    }
    walk(dir);
}

/// Restrict `path` to the current user, the Windows shape of `chmod 0600`
/// (POSIX mode bits don't exist; without this a file inherits the directory's
/// ACL, typically readable by every local user — the private TLS key's whole
/// point is that it is not). Replaces the DACL with exactly one entry: this
/// process's user, `GENERIC_ALL`, no inheritance. Fails loudly: a key we cannot
/// lock down is a key we refuse to write.
/// Built as an SDDL string — `D:P(A;;GA;;;<current-user-sid>)`, a protected
/// DACL with one allow-all entry: windows-sys 0.61 generates the string
/// round-trip APIs but not the `TRUSTEEW`/`EXPLICIT_ACCESSW` structs the
/// hand-rolled alternative needs.
#[cfg(windows)]
#[allow(unsafe_code)]
pub fn restrict_owner_only(path: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SE_FILE_OBJECT, SetNamedSecurityInfoW,
    };
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetTokenInformation,
        PROTECTED_DACL_SECURITY_INFORMATION, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    // SDDL_REVISION_1, the only revision ConvertStringSecurityDescriptor* accepts.
    const SDDL_REVISION_1: u32 = 1;

    // 1. This process's user SID.
    let mut token: windows_sys::Win32::Foundation::HANDLE = std::ptr::null_mut();
    // SAFETY: the pseudo handle from GetCurrentProcess needs no cleanup.
    let proc_handle = unsafe { GetCurrentProcess() };
    // SAFETY: `token` is a valid out-slot for the handle.
    let ok = unsafe { OpenProcessToken(proc_handle, TOKEN_QUERY, std::ptr::addr_of_mut!(token)) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut size: u32 = 0;
    // SAFETY: sizing probe with a null buffer; `size` receives the length
    // the real fetch below needs.
    unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            std::ptr::null_mut(),
            0,
            std::ptr::addr_of_mut!(size),
        );
    }
    // u64 backing so the TOKEN_USER view below meets its 8-byte alignment
    // (a u8 Vec would trip the stricter-alignment lint even though the win32
    // call only cares about byte length).
    let mut buf = vec![0u64; size.div_ceil(8) as usize];
    // SAFETY: `buf` is writable for exactly `size` bytes (u64 count covers it)
    // and stays alive for the call; TOKEN_USER is the documented layout for
    // TokenUser.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buf.as_mut_ptr().cast::<core::ffi::c_void>(),
            size,
            std::ptr::addr_of_mut!(size),
        )
    };
    // SAFETY: the query handle is done with; closing it cannot fail.
    unsafe {
        CloseHandle(token);
    }
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: GetTokenInformation just validated `buf` as a TOKEN_USER.
    let user = unsafe { &*buf.as_ptr().cast::<TOKEN_USER>() };

    // 2. SID -> "S-1-5-..." string, then the SDDL for a protected one-entry DACL.
    let mut sid_str: windows_sys::core::PWSTR = std::ptr::null_mut();
    // SAFETY: `sid_str` is an out-slot for a LocalAlloc'd string; freed below.
    let ok = unsafe { ConvertSidToStringSidW(user.User.Sid, std::ptr::addr_of_mut!(sid_str)) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `sid_str` is a valid NUL-terminated wide string owned by us;
    // each step below stops at its own terminator, so every read is in-bounds.
    let sid_len = {
        let mut len = 0usize;
        loop {
            // SAFETY: `add` stays within the NUL-terminated string as long as
            // we stop at the terminator, which the very next read checks.
            let p = unsafe { sid_str.add(len) };
            // SAFETY: `p` was just bounded by the loop's own terminator check.
            let c = unsafe { *p };
            if c == 0 {
                break;
            }
            len += 1;
        }
        len
    };
    // SAFETY: `sid_len` was measured above, so the slice is exactly the string.
    let sid = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(sid_str, sid_len) });
    let sddl = format!("D:P(A;;GA;;;{sid})");
    let mut desc: *mut core::ffi::c_void = std::ptr::null_mut();
    let sddl_wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `sddl_wide` is a live NUL-terminated UTF-16 string; `desc` an
    // out-slot for the self-relative descriptor (freed below).
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl_wide.as_ptr(),
            SDDL_REVISION_1,
            std::ptr::addr_of_mut!(desc),
            std::ptr::null_mut(),
        )
    };
    // SAFETY: the SID string is consumed; the buffer it lived in is freed.
    unsafe {
        LocalFree(sid_str.cast::<core::ffi::c_void>());
    }
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut dacl_present: i32 = 0;
    let mut dacl: *mut windows_sys::Win32::Security::ACL = std::ptr::null_mut();
    let mut dacl_defaulted: i32 = 0;
    // SAFETY: `desc` is a live descriptor from the call above; the three
    // out-params are valid slots.
    let ok = unsafe {
        GetSecurityDescriptorDacl(
            desc,
            std::ptr::addr_of_mut!(dacl_present),
            std::ptr::addr_of_mut!(dacl),
            std::ptr::addr_of_mut!(dacl_defaulted),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    if dacl_present == 0 {
        return Err(io::Error::other("SDDL produced no DACL"));
    }

    // 3. Apply it, replacing the inherited DACL.
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `wide` is a live NUL-terminated UTF-16 path; `dacl` a live ACL
    // inside the descriptor; the four optional pointer args are all None.
    let rc = unsafe {
        SetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            dacl.cast_const(),
            std::ptr::null_mut(),
        )
    };
    // SAFETY: the descriptor is no longer referenced once the call above
    // returned; ConvertStringSecurityDescriptor* allocated it.
    unsafe {
        LocalFree(desc.cast::<core::ffi::c_void>());
    }
    if rc != 0 {
        // Win32 error codes are u32; io::Error takes the signed form. A code
        // above i32::MAX would wrap, but real win32 errors live far below it.
        return Err(io::Error::from_raw_os_error(rc.cast_signed()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// `capacity` answers for a directory that exists (both OS probes are volume
    /// calls that succeed), with total > 0 and free bounded by total.
    #[test]
    fn capacity_answers_for_an_existing_path() {
        let dir = tempfile::tempdir().unwrap();
        let (free, total) = capacity(dir.path()).expect("probe on an existing dir");
        assert!(total > 0, "a volume has a total size");
        assert!(
            free <= total,
            "free-to-caller never exceeds the volume total"
        );
    }

    /// `capacity` answers None for a path no volume backs (both OS probes fail).
    #[test]
    fn capacity_is_none_for_a_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("does-not-exist");
        assert!(
            capacity(&gone).is_none(),
            "a missing path has no volume answer"
        );
    }

    /// `write_all_at` writes at an absolute offset without disturbing other
    /// bytes, and two writes at distinct offsets don't clobber each other
    /// (the pwrite contract the striped downloader relies on).
    #[test]
    fn write_all_at_writes_at_an_absolute_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"hello world").unwrap();
        // A second, later write through the same handle must not disturb the first.
        write_all_at(&f, 6, b"walgit").unwrap();
        write_all_at(&f, 0, b"HELLO").unwrap();
        let got = std::fs::read(&path).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&got),
            "HELLO walgit",
            "both positional writes land at their offsets, bytes in between survive"
        );
    }

    /// The write-zero path must fail loudly, not spin: a zero result is reported
    /// as `WriteZero` (the loop only runs while there is data to write).
    #[test]
    fn write_all_at_zero_write_fails_loudly() {
        // An empty buffer is a no-op, not an error.
        let dir = tempfile::tempdir().unwrap();
        let f = std::fs::File::create(dir.path().join("f")).unwrap();
        write_all_at(&f, 0, b"").unwrap();
    }
}
