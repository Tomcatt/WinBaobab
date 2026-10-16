mod common;

use std::fs;
use std::path::Path;
use std::process::Command;

use winbaobab::winfs::read_dir_entries;

fn find<'a>(v: &'a [winbaobab::winfs::RawEntry], name: &str) -> &'a winbaobab::winfs::RawEntry {
    v.iter()
        .find(|e| e.name == name)
        .unwrap_or_else(|| panic!("missing {name}"))
}

#[test]
fn enumerates_files_and_dirs() {
    let t = tempfile::tempdir().unwrap();
    fs::write(t.path().join("a.bin"), vec![1u8; 100_000]).unwrap();
    fs::create_dir(t.path().join("sub")).unwrap();
    let v = read_dir_entries(t.path()).unwrap();
    assert_eq!(v.len(), 2);
    let a = find(&v, "a.bin");
    assert!(!a.is_dir && !a.is_reparse);
    assert!(a.alloc_size >= 100_000);
    assert!(find(&v, "sub").is_dir);
}

#[test]
fn empty_dir_is_empty() {
    let t = tempfile::tempdir().unwrap();
    assert!(read_dir_entries(t.path()).unwrap().is_empty());
}

#[test]
fn missing_dir_errors() {
    let t = tempfile::tempdir().unwrap();
    let e = read_dir_entries(&t.path().join("nope")).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn flags_junctions_as_reparse() {
    let t = tempfile::tempdir().unwrap();
    let target = t.path().join("target");
    fs::create_dir(&target).unwrap();
    let st = Command::new("cmd")
        .args(["/C", "mklink", "/J", "link", "target"])
        .current_dir(t.path())
        .output()
        .unwrap();
    assert!(st.status.success(), "mklink failed: {st:?}");
    let v = read_dir_entries(t.path()).unwrap();
    assert!(find(&v, "link").is_reparse);
    assert!(!find(&v, "target").is_reparse);
}

#[test]
fn hardlinks_share_file_id() {
    let t = tempfile::tempdir().unwrap();
    let a = t.path().join("a");
    fs::write(&a, b"hello").unwrap();
    fs::hard_link(&a, t.path().join("b")).unwrap();
    let v = read_dir_entries(t.path()).unwrap();
    let (a, b) = (find(&v, "a"), find(&v, "b"));
    assert_ne!(a.file_id, 0);
    assert_eq!(a.file_id, b.file_id);
}

#[test]
fn enumerates_long_and_unicode_paths() {
    let t = tempfile::tempdir().unwrap();
    let p = common::make_long_unicode_dir(t.path());
    fs::write(p.join("x.txt"), b"x").unwrap();
    let v = read_dir_entries(&p).unwrap();
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].name, "x.txt");
    // Also reachable from the parent with the exact name, trailing space intact.
    let parent: &Path = p.parent().unwrap();
    let pv = read_dir_entries(parent).unwrap();
    assert!(find(&pv, "Ünïcødé 🚀 ").is_dir);
}

#[test]
fn enumerates_many_files_across_batches() {
    let t = tempfile::tempdir().unwrap();
    for i in 0..2000 {
        let name = format!("{i:05}_{}", "n".repeat(100));
        fs::write(t.path().join(name), b"").unwrap();
    }
    let v = read_dir_entries(t.path()).unwrap();
    assert_eq!(v.len(), 2000);
    let names: std::collections::HashSet<_> = v.iter().map(|e| e.name.clone()).collect();
    assert_eq!(names.len(), 2000);
}

#[test]
fn scan_real_temp_dir() {
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};
    use winbaobab::model::Tree;
    use winbaobab::scan::{ScanState, WinDirSource, start_scan};

    let t = tempfile::tempdir().unwrap();
    fs::write(t.path().join("a"), vec![1u8; 1_000]).unwrap();
    fs::write(t.path().join("b"), vec![1u8; 2_000]).unwrap();
    fs::create_dir(t.path().join("sub")).unwrap();
    fs::write(t.path().join("sub").join("c"), vec![1u8; 3_000]).unwrap();

    let h = start_scan(t.path().to_path_buf(), Arc::new(WinDirSource), None);
    let start = Instant::now();
    while h.state() == ScanState::Running {
        assert!(start.elapsed() < Duration::from_secs(10), "scan timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(h.state(), ScanState::Done);
    assert!(h.tree.read().unwrap().node(Tree::ROOT).size >= 6_000);
    assert_eq!(h.tree.read().unwrap().node(Tree::ROOT).items, 4);
    assert_eq!(h.progress.items.load(Ordering::Relaxed), 4);
}

#[test]
fn enumerates_trailing_dot_and_space_names_exactly() {
    let t = tempfile::tempdir().unwrap();
    let d = common::make_trimmable_names(t.path());
    let v = read_dir_entries(&d).unwrap();
    assert_eq!(v.len(), 4);
    for (name, size) in [
        ("foo", common::FOO),
        ("foo.", common::FOO_DOT),
        ("bar", common::BAR),
        ("bar ", common::BAR_SPACE),
    ] {
        let e = find(&v, name);
        assert!(!e.is_dir);
        let size = size as u64;
        assert!(
            e.alloc_size >= size && e.alloc_size < size + 65_536,
            "{name:?}: {} for {size}",
            e.alloc_size
        );
    }
}

#[test]
fn scan_lists_trailing_dot_and_space_names_with_their_sizes() {
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use winbaobab::model::Tree;
    use winbaobab::scan::{ScanState, WinDirSource, start_scan};

    let t = tempfile::tempdir().unwrap();
    let d = common::make_trimmable_names(t.path());
    // A directory `d.` next to `d`, each with its own file: the scan must
    // descend into the right one.
    let v = winbaobab::winfs::to_verbatim(&d);
    fs::create_dir(v.join("d")).unwrap();
    fs::create_dir(v.join("d.")).unwrap();
    fs::write(v.join("d").join("in_d"), vec![1u8; 50_000]).unwrap();
    fs::write(v.join("d.").join("in_d_dot"), vec![1u8; 90_000]).unwrap();

    let h = start_scan(d.clone(), Arc::new(WinDirSource), None);
    let start = Instant::now();
    while h.state() == ScanState::Running {
        assert!(start.elapsed() < Duration::from_secs(10), "scan timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(h.state(), ScanState::Done);
    let tree = h.tree.read().unwrap();
    let child = |parent, name: &str| {
        tree.node(parent)
            .children
            .iter()
            .copied()
            .find(|&c| &*tree.node(c).name == name)
            .unwrap_or_else(|| panic!("missing {name:?}"))
    };
    for (name, size) in [
        ("foo", common::FOO),
        ("foo.", common::FOO_DOT),
        ("bar", common::BAR),
        ("bar ", common::BAR_SPACE),
    ] {
        let id = child(Tree::ROOT, name);
        let got = tree.node(id).size;
        let size = size as u64;
        assert!(
            got >= size && got < size + 65_536,
            "{name:?}: {got} for {size}"
        );
        assert_eq!(tree.path_of(id), d.join(name));
    }
    let dd = child(Tree::ROOT, "d.");
    let plain = child(Tree::ROOT, "d");
    child(dd, "in_d_dot");
    child(plain, "in_d");
    assert_eq!(tree.node(dd).children.len(), 1);
    assert_eq!(tree.node(plain).children.len(), 1);
    assert!(tree.node(dd).size >= 90_000 && tree.node(dd).size < 90_000 + 65_536);
    assert!(tree.node(plain).size >= 50_000 && tree.node(plain).size < 50_000 + 65_536);
}
