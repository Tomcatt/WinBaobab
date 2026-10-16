use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};

const MAX_RECENT: usize = 10;
const MAX_FILE_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Chart {
    Rings,
    Treemap,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecentScan {
    pub path: PathBuf,
    pub when_unix: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub warn_permanent_delete: bool,
    pub default_chart: Chart,
    pub recent: Vec<RecentScan>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            warn_permanent_delete: true,
            default_chart: Chart::Rings,
            recent: Vec::new(),
        }
    }
}

/// Lowercased path text without trailing backslashes, for duplicate detection.
fn recent_key(path: &Path) -> String {
    path.to_string_lossy()
        .to_lowercase()
        .trim_end_matches('\\')
        .to_string()
}

impl Settings {
    /// `%APPDATA%\WinBaobab\settings.json`, or under the current dir if APPDATA is unset.
    pub fn default_path() -> PathBuf {
        let base = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("WinBaobab").join("settings.json")
    }

    /// Missing or corrupt files yield the defaults; never panics. Each field is
    /// parsed independently so one bad value only resets that field, and
    /// unparseable `recent` entries are dropped.
    pub fn load_from(path: &Path) -> Settings {
        let mut s = Settings::default();
        let too_big = std::fs::metadata(path).is_ok_and(|m| m.len() > MAX_FILE_BYTES);
        if too_big {
            return s;
        }
        let Ok(bytes) = std::fs::read(path) else {
            return s;
        };
        let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
        let Ok(serde_json::Value::Object(map)) = serde_json::from_slice(bytes) else {
            return s;
        };
        if let Some(v) = map
            .get("warn_permanent_delete")
            .and_then(serde_json::Value::as_bool)
        {
            s.warn_permanent_delete = v;
        }
        if let Some(v) = map
            .get("default_chart")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
        {
            s.default_chart = v;
        }
        if let Some(serde_json::Value::Array(items)) = map.get("recent") {
            s.recent = items
                .iter()
                .filter_map(|v| serde_json::from_value::<RecentScan>(v.clone()).ok())
                .take(MAX_RECENT)
                .collect();
        }
        s
    }

    /// Creates the parent dir, writes `<path>.<pid>.tmp`, then renames over the target.
    pub fn save_to(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        // Serde cannot serialize a PathBuf that isn't valid Unicode (e.g. unpaired
        // surrogates), and one such entry would make the whole save fail, so skip them.
        let mut clean = self.clone();
        clean.recent.retain(|r| r.path.to_str().is_some());
        let json = serde_json::to_string_pretty(&clean).map_err(io::Error::other)?;
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(format!(".{}.tmp", std::process::id()));
        let tmp = PathBuf::from(tmp);
        let result = std::fs::write(&tmp, json).and_then(|()| std::fs::rename(&tmp, path));
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }
    /// Newest first, case-insensitive dedupe, at most 10 entries.
    pub fn add_recent(&mut self, path: &Path, now_unix: u64) {
        let key = recent_key(path);
        self.recent.retain(|r| recent_key(&r.path) != key);
        self.recent.insert(
            0,
            RecentScan {
                path: path.to_path_buf(),
                when_unix: now_unix,
            },
        );
        self.recent.truncate(MAX_RECENT);
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        let s = Settings::default();
        assert!(s.warn_permanent_delete);
        assert_eq!(s.default_chart, Chart::Rings);
        assert!(s.recent.is_empty());
    }

