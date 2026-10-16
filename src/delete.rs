//! Delete policy (what may be deleted, and whether the Recycle Bin applies)
//! and the delete operations themselves.
//!
//! The one hard rule: [`recycle`] never falls through to a permanent delete.
//! The shell is asked to recycle with a progress sink that vetoes any delete
//! it is about to perform without the Recycle Bin.

use std::ffi::c_void;
use std::fs;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::{E_ABORT, MAX_PATH};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoTaskMemFree, CoUninitialize,
};
use windows::Win32::UI::Shell::{
    FOF_ALLOWUNDO, FOF_NO_UI, FOFX_RECYCLEONDELETE, FileOperation, IFileOperation,
    IFileOperationProgressSink, IFileOperationProgressSink_Impl, IShellItem,
    SHCreateItemFromParsingName, SIGDN_FILESYSPATH, TSF_DELETE_RECYCLE_IF_POSSIBLE,
};
use windows::core::{HRESULT, PCWSTR, Ref, implement};

use crate::drives::{DriveKind, is_volume_root, location_kind};
use crate::model::{NodeId, NodeKind, Tree};
use crate::winfs::{has_trimmable_name, path_attrs, to_verbatim};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteError {
    /// The item could not go to the Recycle Bin; nothing was deleted.
    NotRecyclable,
    Io(String),
}

/// Why the Recycle Bin can't be used on this kind of location, if it can't.
pub fn recycle_block_reason(kind: DriveKind) -> Option<&'static str> {
    match kind {
        DriveKind::Network => Some("Network drives have no Recycle Bin."),
        DriveKind::Removable => Some("Removable drives have no Recycle Bin."),
        DriveKind::Fixed | DriveKind::Other => None,
    }
}

pub fn can_delete(tree: &Tree, id: NodeId, scan_running: bool) -> bool {
    if scan_running || id == Tree::ROOT {
        return false;
    }
    let n = tree.node(id);
    if n.removed || n.kind == NodeKind::Unseen {
        return false;
    }
    !is_volume_root(&tree.path_of(id))
}

fn io_msg(path: &Path, e: impl std::fmt::Display) -> DeleteError {
    DeleteError::Io(format!("{}: {e}", path.display()))
}

/// Moves `path` (file or directory) to the Recycle Bin. Returns
/// `NotRecyclable` on network/removable locations without calling the shell,
/// and whenever the shell would have deleted without recycling.
///
/// The path is never re-normalised (that would turn `foo.` into its sibling
/// `foo`). A path with a name ending in a dot or a space is refused up
/// front with `NotRecyclable`: the shell rejects such `\\?\` paths and, given
/// the item another way, still deletes through a plain path and hits the
/// trimmed sibling (`bar ` recycles `bar`; seen on Windows 11 26200). For
/// every other path the shell's own idea of the item is checked against the
/// intended path both before the operation and in `PreDeleteItem`; on any
/// mismatch nothing is touched and the result is `NotRecyclable`.
pub fn recycle(path: &Path) -> Result<(), DeleteError> {
    let verbatim = to_verbatim(path);
    if recycle_block_reason(location_kind(&verbatim)).is_some()
        || has_trimmable_name(&display_path(&verbatim))
    {
        return Err(DeleteError::NotRecyclable);
    }
    std::thread::spawn(move || recycle_on_sta(&verbatim))
        .join()
        .unwrap_or_else(|_| Err(DeleteError::Io("recycle thread panicked".into())))
}

struct ComGuard;
impl Drop for ComGuard {
    fn drop(&mut self) {
        // SAFETY: paired with a successful CoInitializeEx on this thread.
        unsafe { CoUninitialize() };
    }
}

fn recycle_on_sta(path: &Path) -> Result<(), DeleteError> {
    // SAFETY: called once on a fresh thread; balanced by ComGuard.
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
        .ok()
        .map_err(|e| io_msg(path, format!("COM init failed: {e}")))?;
    let _com = ComGuard;
    // Every COM object lives inside shell_recycle and is released before
    // `_com` uninitializes COM.
    shell_recycle(path)?;
    if fs::symlink_metadata(path).is_ok() {
        return Err(io_msg(path, "still exists after moving to the Recycle Bin"));
    }
    Ok(())
}

