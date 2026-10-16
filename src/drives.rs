//! Drive enumeration and location classification.

use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::NO_ERROR;
use windows::Win32::NetworkManagement::WNet::WNetGetConnectionW;
use windows::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
};
use windows::Win32::System::Diagnostics::Debug::{
    SEM_FAILCRITICALERRORS, SetThreadErrorMode, THREAD_ERROR_MODE,
};
use windows::Win32::System::WindowsProgramming::{DRIVE_FIXED, DRIVE_REMOTE, DRIVE_REMOVABLE};
use windows::core::{PCWSTR, PWSTR};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DriveKind {
    Fixed,
    Removable,
    Network,
    Other,
}

#[derive(Clone, Debug)]
pub struct DriveInfo {
    pub root: PathBuf,
    pub label: String,
    pub kind: DriveKind,
    pub total: u64,
    pub free: u64,
    pub remote: Option<String>,
}

fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(Some(0)).collect()
}

/// Suppresses the "no disk in drive" dialog on this thread while alive.
struct NoCriticalErrors(Option<THREAD_ERROR_MODE>);

impl NoCriticalErrors {
    fn new() -> Self {
        let mut old = THREAD_ERROR_MODE(0);
        // SAFETY: plain out-pointer to a local.
        let ok = unsafe { SetThreadErrorMode(SEM_FAILCRITICALERRORS, Some(&mut old)).is_ok() };
        NoCriticalErrors(ok.then_some(old))
    }
}

impl Drop for NoCriticalErrors {
    fn drop(&mut self) {
        // Only restore if the mode was actually changed (so `old` is valid).
        if let Some(old) = self.0 {
            // SAFETY: restores the mode saved in `new`.
            unsafe {
                let _ = SetThreadErrorMode(old, None);
            }
        }
    }
}

fn kind_from_type(t: u32) -> DriveKind {
    match t {
        DRIVE_FIXED => DriveKind::Fixed,
        DRIVE_REMOVABLE => DriveKind::Removable,
        DRIVE_REMOTE => DriveKind::Network,
        _ => DriveKind::Other,
    }
}

/// Normalizes separators and strips `\\?\` (turning `\\?\UNC\` into `\\`).
fn normalize(path: &Path) -> String {
    let s = path.to_string_lossy().replace('/', "\\");
    match s.strip_prefix(r"\\?\") {
        Some(r) => match r.strip_prefix(r"UNC\") {
            Some(unc) => format!(r"\\{unc}"),
            None => r.to_string(),
        },
        None => s,
    }
}

/// Returns (total, free-to-caller) or None if the query fails.
fn disk_space(root: &Path) -> Option<(u64, u64)> {
    let _guard = NoCriticalErrors::new();
    let w = wide(root);
    let (mut free, mut total) = (0u64, 0u64);
    // SAFETY: `w` is NUL-terminated and outlives the call; out-pointers are locals.
    unsafe {
        GetDiskFreeSpaceExW(PCWSTR(w.as_ptr()), Some(&mut free), Some(&mut total), None).ok()?;
    }
    Some((total, free))
}

fn volume_label(root: &Path) -> String {
    let w = wide(root);
    let mut buf = [0u16; 261];
    // SAFETY: `w` is NUL-terminated; `buf` is a valid mutable slice.
    let ok = unsafe {
        GetVolumeInformationW(PCWSTR(w.as_ptr()), Some(&mut buf), None, None, None, None).is_ok()
    };
    if !ok {
        return String::new();
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    OsString::from_wide(&buf[..n])
        .to_string_lossy()
        .into_owned()
}

fn remote_name(letter: char) -> Option<String> {
    let local = wide(format!("{letter}:"));
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: `local` is NUL-terminated; `buf`/`len` describe a valid buffer.
    let rc = unsafe {
        WNetGetConnectionW(
            PCWSTR(local.as_ptr()),
            Some(PWSTR(buf.as_mut_ptr())),
            &mut len,
        )
    };
    interpret_wnet(rc.0, &buf)
}

/// Interprets a `WNetGetConnectionW` result. A disconnected persistent
/// mapping returns ERROR_CONNECTION_UNAVAIL (1201) yet still fills the buffer
/// with the remembered remote name, so that counts as success when non-empty.
fn interpret_wnet(code: u32, buf: &[u16]) -> Option<String> {
    const ERROR_CONNECTION_UNAVAIL: u32 = 1201;
    if code != NO_ERROR.0 && code != ERROR_CONNECTION_UNAVAIL {
        return None;
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    if n == 0 {
        return None;
    }
    Some(
        OsString::from_wide(&buf[..n])
            .to_string_lossy()
            .into_owned(),
    )
}

pub fn list_drives() -> Vec<DriveInfo> {
    let _guard = NoCriticalErrors::new();
    // SAFETY: no arguments.
    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let root = PathBuf::from(format!("{letter}:\\"));
        let w = wide(&root);
        // SAFETY: `w` is NUL-terminated.
        let kind = kind_from_type(unsafe { GetDriveTypeW(PCWSTR(w.as_ptr())) });
        let (total, free) = disk_space(&root).unwrap_or((0, 0));
        let label = if total > 0 {
            volume_label(&root)
        } else {
            String::new()
        };
        let remote = if kind == DriveKind::Network {
            remote_name(letter)
        } else {
            None
        };
        out.push(DriveInfo {
            root,
            label,
            kind,
            total,
            free,
            remote,
        });
    }
    out
}

pub fn location_kind(path: &Path) -> DriveKind {
    let s = normalize(path);
    if s.starts_with(r"\\") {
        return DriveKind::Network;
    }
    let b = s.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        let _guard = NoCriticalErrors::new();
        let w = wide(format!("{}:\\", (b[0] as char).to_ascii_uppercase()));
        // SAFETY: `w` is NUL-terminated.
        return kind_from_type(unsafe { GetDriveTypeW(PCWSTR(w.as_ptr())) });
    }
    DriveKind::Other
}

