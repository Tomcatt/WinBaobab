mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use winbaobab::delete::{DeleteError, delete_permanently, recycle};

#[test]
fn permanent_delete_file() {
    let t = tempfile::tempdir().unwrap();
    let f = t.path().join("a.txt");
    fs::write(&f, b"hello").unwrap();
    delete_permanently(&f).unwrap();
    assert!(!f.exists());
}

#[test]
fn permanent_delete_readonly_file() {
    let t = tempfile::tempdir().unwrap();
    let f = t.path().join("ro.txt");
    fs::write(&f, b"hello").unwrap();
    let mut perm = fs::metadata(&f).unwrap().permissions();
    perm.set_readonly(true);
    fs::set_permissions(&f, perm).unwrap();
    delete_permanently(&f).unwrap();
    assert!(!f.exists());
}

#[test]
fn permanent_delete_dir_tree() {
    let t = tempfile::tempdir().unwrap();
    let d = t.path().join("tree");
    fs::create_dir_all(d.join("x").join("y")).unwrap();
    fs::write(d.join("a"), b"a").unwrap();
    fs::write(d.join("x").join("b"), b"b").unwrap();
    fs::write(d.join("x").join("y").join("c"), b"c").unwrap();
    delete_permanently(&d).unwrap();
    assert!(!d.exists());
    assert!(t.path().exists());
}

#[test]
fn permanent_delete_dir_with_readonly_file() {
    let t = tempfile::tempdir().unwrap();
    let d = t.path().join("tree");
    fs::create_dir(&d).unwrap();
    let f = d.join("ro.txt");
    fs::write(&f, b"x").unwrap();
    let mut perm = fs::metadata(&f).unwrap().permissions();
    perm.set_readonly(true);
    fs::set_permissions(&f, perm).unwrap();
    delete_permanently(&d).unwrap();
    assert!(!d.exists());
}

#[test]
fn permanent_delete_long_path() {
    let t = tempfile::tempdir().unwrap();
    let p = common::make_long_unicode_dir(t.path());
    let f = p.join("x.txt");
    fs::write(&f, b"x").unwrap();
    // The file alone, by its >260-char path...
    delete_permanently(&f).unwrap();
    assert!(!f.exists());
    // ...then the whole long chain from its top directory.
    fs::write(&f, b"x").unwrap();
    let top = t.path().join("a_fairly_long_directory_name_0123456789");
    delete_permanently(&top).unwrap();
    assert!(!top.exists());
    assert!(!p.exists());
}

#[test]
fn permanent_delete_does_not_follow_junctions() {
    let outer = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let keep = elsewhere.path().join("keep.txt");
    fs::write(&keep, b"must survive").unwrap();

    let d = outer.path().join("victim");
    fs::create_dir(&d).unwrap();
    fs::write(d.join("own.txt"), b"x").unwrap();
    let st = Command::new("cmd")
        .arg("/C")
        .arg("mklink")
        .arg("/J")
        .arg(d.join("link"))
        .arg(elsewhere.path())
        .output()
        .unwrap();
    assert!(st.status.success(), "mklink failed: {st:?}");
    assert!(d.join("link").join("keep.txt").exists());

    delete_permanently(&d).unwrap();
    assert!(!d.exists());
    assert!(keep.exists(), "junction target contents were deleted");
    assert_eq!(fs::read(&keep).unwrap(), b"must survive");
}

#[test]
fn permanent_delete_of_junction_removes_only_the_link() {
    let outer = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let keep = elsewhere.path().join("keep.txt");
    fs::write(&keep, b"must survive").unwrap();

    let link = outer.path().join("link");
    let st = Command::new("cmd")
        .arg("/C")
        .arg("mklink")
        .arg("/J")
        .arg(&link)
        .arg(elsewhere.path())
        .output()
        .unwrap();
    assert!(st.status.success(), "mklink failed: {st:?}");

    delete_permanently(&link).unwrap();
    assert!(fs::symlink_metadata(&link).is_err(), "link still there");
    assert!(elsewhere.path().is_dir());
    assert_eq!(fs::read(&keep).unwrap(), b"must survive");
}

#[test]
fn delete_missing_path_is_io_error() {
    let t = tempfile::tempdir().unwrap();
    let r = delete_permanently(&t.path().join("nope"));
    assert!(matches!(r, Err(DeleteError::Io(_))), "got {r:?}");
}

#[test]
fn recycle_refuses_network_path() {
    let r = recycle(Path::new(r"\\127.0.0.1\c$\nonexistent"));
    assert_eq!(r, Err(DeleteError::NotRecyclable));
}

#[test]
fn recycle_missing_local_path_is_io_error() {
    let t = tempfile::tempdir().unwrap();
    let r = recycle(&t.path().join("nope"));
    assert!(matches!(r, Err(DeleteError::Io(_))), "got {r:?}");
}