/// `\\?\C:\x` -> `C:\x`, `\\?\UNC\s\h` -> `\\s\h`; other paths unchanged.
fn display_path(path: &Path) -> PathBuf {
    let s = path.as_os_str().to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(r) => match r.strip_prefix(r"UNC\") {
            Some(unc) => PathBuf::from(format!(r"\\{unc}")),
            None => PathBuf::from(r),
        },
        None => path.to_path_buf(),
    }
}

/// Case-insensitive comparison of two paths in display form.
fn same_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        display_path(p)
            .as_os_str()
            .to_string_lossy()
            .trim_end_matches('\\')
            .to_lowercase()
    };
    norm(a) == norm(b)
}

/// True if `p` is `root` or lies inside it (a folder's contents may be
/// reported item by item).
fn same_or_inside(p: &Path, root: &Path) -> bool {
    if same_path(p, root) {
        return true;
    }
    let norm = |x: &Path| display_path(x).as_os_str().to_string_lossy().to_lowercase();
    let root = norm(root);
    let root = root.trim_end_matches('\\');
    norm(p)
        .strip_prefix(root)
        .is_some_and(|rest| rest.starts_with('\\'))
}

/// The file-system path the shell associates with `item`, if any.
fn item_path(item: &IShellItem) -> Option<PathBuf> {
    // SAFETY: COM call on a live interface; the returned string is copied
    // and then freed with CoTaskMemFree.
    unsafe {
        let p = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const c_void));
        s.map(PathBuf::from)
    }
}

fn parse_item(path: &Path) -> windows::core::Result<IShellItem> {
    let w: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `w` is NUL-terminated and outlives the call.
    unsafe { SHCreateItemFromParsingName(PCWSTR(w.as_ptr()), None) }
}

fn shell_recycle(path: &Path) -> Result<(), DeleteError> {
    let plain = display_path(path);
    let long = plain.as_os_str().encode_wide().count() >= MAX_PATH as usize;
    let item: IShellItem = if long {
        // The shell can't handle this long path; nothing was touched.
        parse_item(path).map_err(|_| DeleteError::NotRecyclable)?
    } else {
        parse_item(&plain)
            .map_err(|e| io_msg(path, format!("the shell can't open this path ({e})")))?
    };
    // The shell must mean exactly this item, not a trimmed sibling.
    if !item_path(&item).is_some_and(|p| same_path(&p, &plain)) {
        return Err(DeleteError::NotRecyclable);
    }
    // SAFETY: in-process COM activation on this thread's initialized STA.
    let op: IFileOperation =
        unsafe { CoCreateInstance(&FileOperation, None, CLSCTX_INPROC_SERVER) }
            .map_err(|e| io_msg(path, e))?;
    let refused = Arc::new(AtomicBool::new(false));
    let not_recycled = Arc::new(AtomicBool::new(false));
    let sink: IFileOperationProgressSink = RecycleOnlySink {
        refused: refused.clone(),
        not_recycled: not_recycled.clone(),
        expected: plain.clone(),
    }
    .into();
    // SAFETY: COM calls on live interfaces owned by this function.
    let performed = unsafe {
        op.SetOperationFlags(FOF_NO_UI | FOFX_RECYCLEONDELETE | FOF_ALLOWUNDO)
            .map_err(|e| io_msg(path, e))?;
        let cookie = op.Advise(&sink).map_err(|e| io_msg(path, e))?;
        let r = op
            .DeleteItem(&item, None)
            .and_then(|()| op.PerformOperations());
        let _ = op.Unadvise(cookie);
        r
    };
    if refused.load(Ordering::SeqCst) {
        return Err(DeleteError::NotRecyclable);
    }
    if not_recycled.load(Ordering::SeqCst) {
        // Should be impossible given PreDeleteItem's veto; never report Ok.
        return Err(io_msg(path, "deleted but not recycled"));
    }
    performed.map_err(|e| io_msg(path, e))?;
    // SAFETY: COM call on a live interface.
    let aborted = unsafe { op.GetAnyOperationsAborted() }.map_err(|e| io_msg(path, e))?;
    if aborted.as_bool() {
        return Err(io_msg(path, "the Recycle Bin operation was aborted"));
    }
    Ok(())
}

