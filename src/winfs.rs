//! Native Windows directory enumeration: one handle per directory, batched
//! `FILE_ID_BOTH_DIR_INFO` records (allocation size, file id, attributes),
//! verbatim (`\\?\`) paths so long paths and odd names work, SMB friendly.

use std::ffi::{OsStr, OsString, c_void};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf, Prefix};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_FILES, FILETIME, HANDLE, SYSTEMTIME,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_BOTH_DIR_INFO,
    FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FileAttributeTagInfo, FileIdBothDirectoryInfo,
    FileIdBothDirectoryRestartInfo, GetFileInformationByHandleEx, OPEN_EXISTING,
};
use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
use windows::core::PCWSTR;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawEntry {
    /// Lossy UTF-8 conversion of the UTF-16 name (unpaired surrogates become U+FFFD).
    pub name: String,
    pub is_dir: bool,
    /// A name-surrogate reparse point (symlink, junction, mount point): the
    /// scanner must not descend it. Other reparse points (cloud placeholders,
    /// dedup, WOF) are reported as ordinary files and directories.
    pub is_reparse: bool,
    pub alloc_size: u64,
    pub file_id: u128,
    pub modified: SystemTime,
}

/// `C:\x` -> `\\?\C:\x`, `\\srv\sh\x` -> `\\?\UNC\srv\sh\x`; verbatim and
/// device (`\\?\`, `\\.\`) paths are returned unchanged.
///
/// Fully qualified paths are converted textually: `/` becomes `\`, empty
/// and `.` segments are dropped and `..` pops a segment, but names are
/// otherwise kept exactly, so a trailing dot or space (`foo.`, `bar `)
/// still names that file rather than its `foo` / `bar` sibling. Only
/// relative and drive-relative paths go through `std::path::absolute`
/// (which strips trailing dots and spaces); scan roots are made fully
/// qualified once by [`normalize_root`], so paths built from scanned names
/// never take that route.
pub fn to_verbatim(path: &Path) -> PathBuf {
    if let Some((head, segs)) = qualified_parts(path) {
        let is_disk = matches!(head, Head::Disk(_));
        let mut out = match head {
            Head::Disk(d) => OsString::from(format!(r"\\?\{}:", d as char)),
            Head::Unc(srv, share) => {
                let mut o = OsString::from(r"\\?\UNC\");
                o.push(srv);
                o.push(r"\");
                o.push(share);
                o
            }
            Head::Verbatim => return path.to_path_buf(),
        };
        if segs.is_empty() && is_disk {
            out.push(r"\");
        }
        push_segments(&mut out, &segs);
        return PathBuf::from(out);
    }
    match std::path::absolute(path) {
        Ok(abs) if qualified_parts(&abs).is_some() => to_verbatim(&abs),
        _ => path.to_path_buf(),
    }
}

/// The display (non-verbatim) form of a scan root, made fully qualified and
/// lexically cleaned once, before any child path is built from it. Verbatim
/// and device paths are returned unchanged; trailing dots and spaces in
/// fully qualified paths are kept.
pub fn normalize_root(path: &Path) -> PathBuf {
    if let Some((head, segs)) = qualified_parts(path) {
        let is_disk = matches!(head, Head::Disk(_));
        let mut out = match head {
            Head::Disk(d) => OsString::from(format!("{}:", d as char)),
            Head::Unc(srv, share) => {
                let mut o = OsString::from(r"\\");
                o.push(srv);
                o.push(r"\");
                o.push(share);
                o
            }
            Head::Verbatim => return path.to_path_buf(),
        };
        if segs.is_empty() && is_disk {
            out.push(r"\");
        }
        push_segments(&mut out, &segs);
        return PathBuf::from(out);
    }
    match std::path::absolute(path) {
        Ok(abs) if qualified_parts(&abs).is_some() => normalize_root(&abs),
        _ => path.to_path_buf(),
    }
}

enum Head<'a> {
    Disk(u8),
    Unc(&'a OsStr, &'a OsStr),
    Verbatim,
}

/// Splits a fully qualified path into its root and its cleaned name segments
/// (no `.`, `..` resolved, never above the root). None for relative, rooted
/// (`\x`) and drive-relative (`C:x`) paths.
fn qualified_parts(path: &Path) -> Option<(Head<'_>, Vec<&OsStr>)> {
    let mut comps = path.components();
    let Some(Component::Prefix(p)) = comps.next() else {
        return None;
    };
    let head = match p.kind() {
        Prefix::Verbatim(_)
        | Prefix::VerbatimUNC(..)
        | Prefix::VerbatimDisk(_)
        | Prefix::DeviceNS(_) => return Some((Head::Verbatim, Vec::new())),
        Prefix::Disk(d) => Head::Disk(d.to_ascii_uppercase()),
        Prefix::UNC(srv, share) => Head::Unc(srv, share),
    };
    if comps.next() != Some(Component::RootDir) {
        // `C:x` is drive-relative; a bare `\\srv\share` has no RootDir but
        // is fully qualified.
        return match head {
            Head::Unc(..) if path.components().count() == 1 => Some((head, Vec::new())),
            _ => None,
        };
    }
    let mut segs = Vec::new();
    for c in comps {
        match c {
            Component::Normal(n) => segs.push(n),
            Component::ParentDir => {
                segs.pop();
            }
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
        }
    }
    Some((head, segs))
}

fn push_segments(out: &mut OsString, segs: &[&OsStr]) {
    for s in segs {
        out.push(r"\");
        out.push(s);
    }
}

/// True if any component of `path` ends in a dot or a space. Win32 path
/// normalisation trims those, so such a path only names the right item in
/// verbatim (`\\?\`) form; the shell (Explorer, the Recycle Bin) can't be
/// trusted with it at all.
pub fn has_trimmable_name(path: &Path) -> bool {
    path.components().any(|c| match c {
        Component::Normal(n) => {
            let n = n.to_string_lossy();
            n.ends_with('.') || n.ends_with(' ')
        }
        _ => false,
    })
}

/// True for name-surrogate reparse points (`IsReparseTagNameSurrogate`).
/// When the reparse attribute is set, `FILE_ID_BOTH_DIR_INFO.EaSize` holds
/// the reparse tag.
pub fn is_link_reparse(attributes: u32, ea_size_or_tag: u32) -> bool {
    const NAME_SURROGATE_BIT: u32 = 0x2000_0000;
    attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 && ea_size_or_tag & NAME_SURROGATE_BIT != 0
}

struct HandleGuard(HANDLE);
impl Drop for HandleGuard {
    fn drop(&mut self) {
        // SAFETY: the handle was returned by CreateFileW and is closed once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn win_err(e: windows::core::Error) -> io::Error {
    let hr = e.code().0 as u32;
    // HRESULT_FROM_WIN32 form: 0x8007xxxx
    if hr >> 16 == 0x8007 {
        io::Error::from_raw_os_error((hr & 0xFFFF) as i32)
    } else {
        io::Error::other(e)
    }
}

/// Local-time "YYYY-MM-DD HH:MM" for a modification time, DST-aware like
/// Explorer (None if unconvertible).
pub fn local_datetime_string(t: SystemTime) -> Option<String> {
    const EPOCH_DIFF_TICKS: u64 = 116_444_736_000_000_000;
    let since = t.duration_since(UNIX_EPOCH).ok()?;
    let ticks = EPOCH_DIFF_TICKS + (since.as_nanos() / 100) as u64;
    let utc = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let mut sys = SYSTEMTIME::default();
    let mut local = SYSTEMTIME::default();
    // SAFETY: all pointers refer to valid, live values.
    unsafe {
        FileTimeToSystemTime(&utc, &mut sys).ok()?;
        SystemTimeToTzSpecificLocalTime(None, &sys, &mut local).ok()?;
    }
    Some(crate::format::format_datetime(
        local.wYear,
        local.wMonth,
        local.wDay,
        local.wHour,
        local.wMinute,
    ))
}

fn filetime_to_system_time(t: i64) -> SystemTime {
    // 100 ns ticks since 1601-01-01; Unix epoch is 11_644_473_600 s later.
    const EPOCH_DIFF_TICKS: i64 = 116_444_736_000_000_000;
    let d = t - EPOCH_DIFF_TICKS;
    if d >= 0 {
        UNIX_EPOCH + Duration::from_nanos(d as u64 * 100)
    } else {
        UNIX_EPOCH - Duration::from_nanos(d.unsigned_abs() * 100)
    }
}

pub fn read_dir_entries(dir: &Path) -> io::Result<Vec<RawEntry>> {
    let wide: Vec<u16> = to_verbatim(dir)
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `wide` is NUL-terminated and outlives the call.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            FILE_LIST_DIRECTORY.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )
    }
    .map_err(win_err)?;
    let guard = HandleGuard(handle);

    // u64 elements guarantee the 8-byte alignment the records need.
    let mut buf = vec![0u64; 64 * 1024 / 8];
    let mut out = Vec::new();
    let mut class = FileIdBothDirectoryRestartInfo;
    let mut first = true;
    loop {
        // SAFETY: buf is valid for the stated byte length.
        let r = unsafe {
            GetFileInformationByHandleEx(
                guard.0,
                class,
                buf.as_mut_ptr() as *mut c_void,
                (buf.len() * 8) as u32,
            )
        };
        class = FileIdBothDirectoryInfo;
        if let Err(e) = r {
            if e.code() == ERROR_NO_MORE_FILES.to_hresult() {
                break;
            }
            // FAT/exFAT roots have no `.`/`..`, so an empty one reports
            // FILE_NOT_FOUND on the first call.
            if first && e.code() == ERROR_FILE_NOT_FOUND.to_hresult() {
                break;
            }
            return Err(win_err(e));
        }
        first = false;
        walk_records(bytes_of(&buf), &mut out)?;
    }
    Ok(out)
}

/// What a path itself is, without following it if it is a reparse point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathAttrs {
    pub is_dir: bool,
    /// Name-surrogate reparse point (junction, symlink, mount point); see
    /// [`is_link_reparse`]. Cloud placeholders, dedup and WOF are not links.
    pub is_link: bool,
}

pub fn path_attrs(path: &Path) -> io::Result<PathAttrs> {
    let wide: Vec<u16> = to_verbatim(path)
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `wide` is NUL-terminated and outlives the call.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }
    .map_err(win_err)?;
    let guard = HandleGuard(handle);
    let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
    // SAFETY: `info` is a valid, correctly sized FILE_ATTRIBUTE_TAG_INFO.
    unsafe {
        GetFileInformationByHandleEx(
            guard.0,
            FileAttributeTagInfo,
            &mut info as *mut _ as *mut c_void,
            std::mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    }
    .map_err(win_err)?;
    Ok(PathAttrs {
        is_dir: info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0,
        is_link: is_link_reparse(info.FileAttributes, info.ReparseTag),
    })
}

fn bytes_of(buf: &[u64]) -> &[u8] {
    // SAFETY: reinterpreting initialized u64s as bytes of the same total size.
    unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, std::mem::size_of_val(buf)) }
}

fn invalid(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// Walk the chain of FILE_ID_BOTH_DIR_INFO records in `bytes`, validating every
/// offset and length against the buffer before reading.
fn walk_records(bytes: &[u8], out: &mut Vec<RawEntry>) -> io::Result<()> {
    let len = bytes.len();
    let name_off = std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
    let mut off = 0usize;
    loop {
        if off.saturating_add(std::mem::size_of::<FILE_ID_BOTH_DIR_INFO>()) > len {
            return Err(invalid("directory record header out of bounds"));
        }
        // SAFETY: header bounds checked above; read_unaligned has no alignment need.
        let rec: FILE_ID_BOTH_DIR_INFO = unsafe {
            std::ptr::read_unaligned(bytes.as_ptr().add(off) as *const FILE_ID_BOTH_DIR_INFO)
        };
        let name_len = rec.FileNameLength as usize;
        if !name_len.is_multiple_of(2)
            || off.saturating_add(name_off).saturating_add(name_len) > len
        {
            return Err(invalid("directory record name out of bounds"));
        }
        let name_units: Vec<u16> = bytes[off + name_off..off + name_off + name_len]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        if !matches!(name_units.as_slice(), [0x2E] | [0x2E, 0x2E]) {
            out.push(RawEntry {
                name: OsString::from_wide(&name_units)
                    .to_string_lossy()
                    .into_owned(),
                is_dir: rec.FileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0,
                is_reparse: is_link_reparse(rec.FileAttributes, rec.EaSize),
                alloc_size: rec.AllocationSize.max(0) as u64,
                file_id: rec.FileId as u64 as u128,
                modified: filetime_to_system_time(rec.LastWriteTime),
            });
        }
        let next = rec.NextEntryOffset as usize;
        if next == 0 {
            return Ok(());
        }
        if !next.is_multiple_of(8) || off.saturating_add(next) >= len {
            return Err(invalid("bad NextEntryOffset in directory record"));
        }
        off += next;
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_verbatim_forms() {
        assert_eq!(to_verbatim(Path::new(r"C:\x")), PathBuf::from(r"\\?\C:\x"));
        assert_eq!(
            to_verbatim(Path::new(r"\\srv\sh\x")),
            PathBuf::from(r"\\?\UNC\srv\sh\x")
        );
        assert_eq!(
            to_verbatim(Path::new(r"\\?\C:\x")),
            PathBuf::from(r"\\?\C:\x")
        );
        assert_eq!(
            to_verbatim(Path::new(r"\\?\UNC\srv\sh")),
            PathBuf::from(r"\\?\UNC\srv\sh")
        );
        assert_eq!(to_verbatim(Path::new(r"c:\")), PathBuf::from(r"\\?\C:\"));
        assert_eq!(
            to_verbatim(Path::new(r"\\srv\sh")),
            PathBuf::from(r"\\?\UNC\srv\sh")
        );
        assert_eq!(
            to_verbatim(Path::new(r"C:/a//b/./c/../d\")),
            PathBuf::from(r"\\?\C:\a\b\d")
        );
        assert_eq!(
            to_verbatim(Path::new(r"C:\..\..\x")),
            PathBuf::from(r"\\?\C:\x")
        );
    }

    #[test]
    fn to_verbatim_keeps_trailing_dots_and_spaces() {
        assert_eq!(
            to_verbatim(Path::new(r"C:\t\foo.")),
            PathBuf::from(r"\\?\C:\t\foo.")
        );
        assert_eq!(
            to_verbatim(Path::new(r"C:\t\bar ")),
            PathBuf::from(r"\\?\C:\t\bar ")
        );
        assert_eq!(
            to_verbatim(Path::new(r"C:\d. \x..")),
            PathBuf::from(r"\\?\C:\d. \x..")
        );
        assert_eq!(
            to_verbatim(Path::new(r"\\srv\sh\foo.")),
            PathBuf::from(r"\\?\UNC\srv\sh\foo.")
        );
        // Verbatim input passes through untouched, odd names and all.
        assert_eq!(
            to_verbatim(Path::new(r"\\?\C:\t\foo.\..\bar ")),
            PathBuf::from(r"\\?\C:\t\foo.\..\bar ")
        );
        // Joining a raw name onto a root keeps it.
        let child = Path::new(r"C:\t").join("foo.");
        assert_eq!(to_verbatim(&child), PathBuf::from(r"\\?\C:\t\foo."));
    }

    #[test]
    fn normalize_root_forms() {
        assert_eq!(normalize_root(Path::new(r"C:\")), PathBuf::from(r"C:\"));
        assert_eq!(
            normalize_root(Path::new(r"C:/a/./b/")),
            PathBuf::from(r"C:\a\b")
        );
        assert_eq!(
            normalize_root(Path::new(r"C:\a\foo.")),
            PathBuf::from(r"C:\a\foo.")
        );
        assert_eq!(
            normalize_root(Path::new(r"\\srv\sh\x")),
            PathBuf::from(r"\\srv\sh\x")
        );
        assert_eq!(
            normalize_root(Path::new(r"\\?\C:\x.")),
            PathBuf::from(r"\\?\C:\x.")
        );
        let rel = normalize_root(Path::new("rel"));
        assert!(rel.is_absolute(), "{rel:?}");
        assert!(rel.ends_with("rel"));
    }

    #[test]
    fn only_name_surrogate_reparse_points_are_links() {
        let rp = FILE_ATTRIBUTE_REPARSE_POINT.0;
        assert!(is_link_reparse(rp, 0xA000_000C)); // symlink
        assert!(is_link_reparse(rp, 0xA000_0003)); // mount point / junction
        assert!(!is_link_reparse(rp, 0x9000_001A)); // OneDrive cloud placeholder
        assert!(!is_link_reparse(rp, 0x8000_0013)); // dedup
        assert!(!is_link_reparse(rp, 0x8000_0017)); // WOF
        // Without the attribute, EaSize is an EA size, not a tag.
        assert!(!is_link_reparse(0, 0xA000_000C));
    }
}