/// Serializes the tests that count Recycle Bin items.
static BIN_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn recycle_bin_items(drive_root: &Path) -> i64 {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::UI::Shell::{SHQUERYRBINFO, SHQueryRecycleBinW};
    use windows::core::PCWSTR;
    let w: Vec<u16> = drive_root
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut info = SHQUERYRBINFO {
        cbSize: std::mem::size_of::<SHQUERYRBINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: `w` is NUL-terminated and `info` has cbSize set.
    unsafe { SHQueryRecycleBinW(PCWSTR(w.as_ptr()), &mut info) }.unwrap();
    info.i64NumItems
}

/// Run manually: `cargo test -- --ignored`. Leaves one tiny file in the
/// Recycle Bin of the drive holding %TEMP%.
#[test]
#[ignore]
fn recycle_local_file_goes_to_bin() {
    let _bin = BIN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let f = t.path().join("winbaobab-recycle-test.txt");
    fs::write(&f, b"winbaobab recycle test").unwrap();
    let root = t.path().ancestors().last().unwrap().to_path_buf();
    let before = recycle_bin_items(&root);
    // Ok also proves the sink saw a Recycle Bin item for the delete (it
    // would have returned Io("deleted but not recycled") otherwise).
    assert_eq!(recycle(&f), Ok(()));
    assert!(!f.exists());
    assert_eq!(recycle_bin_items(&root), before + 1);
}

struct SubstGuard(char);
impl Drop for SubstGuard {
    fn drop(&mut self) {
        let _ = Command::new("subst")
            .arg(format!("{}:", self.0))
            .arg("/D")
            .output();
    }
}

fn free_drive_letter() -> char {
    use windows::Win32::Storage::FileSystem::GetLogicalDrives;
    // SAFETY: no arguments; returns a bitmask of used drive letters.
    let used = unsafe { GetLogicalDrives() };
    (b'D'..=b'Z')
        .rev()
        .find(|&c| used & (1 << (c - b'A')) == 0)
        .map(char::from)
        .expect("no free drive letter")
}

/// Run manually: `cargo test -- --ignored`. Recycling through a `subst`
/// drive historically deletes permanently in Explorer; ours must either
/// really recycle or refuse with the file intact.
#[test]
#[ignore]
fn recycle_on_subst_drive_never_permanent() {
    let _bin = BIN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let name = "winbaobab-subst-test.txt";
    let real = t.path().join(name);
    fs::write(&real, b"winbaobab subst test").unwrap();

    let letter = free_drive_letter();
    let st = Command::new("subst")
        .arg(format!("{letter}:"))
        .arg(t.path())
        .output()
        .unwrap();
    assert!(st.status.success(), "subst failed: {st:?}");
    let _guard = SubstGuard(letter);

    let via = PathBuf::from(format!(r"{letter}:\{name}"));
    assert!(via.exists());
    let root = t.path().ancestors().last().unwrap().to_path_buf();
    let before = recycle_bin_items(&root);
    let r = recycle(&via);
    let after = recycle_bin_items(&root);
    eprintln!("subst {letter}: recycle -> {r:?}, bin {before} -> {after}");
    match r {
        Ok(()) => {
            assert!(!real.exists());
            assert_eq!(after, before + 1, "file gone but not in the Recycle Bin");
        }
        Err(DeleteError::NotRecyclable) | Err(DeleteError::Io(_)) => {
            assert!(real.exists(), "recycle failed but the file is gone");
        }
    }
}

/// Asserts which of the four fixture files are still there.
fn assert_left(dir: &Path, expect: [(&str, bool); 4]) {
    for (name, there) in expect {
        assert_eq!(common::exists(&dir.join(name)), there, "{name:?}");
    }
}

#[test]
fn permanent_delete_hits_only_the_trailing_dot_name() {
    let t = tempfile::tempdir().unwrap();
    let d = common::make_trimmable_names(t.path());
    delete_permanently(&d.join("foo.")).unwrap();
    assert_left(
        &d,
        [
            ("foo", true),
            ("foo.", false),
            ("bar", true),
            ("bar ", true),
        ],
    );
    assert_eq!(common::len(&d.join("foo")), common::FOO as u64);
    delete_permanently(&d.join("foo")).unwrap();
    assert_left(
        &d,
        [
            ("foo", false),
            ("foo.", false),
            ("bar", true),
            ("bar ", true),
        ],
    );
}

#[test]
fn permanent_delete_hits_only_the_trailing_space_name() {
    let t = tempfile::tempdir().unwrap();
    let d = common::make_trimmable_names(t.path());
    delete_permanently(&d.join("bar ")).unwrap();
    assert_left(
        &d,
        [
            ("foo", true),
            ("foo.", true),
            ("bar", true),
            ("bar ", false),
        ],
    );
    assert_eq!(common::len(&d.join("bar")), common::BAR as u64);
    delete_permanently(&d.join("bar")).unwrap();
    assert_left(
        &d,
        [
            ("foo", true),
            ("foo.", true),
            ("bar", false),
            ("bar ", false),
        ],
    );
}

#[test]
fn permanent_delete_of_plain_name_spares_the_trailing_dot_sibling() {
    let t = tempfile::tempdir().unwrap();
    let d = common::make_trimmable_names(t.path());
    delete_permanently(&d.join("foo")).unwrap();
    assert_left(
        &d,
        [
            ("foo", false),
            ("foo.", true),
            ("bar", true),
            ("bar ", true),
        ],
    );
    assert_eq!(common::len(&d.join("foo.")), common::FOO_DOT as u64);
}

#[test]
fn permanent_delete_inside_a_trailing_dot_directory() {
    let t = tempfile::tempdir().unwrap();
    let v = winbaobab::winfs::to_verbatim(t.path());
    fs::create_dir(v.join("d")).unwrap();
    fs::create_dir(v.join("d.")).unwrap();
    fs::write(v.join("d").join("x"), b"plain").unwrap();
    fs::write(v.join("d.").join("x"), b"dotted").unwrap();
    delete_permanently(&t.path().join("d.").join("x")).unwrap();
    assert!(common::exists(&t.path().join("d").join("x")));
    assert!(!common::exists(&t.path().join("d.").join("x")));
    delete_permanently(&t.path().join("d.")).unwrap();
    assert!(common::exists(&t.path().join("d").join("x")));
    assert!(!common::exists(&t.path().join("d.")));
}

/// The shell can't recycle a name ending in a dot or space without hitting
/// its trimmed sibling, so `recycle` must refuse it and touch nothing (the
/// refusal happens before the shell is involved, so this runs by default).
#[test]
fn recycle_refuses_trailing_dot_and_space_names() {
    let _bin = BIN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for target in ["foo.", "bar "] {
        let t = tempfile::tempdir().unwrap();
        let d = common::make_trimmable_names(t.path());
        let root = t.path().ancestors().last().unwrap().to_path_buf();
        let before = recycle_bin_items(&root);
        assert_eq!(recycle(&d.join(target)), Err(DeleteError::NotRecyclable));
        assert_left(
            &d,
            [("foo", true), ("foo.", true), ("bar", true), ("bar ", true)],
        );
        assert_eq!(recycle_bin_items(&root), before, "{target:?}");
    }
    // Also inside a folder whose own name ends in a dot.
    let t = tempfile::tempdir().unwrap();
    let v = winbaobab::winfs::to_verbatim(t.path());
    fs::create_dir(v.join("d")).unwrap();
    fs::create_dir(v.join("d.")).unwrap();
    fs::write(v.join("d").join("x"), b"plain").unwrap();
    fs::write(v.join("d.").join("x"), b"dotted").unwrap();
    assert_eq!(
        recycle(&t.path().join("d.").join("x")),
        Err(DeleteError::NotRecyclable)
    );
    assert!(common::exists(&t.path().join("d").join("x")));
    assert!(common::exists(&t.path().join("d.").join("x")));
}

/// Recycles the plain `target` in a fresh fixture and checks that exactly
/// it went to the Recycle Bin and its trailing-dot/space sibling stayed.
fn recycle_only(target: &str) {
    let _bin = BIN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let d = common::make_trimmable_names(t.path());
    let root = t.path().ancestors().last().unwrap().to_path_buf();
    let before = recycle_bin_items(&root);
    let r = recycle(&d.join(target));
    let after = recycle_bin_items(&root);
    eprintln!("recycle {target:?} -> {r:?}, bin {before} -> {after}");
    assert_eq!(r, Ok(()));
    for n in ["foo", "foo.", "bar", "bar "] {
        assert_eq!(common::exists(&d.join(n)), n != target, "{n:?}");
    }
    assert_eq!(after, before + 1, "not in the Recycle Bin");
}

/// Run manually: `cargo test -- --ignored`. Leaves one test file (under
/// 1 MB) in the Recycle Bin of the drive holding %TEMP%.
#[test]
#[ignore]
fn recycle_of_plain_name_spares_the_trailing_dot_sibling() {
    recycle_only("foo");
}

/// Run manually: `cargo test -- --ignored`. Leaves one test file (under
/// 1 MB) in the Recycle Bin of the drive holding %TEMP%.
#[test]
#[ignore]
fn recycle_of_plain_name_spares_the_trailing_space_sibling() {
    recycle_only("bar");
}

/// Run manually: `cargo test -- --ignored`. Leaves one small folder in the
/// Recycle Bin of the drive holding %TEMP%. Proves the exact-item check in
/// the progress sink still lets a whole folder through.
#[test]
#[ignore]
fn recycle_local_dir_goes_to_bin() {
    let _bin = BIN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let d = t.path().join("winbaobab-recycle-dir");
    fs::create_dir_all(d.join("sub")).unwrap();
    fs::write(d.join("a.txt"), b"a").unwrap();
    fs::write(d.join("sub").join("b.txt"), b"b").unwrap();
    let root = t.path().ancestors().last().unwrap().to_path_buf();
    let before = recycle_bin_items(&root);
    assert_eq!(recycle(&d), Ok(()));
    assert!(!d.exists());
    assert_eq!(recycle_bin_items(&root), before + 1);
}