    #[test]
    fn roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub").join("settings.json");
        let mut s = Settings {
            warn_permanent_delete: false,
            default_chart: Chart::Treemap,
            ..Settings::default()
        };
        s.add_recent(Path::new(r"C:\Data"), 42);
        s.save_to(&p).unwrap();
        assert_eq!(Settings::load_from(&p), s);
        // Saving again over an existing file works and leaves no temp file.
        s.save_to(&p).unwrap();
        let leftovers = std::fs::read_dir(p.parent().unwrap())
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".tmp")
            })
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn corrupt_file_gives_default() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        std::fs::write(&p, "{ not json").unwrap();
        assert_eq!(Settings::load_from(&p), Settings::default());
        assert_eq!(
            Settings::load_from(&dir.path().join("missing.json")),
            Settings::default()
        );
    }

    #[test]
    fn missing_field_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        std::fs::write(&p, "{}").unwrap();
        let s = Settings::load_from(&p);
        assert!(s.warn_permanent_delete);
        assert_eq!(s.default_chart, Chart::Rings);
    }

    #[test]
    fn recent_dedupes_case_insensitively_and_caps_at_10() {
        let mut s = Settings::default();
        s.add_recent(Path::new(r"C:\Foo"), 1);
        s.add_recent(Path::new(r"D:\Bar"), 2);
        s.add_recent(Path::new(r"c:\foo\"), 3);
        assert_eq!(s.recent.len(), 2);
        assert_eq!(s.recent[0].path, PathBuf::from(r"c:\foo\"));
        assert_eq!(s.recent[0].when_unix, 3);
        assert_eq!(s.recent[1].path, PathBuf::from(r"D:\Bar"));

        for i in 0..15u64 {
            s.add_recent(Path::new(&format!(r"C:\dir{i}")), 10 + i);
        }
        assert_eq!(s.recent.len(), 10);
        assert_eq!(s.recent[0].path, PathBuf::from(r"C:\dir14"));
        assert_eq!(s.recent[9].path, PathBuf::from(r"C:\dir5"));
    }

    #[test]
    fn unknown_chart_keeps_other_fields() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        std::fs::write(
            &p,
            r#"{"warn_permanent_delete":false,"default_chart":"Sunburst"}"#,
        )
        .unwrap();
        let s = Settings::load_from(&p);
        assert!(!s.warn_permanent_delete);
        assert_eq!(s.default_chart, Chart::Rings);
    }

    #[test]
    fn malformed_recent_entry_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        std::fs::write(
            &p,
            r#"{"warn_permanent_delete":false,"recent":[{"path":"C:\\A","when_unix":1},{"path":5},"junk",{"path":"C:\\B","when_unix":2}]}"#,
        )
        .unwrap();
        let s = Settings::load_from(&p);
        assert!(!s.warn_permanent_delete);
        assert_eq!(s.recent.len(), 2);
        assert_eq!(s.recent[0].path, PathBuf::from(r"C:\A"));
        assert_eq!(s.recent[1].path, PathBuf::from(r"C:\B"));
    }

    #[test]
    fn bom_is_stripped() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        std::fs::write(&p, b"\xEF\xBB\xBF{\"warn_permanent_delete\":false}").unwrap();
        assert!(!Settings::load_from(&p).warn_permanent_delete);
    }

    #[test]
    fn oversized_file_gives_default() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        let pad = " ".repeat(1024 * 1024 + 1);
        std::fs::write(&p, format!("{{\"warn_permanent_delete\":false}}{pad}")).unwrap();
        assert!(Settings::load_from(&p).warn_permanent_delete);
    }

    #[test]
    fn load_caps_recent_at_10() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        let items: Vec<String> = (0..15)
            .map(|i| format!(r#"{{"path":"C:\\d{i}","when_unix":{i}}}"#))
            .collect();
        std::fs::write(&p, format!("{{\"recent\":[{}]}}", items.join(","))).unwrap();
        assert_eq!(Settings::load_from(&p).recent.len(), 10);
    }

    #[test]
    fn save_skips_non_unicode_recent_paths() {
        use std::os::windows::ffi::OsStringExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        let bad = PathBuf::from(std::ffi::OsString::from_wide(&[0x43, 0x3A, 0x5C, 0xD800]));
        let mut s = Settings::default();
        s.recent.push(RecentScan {
            path: bad,
            when_unix: 1,
        });
        s.add_recent(Path::new(r"C:\ok"), 2);
        s.save_to(&p).unwrap();
        let loaded = Settings::load_from(&p);
        assert_eq!(loaded.recent.len(), 1);
        assert_eq!(loaded.recent[0].path, PathBuf::from(r"C:\ok"));
    }
}
