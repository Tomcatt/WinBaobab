//! "Open in Explorer".

use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use windows::Win32::System::SystemInformation::GetWindowsDirectoryW;

use crate::winfs::has_trimmable_name;

/// The raw explorer.exe argument: `"<dir>"` opens a folder, `/select,"<file>"`
/// opens its parent with the file selected. Quoting keeps spaces and commas
/// intact. One trailing backslash is trimmed so the closing quote is never
/// preceded by `\`; a bare drive root (`C:\`) has no spaces or commas and is
/// passed unquoted.
fn explorer_arg(path: &Path, is_dir: bool) -> OsString {
    let mut w: Vec<u16> = path.as_os_str().encode_wide().collect();
    let bs = u16::from(b'\\');
    let is_drive_root = w.len() == 3
        && u8::try_from(w[0]).is_ok_and(|c| c.is_ascii_alphabetic())
        && w[1] == u16::from(b':')
        && w[2] == bs;
    let mut a = OsString::from(if is_dir { "" } else { "/select," });
    if is_drive_root {
        a.push(OsString::from_wide(&w));
        return a;
    }
    if w.last() == Some(&bs) {
        w.pop();
    }
    a.push("\"");
    a.push(OsString::from_wide(&w));
    a.push("\"");
    a
}

/// `%WINDIR%\explorer.exe`, resolved without the search path.
fn explorer_exe() -> Option<PathBuf> {
    let mut buf = [0u16; 260];
    // SAFETY: the buffer is valid for its full length.
    let n = unsafe { GetWindowsDirectoryW(Some(&mut buf)) } as usize;
    if n == 0 || n >= buf.len() {
        return None;
    }
    Some(PathBuf::from(OsString::from_wide(&buf[..n])).join("explorer.exe"))
}

/// What Explorer is actually asked to show. Explorer trims a trailing dot
/// or space from names (`foo.` would open or select its sibling `foo`), so
/// for such a path it opens the deepest enclosing folder whose names are
/// all ordinary, with nothing selected.
fn explorer_target(path: &Path, is_dir: bool) -> (PathBuf, bool) {
    if !has_trimmable_name(path) {
        return (path.to_path_buf(), is_dir);
    }
    let mut p = path;
    while has_trimmable_name(p) {
        match p.parent() {
            Some(parent) => p = parent,
            None => break,
        }
    }
    (p.to_path_buf(), true)
}

/// Fire and forget; explorer.exe's exit code carries no meaning.
pub fn open_in_explorer(path: &Path, is_dir: bool) {
    let (target, is_dir) = explorer_target(path, is_dir);
    if let Some(exe) = explorer_exe() {
        let _ = Command::new(exe)
            .raw_arg(explorer_arg(&target, is_dir))
            .spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arg(p: &str, is_dir: bool) -> String {
        explorer_arg(Path::new(p), is_dir).into_string().unwrap()
    }

    #[test]
    fn dir_with_spaces_and_commas_is_quoted() {
        assert_eq!(arg(r"C:\My Files, old", true), r#""C:\My Files, old""#);
    }

    #[test]
    fn file_is_selected_and_quoted() {
        assert_eq!(arg(r"C:\a b\c,d.txt", false), r#"/select,"C:\a b\c,d.txt""#);
    }

    #[test]
    fn drive_root_is_unquoted() {
        assert_eq!(arg(r"C:\", true), r"C:\");
    }

    #[test]
    fn trailing_backslash_never_precedes_quote() {
        assert_eq!(arg(r"\\srv\share\", true), r#""\\srv\share""#);
        assert_eq!(arg(r"C:\dir\", true), r#""C:\dir""#);
        assert_eq!(arg(r"\\srv\share", true), r#""\\srv\share""#);
    }

    #[test]
    fn trailing_dot_or_space_opens_the_enclosing_plain_folder() {
        let t = |p: &str, d: bool| explorer_target(Path::new(p), d);
        assert_eq!(
            t(r"C:\t\foo.txt", false),
            (PathBuf::from(r"C:\t\foo.txt"), false)
        );
        assert_eq!(t(r"C:\t\foo.", false), (PathBuf::from(r"C:\t"), true));
        assert_eq!(t(r"C:\t\bar ", true), (PathBuf::from(r"C:\t"), true));
        assert_eq!(t(r"C:\t\d.\x", false), (PathBuf::from(r"C:\t"), true));
        assert_eq!(t(r"C:\a \b.\c", true), (PathBuf::from(r"C:\"), true));
    }

    #[test]
    fn explorer_exe_is_in_windows_dir() {
        let exe = explorer_exe().unwrap();
        assert!(exe.is_absolute());
        assert!(exe.ends_with("explorer.exe"));
        assert!(exe.exists());
    }
}