/// Vetoes (E_ABORT) any delete the shell is about to perform without the
/// Recycle Bin, and records that it did. As a second line of defence it
/// also records a successful delete that produced no Recycle Bin item.
#[implement(IFileOperationProgressSink)]
struct RecycleOnlySink {
    refused: Arc<AtomicBool>,
    not_recycled: Arc<AtomicBool>,
    /// The only item this operation may delete (display form).
    expected: PathBuf,
}

impl IFileOperationProgressSink_Impl for RecycleOnlySink_Impl {
    fn PreDeleteItem(&self, dwflags: u32, item: Ref<IShellItem>) -> windows::core::Result<()> {
        let right_item = item
            .as_ref()
            .and_then(item_path)
            .is_some_and(|p| same_or_inside(&p, &self.expected));
        if dwflags & TSF_DELETE_RECYCLE_IF_POSSIBLE.0 as u32 != 0 && right_item {
            Ok(())
        } else {
            self.refused.store(true, Ordering::SeqCst);
            Err(E_ABORT.into())
        }
    }
    fn StartOperations(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn FinishOperations(&self, _hr: HRESULT) -> windows::core::Result<()> {
        Ok(())
    }
    fn PreRenameItem(
        &self,
        _f: u32,
        _i: Ref<IShellItem>,
        _n: &PCWSTR,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn PostRenameItem(
        &self,
        _f: u32,
        _i: Ref<IShellItem>,
        _n: &PCWSTR,
        _hr: HRESULT,
        _new: Ref<IShellItem>,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn PreMoveItem(
        &self,
        _f: u32,
        _i: Ref<IShellItem>,
        _d: Ref<IShellItem>,
        _n: &PCWSTR,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn PostMoveItem(
        &self,
        _f: u32,
        _i: Ref<IShellItem>,
        _d: Ref<IShellItem>,
        _n: &PCWSTR,
        _hr: HRESULT,
        _new: Ref<IShellItem>,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn PreCopyItem(
        &self,
        _f: u32,
        _i: Ref<IShellItem>,
        _d: Ref<IShellItem>,
        _n: &PCWSTR,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn PostCopyItem(
        &self,
        _f: u32,
        _i: Ref<IShellItem>,
        _d: Ref<IShellItem>,
        _n: &PCWSTR,
        _hr: HRESULT,
        _new: Ref<IShellItem>,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn PostDeleteItem(
        &self,
        _f: u32,
        _i: Ref<IShellItem>,
        hrdelete: HRESULT,
        newly_created: Ref<IShellItem>,
    ) -> windows::core::Result<()> {
        // A recycled item reports its new location in the bin.
        if hrdelete.is_ok() && newly_created.is_null() {
            self.not_recycled.store(true, Ordering::SeqCst);
        }
        Ok(())
    }
    fn PreNewItem(&self, _f: u32, _d: Ref<IShellItem>, _n: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }
    fn PostNewItem(
        &self,
        _f: u32,
        _d: Ref<IShellItem>,
        _n: &PCWSTR,
        _t: &PCWSTR,
        _attrs: u32,
        _hr: HRESULT,
        _new: Ref<IShellItem>,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn UpdateProgress(&self, _total: u32, _done: u32) -> windows::core::Result<()> {
        Ok(())
    }
    fn ResetTimer(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn PauseTimer(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn ResumeTimer(&self) -> windows::core::Result<()> {
        Ok(())
    }
}

/// Deletes a file or a whole directory tree, skipping the Recycle Bin.
/// Junctions and symlinks are removed as links; their targets are never
/// touched. On a partial failure returns the first error.
pub fn delete_permanently(path: &Path) -> Result<(), DeleteError> {
    let v = to_verbatim(path);
    match remove_once(&v) {
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            let cleared = set_readonly(&v, false);
            let r = remove_once(&v);
            if r.is_err() && cleared {
                // Leave the item as we found it.
                set_readonly(&v, true);
            }
            r
        }
        r => r,
    }
    .map_err(|e| io_msg(path, e))
}

fn remove_once(v: &Path) -> io::Result<()> {
    let a = path_attrs(v)?;
    match (a.is_dir, a.is_link) {
        // Directory junction or symlink: remove the link itself only.
        (true, true) => fs::remove_dir(v),
        // Real directory (including cloud/dedup/WOF reparse directories).
        // remove_dir_all removes links inside the tree without descending.
        (true, false) => fs::remove_dir_all(v),
        (false, _) => fs::remove_file(v),
    }
}

/// Sets or clears the read-only attribute; true if it changed it.
fn set_readonly(v: &Path, readonly: bool) -> bool {
    let Ok(m) = fs::symlink_metadata(v) else {
        return false;
    };
    let mut perm = m.permissions();
    if perm.readonly() == readonly {
        return false;
    }
    #[allow(clippy::permissions_set_readonly_false)]
    perm.set_readonly(readonly);
    fs::set_permissions(v, perm).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tree() -> (Tree, NodeId, NodeId) {
        let mut t = Tree::new(PathBuf::from(r"C:\data"));
        let f = t.add_child(Tree::ROOT, "f.txt", NodeKind::File, 5, None);
        t.set_unseen(100);
        let u = t
            .node(Tree::ROOT)
            .children
            .iter()
            .copied()
            .find(|&c| t.node(c).kind == NodeKind::Unseen)
            .unwrap();
        (t, f, u)
    }

    #[test]
    fn recycle_blocked_on_network_and_removable() {
        assert_eq!(
            recycle_block_reason(DriveKind::Network),
            Some("Network drives have no Recycle Bin.")
        );
        assert_eq!(
            recycle_block_reason(DriveKind::Removable),
            Some("Removable drives have no Recycle Bin.")
        );
    }

    #[test]
    fn recycle_allowed_on_fixed() {
        assert_eq!(recycle_block_reason(DriveKind::Fixed), None);
    }

    #[test]
    fn cannot_delete_root_or_unseen() {
        let (t, _, u) = tree();
        assert!(!can_delete(&t, Tree::ROOT, false));
        assert!(!can_delete(&t, u, false));
    }

    #[test]
    fn cannot_delete_while_scanning() {
        let (t, f, _) = tree();
        assert!(!can_delete(&t, f, true));
    }

    #[test]
    fn can_delete_normal_file() {
        let (t, f, _) = tree();
        assert!(can_delete(&t, f, false));
    }

    #[test]
    fn cannot_delete_removed() {
        let (mut t, f, _) = tree();
        t.remove_subtree(f);
        assert!(!can_delete(&t, f, false));
    }

    #[test]
    fn trimmable_names() {
        assert!(has_trimmable_name(Path::new(r"C:\t\foo.")));
        assert!(has_trimmable_name(Path::new(r"C:\t\bar ")));
        assert!(has_trimmable_name(Path::new(r"C:\d.\x")));
        assert!(has_trimmable_name(Path::new(r"\\s\h\x ")));
        assert!(!has_trimmable_name(Path::new(r"C:\t\foo.txt")));
        assert!(!has_trimmable_name(Path::new(r"C:\")));
        assert!(!has_trimmable_name(Path::new(r"\\s\h\x")));
    }

    #[test]
    fn recycle_refuses_trimmable_names_without_touching_anything() {
        // Refused before the shell or the file system is involved, so the
        // (nonexistent) paths are never looked up.
        for p in [
            r"C:\nonexistent-winbaobab\foo.",
            r"C:\nonexistent-winbaobab\bar ",
        ] {
            assert_eq!(recycle(Path::new(p)), Err(DeleteError::NotRecyclable));
        }
    }

    #[test]
    fn shell_paths_must_match_exactly() {
        assert_eq!(
            display_path(Path::new(r"\\?\C:\t\foo.")),
            PathBuf::from(r"C:\t\foo.")
        );
        assert_eq!(
            display_path(Path::new(r"\\?\UNC\s\h\x")),
            PathBuf::from(r"\\s\h\x")
        );
        assert!(same_path(
            Path::new(r"c:\T\foo."),
            Path::new(r"\\?\C:\t\foo.")
        ));
        assert!(!same_path(Path::new(r"C:\t\foo"), Path::new(r"C:\t\foo.")));
        assert!(!same_path(Path::new(r"C:\t\bar"), Path::new(r"C:\t\bar ")));
        assert!(same_or_inside(
            Path::new(r"C:\t\d.\x"),
            Path::new(r"C:\t\d.")
        ));
        assert!(!same_or_inside(
            Path::new(r"C:\t\d\x"),
            Path::new(r"C:\t\d.")
        ));
        assert!(!same_or_inside(Path::new(r"C:\t\dx"), Path::new(r"C:\t\d")));
    }
}