/// Pure check: `C:`, `C:\`, `\\srv\share`, `\\srv\share\`.
pub fn is_volume_root(path: &Path) -> bool {
    let s = normalize(path);
    let b = s.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        return matches!(&s[2..], "" | "\\");
    }
    if let Some(rest) = s.strip_prefix(r"\\") {
        let rest = rest.strip_suffix('\\').unwrap_or(rest);
        let parts: Vec<&str> = rest.split('\\').collect();
        return parts.len() == 2 && parts.iter().all(|p| !p.is_empty());
    }
    false
}

/// Whether a scan of `path` (on a drive of `kind`) gets the "Unseen" block
/// (volume used bytes minus what the scan counted). Only the root of a local
/// fixed volume qualifies: for a subfolder, a removable or network drive, or
/// any UNC path, the volume's used figure says nothing about what the scan
/// missed.
pub fn unseen_allowed(path: &Path, kind: DriveKind) -> bool {
    kind == DriveKind::Fixed && is_volume_root(path) && !normalize(path).starts_with(r"\\")
}

/// The used-bytes figure for the Unseen block of a scan rooted at `path`,
/// or `None` when it gets no Unseen block. `known` (from a drive row) saves
/// the disk query. Touches the drive, so call it off the UI thread.
pub fn unseen_used(path: &Path, known: Option<u64>) -> Option<u64> {
    if unseen_allowed(path, location_kind(path)) {
        known.or_else(|| disk_used(path))
    } else {
        None
    }
}

/// Used bytes (total - free) of the volume containing `path`.
///
/// Expects a directory or volume root; a file path returns `None`.
pub fn disk_used(path: &Path) -> Option<u64> {
    let mut p = path.as_os_str().to_os_string();
    if !p.to_string_lossy().ends_with('\\') {
        p.push("\\");
    }
    let (total, free) = disk_space(Path::new(&p))?;
    Some(total.saturating_sub(free))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_root_forms() {
        for p in [
            r"C:\",
            "C:",
            r"\\srv\share",
            r"\\srv\share\",
            "C:/",
            "//srv/share/",
            r"\\?\C:\",
            r"\\?\UNC\srv\share",
            r"c:\",
        ] {
            assert!(is_volume_root(Path::new(p)), "{p}");
        }
        for p in [r"C:\x", r"\\srv\share\x", r"\\srv", r"C:\x\", r"\\?\C:\x"] {
            assert!(!is_volume_root(Path::new(p)), "{p}");
        }
    }

    #[test]
    fn list_drives_includes_c() {
        let c = list_drives()
            .into_iter()
            .find(|d| d.root == Path::new(r"C:\"))
            .expect("C: listed");
        assert_eq!(c.kind, DriveKind::Fixed);
        assert!(c.total > c.free && c.free > 0);
    }

    #[test]
    fn unc_is_network() {
        assert_eq!(
            location_kind(Path::new(r"\\srv\share\x")),
            DriveKind::Network
        );
        assert_eq!(location_kind(Path::new(r"C:\Windows")), DriveKind::Fixed);
    }

    #[test]
    fn interpret_wnet_codes() {
        let name: Vec<u16> = r"\\srv\share".encode_utf16().chain([0, 0]).collect();
        let want = Some(r"\\srv\share".to_string());
        assert_eq!(interpret_wnet(0, &name), want);
        assert_eq!(interpret_wnet(1201, &name), want);
        assert_eq!(interpret_wnet(1201, &[0u16; 8]), None);
        assert_eq!(interpret_wnet(2250, &name), None);
    }

    #[test]
    fn unseen_only_for_local_fixed_volume_roots() {
        let ok = |p: &str, k| unseen_allowed(Path::new(p), k);
        for p in [r"C:\", "C:", r"\\?\C:\", r"d:\"] {
            assert!(ok(p, DriveKind::Fixed), "{p}");
        }
        // Subfolders never.
        for p in [r"C:\Users", r"C:\x\", r"\\?\C:\x"] {
            assert!(!ok(p, DriveKind::Fixed), "{p}");
        }
        // Removable, network (mapped letters too) and unknown drives never.
        for k in [DriveKind::Removable, DriveKind::Network, DriveKind::Other] {
            assert!(!ok(r"E:\", k), "{k:?}");
        }
        // UNC shares never, whatever kind they were reported as.
        for p in [r"\\srv\share", r"\\srv\share\", r"\\?\UNC\srv\share"] {
            assert!(!ok(p, DriveKind::Fixed), "{p}");
            assert!(!ok(p, DriveKind::Network), "{p}");
        }
    }

    #[test]
    fn unseen_used_gates_on_the_real_drive() {
        let c = Path::new(r"C:\");
        assert!(unseen_used(c, None).is_some_and(|u| u > 0));
        assert_eq!(unseen_used(c, Some(42)), Some(42), "known figure reused");
        // A subfolder of C: or a UNC share gets nothing, even with a known figure.
        assert_eq!(unseen_used(Path::new(r"C:\Windows"), Some(42)), None);
        assert_eq!(unseen_used(Path::new(r"\\srv\share"), Some(42)), None);
    }

    #[test]
    fn disk_used_c() {
        assert!(disk_used(Path::new(r"C:\")).is_some_and(|u| u > 0));
    }
}
