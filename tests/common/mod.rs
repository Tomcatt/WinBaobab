//! Shared fixtures for the integration tests.

use std::fs;
use std::path::{Path, PathBuf};

use winbaobab::winfs::to_verbatim;

/// Creates nested directories under `base` until the verbatim path is longer
/// than 300 characters (well past MAX_PATH), then a Unicode directory whose
/// name ends in a space. Returns that innermost directory (verbatim form).
pub fn make_long_unicode_dir(base: &Path) -> PathBuf {
    let mut p: PathBuf = to_verbatim(base);
    while p.as_os_str().len() <= 300 {
        p.push("a_fairly_long_directory_name_0123456789");
        fs::create_dir(&p).unwrap();
    }
    p.push("Ünïcødé 🚀 ");
    fs::create_dir(&p).unwrap();
    assert!(p.as_os_str().len() > 300);
    p
}

/// Sizes of the [`make_trimmable_names`] fixture files.
#[allow(dead_code)]
pub const FOO: usize = 100_000;
#[allow(dead_code)]
pub const FOO_DOT: usize = 300_000;
#[allow(dead_code)]
pub const BAR: usize = 500_000;
#[allow(dead_code)]
pub const BAR_SPACE: usize = 700_000;

/// Creates `foo`, `foo.`, `bar` and `bar ` (trailing space) with distinct
/// sizes in a new directory under `base` (Win32 path normalisation would
/// turn `foo.` into `foo` and `bar ` into `bar`, so they are created through
/// verbatim paths). Returns that directory in plain (non-verbatim) form.
#[allow(dead_code)]
pub fn make_trimmable_names(base: &Path) -> PathBuf {
    let dir = base.join("odd");
    fs::create_dir(&dir).unwrap();
    for (name, size) in [
        ("foo", FOO),
        ("foo.", FOO_DOT),
        ("bar", BAR),
        ("bar ", BAR_SPACE),
    ] {
        fs::write(to_verbatim(&dir.join(name)), vec![7u8; size]).unwrap();
    }
    for name in ["foo", "foo.", "bar", "bar "] {
        assert!(exists(&dir.join(name)), "fixture {name:?} missing");
    }
    dir
}

/// Exact-name existence check (no Win32 trimming of trailing dots/spaces).
#[allow(dead_code)]
pub fn exists(path: &Path) -> bool {
    fs::symlink_metadata(to_verbatim(path)).is_ok()
}

/// Exact-name file length.
#[allow(dead_code)]
pub fn len(path: &Path) -> u64 {
    fs::symlink_metadata(to_verbatim(path)).unwrap().len()
}
