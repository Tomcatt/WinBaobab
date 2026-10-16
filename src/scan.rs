//! Background parallel scanner that fills a shared [`Tree`].

use std::collections::HashSet;
use std::io;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::thread::JoinHandle;

use crate::model::{NodeId, NodeKind, Tree};
use crate::winfs::RawEntry;

/// Where directory listings come from (real filesystem or a test fake).
pub trait DirSource: Send + Sync {
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<RawEntry>>;
}

/// Reads directories with [`crate::winfs::read_dir_entries`].
pub struct WinDirSource;

impl DirSource for WinDirSource {
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<RawEntry>> {
        crate::winfs::read_dir_entries(dir)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ScanState {
    Running,
    Done,
    Cancelled,
    Failed(String),
}

#[derive(Default)]
pub struct Progress {
    pub items: AtomicU64,
    pub bytes: AtomicU64,
    pub current: Mutex<PathBuf>,
}

pub struct ScanHandle {
    pub tree: Arc<RwLock<Tree>>,
    pub progress: Arc<Progress>,
    cancel: Arc<AtomicBool>,
    state: Arc<Mutex<ScanState>>,
    _thread: JoinHandle<()>,
}

/// Scans `root` on a background thread, fanning out over the global rayon
/// pool. The returned handle's tree fills in as directories are read.
pub fn start_scan(
    root: PathBuf,
    source: Arc<dyn DirSource>,
    drive_used: Option<u64>,
) -> ScanHandle {
    start_scan_with_cancel(root, source, drive_used, Arc::new(AtomicBool::new(false)))
}

fn start_scan_with_cancel(
    root: PathBuf,
    source: Arc<dyn DirSource>,
    drive_used: Option<u64>,
    cancel: Arc<AtomicBool>,
) -> ScanHandle {
    let tree = Arc::new(RwLock::new(Tree::new(root.clone())));
    let progress = Arc::new(Progress::default());
    let state = Arc::new(Mutex::new(ScanState::Running));
    let thread = {
        let (tree, progress, cancel, state) = (
            tree.clone(),
            progress.clone(),
            cancel.clone(),
            state.clone(),
        );
        std::thread::spawn(move || {
            let skipped = AtomicBool::new(false);
            let ctx = ScanCtx {
                tree: &tree,
                source: &*source,
                seen: Mutex::new(HashSet::new()),
                progress: &progress,
                cancel: &cancel,
                skipped: &skipped,
            };
            let mut result = std::panic::catch_unwind(AssertUnwindSafe(|| ctx.run(&root)))
                .unwrap_or_else(|_| ScanState::Failed("scanner crashed".into()));
            // A panic under the write guard poisons the lock; the tree is
            // still structurally valid (each mutation is self-contained).
            tree.clear_poison();
            // A cancel that arrives after every task ran leaves a complete scan.
            if result == ScanState::Done && skipped.load(Ordering::Relaxed) {
                result = ScanState::Cancelled;
            }
            if result == ScanState::Done
                && let Some(used) = drive_used
            {
                write_tree(&tree).set_unseen(used);
            }
            *lock(&progress.current) = PathBuf::new();
            *lock(&state) = result;
        })
    };
    ScanHandle {
        tree,
        progress,
        cancel,
        state,
        _thread: thread,
    }
}

impl ScanHandle {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn state(&self) -> ScanState {
        lock(&self.state).clone()
    }

    /// Read access that never fails: a poisoned lock is recovered.
    pub fn read_tree(&self) -> RwLockReadGuard<'_, Tree> {
        self.tree.read().unwrap_or_else(PoisonError::into_inner)
    }
}

fn write_tree(tree: &RwLock<Tree>) -> RwLockWriteGuard<'_, Tree> {
    tree.write().unwrap_or_else(PoisonError::into_inner)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Drop for ScanHandle {
    fn drop(&mut self) {
        self.cancel();
    }
}

struct ScanCtx<'a> {
    tree: &'a RwLock<Tree>,
    source: &'a dyn DirSource,
    seen: Mutex<HashSet<u128>>,
    progress: &'a Progress,
    cancel: &'a AtomicBool,
    /// Set when a directory was skipped because of the cancel flag.
    skipped: &'a AtomicBool,
}

impl ScanCtx<'_> {
    /// Reads the root synchronously (so its failure is reportable), then the
    /// rest of the tree in a rayon scope. Returns Done, Cancelled or Failed.
    fn run(&self, root: &Path) -> ScanState {
        if self.cancel.load(Ordering::Relaxed) {
            return ScanState::Cancelled;
        }
        *lock(&self.progress.current) = root.to_path_buf();
        let entries = match self.source.read_dir(root) {
            Ok(e) => e,
            Err(e) => return ScanState::Failed(format!("{}: {e}", root.display())),
        };
        let subdirs = self.insert(Tree::ROOT, root, entries);
        rayon::scope(|s| {
            for (id, path) in subdirs {
                s.spawn(move |s| self.visit(s, id, path));
            }
        });
        ScanState::Done
    }

    fn visit<'s>(&'s self, s: &rayon::Scope<'s>, id: NodeId, path: PathBuf) {
        if self.cancel.load(Ordering::Relaxed) {
            self.skipped.store(true, Ordering::Relaxed);
            return;
        }
        *lock(&self.progress.current) = path.clone();
        match self.source.read_dir(&path) {
            Ok(entries) => {
                for (cid, cpath) in self.insert(id, &path, entries) {
                    s.spawn(move |s| self.visit(s, cid, cpath));
                }
            }
            Err(_) => write_tree(self.tree).mark_unreadable(id),
        }
    }

    /// Classifies outside the tree lock, then inserts under one write lock.
    fn insert(&self, parent: NodeId, dir: &Path, entries: Vec<RawEntry>) -> Vec<(NodeId, PathBuf)> {
        let planned = classify(entries, &self.seen);
        let mut tree = write_tree(self.tree);
        insert_planned(&mut tree, parent, dir, planned, self.progress)
    }
}

struct Planned {
    entry: RawEntry,
    kind: NodeKind,
    size: u64,
}

/// Whether a file id can be trusted to identify one file for hard-link
/// dedupe. 0 means "no id" and all-ones is `FILE_INVALID_FILE_ID` (ReFS and
/// some redirectors report it when the id does not fit in 64 bits).
pub(crate) fn dedupe_id(file_id: u128) -> Option<u128> {
    (file_id != 0 && file_id != u64::MAX as u128 && file_id != u128::MAX).then_some(file_id)
}

/// Decides each entry's kind and counted size. Reparse points become links
/// (never descended); a valid file id seen before counts as 0 bytes. Files
/// without a valid id are counted every time.
fn classify(entries: Vec<RawEntry>, seen: &Mutex<HashSet<u128>>) -> Vec<Planned> {
    let mut seen = lock(seen);
    entries
        .into_iter()
        .map(|entry| {
            let (kind, size) = if entry.is_reparse {
                (NodeKind::Link, 0)
            } else if entry.is_dir {
                (NodeKind::Dir, 0)
            } else if dedupe_id(entry.file_id).is_some_and(|id| !seen.insert(id)) {
                (NodeKind::File, 0)
            } else {
                (NodeKind::File, entry.alloc_size)
            };
            Planned { entry, kind, size }
        })
        .collect()
}

/// Adds `planned` under `parent`; returns the child directories to descend.
fn insert_planned(
    tree: &mut Tree,
    parent: NodeId,
    dir: &Path,
    planned: Vec<Planned>,
    progress: &Progress,
) -> Vec<(NodeId, PathBuf)> {
    let mut subdirs = Vec::new();
    let mut bytes = 0;
    for p in &planned {
        let id = tree.add_child(
            parent,
            &p.entry.name,
            p.kind,
            p.size,
            Some(p.entry.modified),
        );
        bytes += p.size;
        if p.kind == NodeKind::Dir {
            subdirs.push((id, dir.join(&p.entry.name)));
        }
    }
    progress
        .items
        .fetch_add(planned.len() as u64, Ordering::Relaxed);
    progress.bytes.fetch_add(bytes, Ordering::Relaxed);
    subdirs
}

/// Blocking: clears `id`'s children and re-reads its subtree from `source`
/// with a fresh hardlink set. Unreadable directories are marked as such.
/// No-op unless `id` is a live directory (links and files are never read).
pub fn rescan_subtree(tree: &mut Tree, id: NodeId, source: &dyn DirSource) {
    let n = tree.node(id);
    if n.kind != NodeKind::Dir || n.removed {
        return;
    }
    tree.clear_children(id);
    let seen = Mutex::new(HashSet::new());
    let progress = Progress::default();
    let mut stack = vec![(id, tree.path_of(id))];
    while let Some((id, path)) = stack.pop() {
        match source.read_dir(&path) {
            Ok(entries) => {
                let planned = classify(entries, &seen);
                stack.extend(insert_planned(tree, id, &path, planned, &progress));
            }
            Err(_) => tree.mark_unreadable(id),
        }
    }
}

/// Enumerates `dir` into a detached tree rooted at `dir`. Takes no lock, so
/// it can run for as long as the disk needs without blocking anyone.
pub fn scan_detached(dir: PathBuf, source: &dyn DirSource) -> Tree {
    let mut t = Tree::new(dir);
    rescan_subtree(&mut t, Tree::ROOT, source);
    t
}

/// Replaces the children of `id` in the live tree with the contents of the
/// detached tree `fresh` (made by [`scan_detached`] for `id`'s path). The write
/// lock is held per chunk of at most `chunk` nodes, never across the whole
/// job. Returns false if `id` stopped being a live directory part-way.
pub fn graft_children(live: &RwLock<Tree>, id: NodeId, fresh: &Tree, chunk: usize) -> bool {
    {
        let mut t = write_tree(live);
        if !t.is_live(id) || t.node(id).kind != NodeKind::Dir {
            return false;
        }
        t.clear_children(id);
        if fresh.node(Tree::ROOT).unreadable {
            t.mark_unreadable(id);
        }
    }
    // Breadth first, so every node's parent is inserted before it.
    let mut order = Vec::new();
    let mut queue = std::collections::VecDeque::from([Tree::ROOT]);
    while let Some(n) = queue.pop_front() {
        for &c in &fresh.node(n).children {
            order.push(c);
            queue.push_back(c);
        }
    }
    let mut map: std::collections::HashMap<NodeId, NodeId> =
        std::collections::HashMap::from([(Tree::ROOT, id)]);
    for part in order.chunks(chunk.max(1)) {
        let mut t = write_tree(live);
        if !t.is_live(id) {
            return false;
        }
        for &f in part {
            let n = fresh.node(f);
            let parent = map[&n.parent.expect("non-root node has a parent")];
            let own = if n.kind == NodeKind::Dir { 0 } else { n.size };
            let nid = t.add_child(parent, &n.name, n.kind, own, n.modified);
            if n.unreadable {
                t.mark_unreadable(nid);
            }
            map.insert(f, nid);
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::{Duration, Instant, SystemTime};

    const R: &str = r"C:\r";

    fn p(rel: &str) -> PathBuf {
        if rel.is_empty() {
            PathBuf::from(R)
        } else {
            Path::new(R).join(rel)
        }
    }

    fn entry(name: &str, is_dir: bool, is_reparse: bool, size: u64, id: u128) -> RawEntry {
        RawEntry {
            name: name.into(),
            is_dir,
            is_reparse,
            alloc_size: size,
            file_id: id,
            modified: SystemTime::UNIX_EPOCH,
        }
    }
    fn file(name: &str, size: u64, id: u128) -> RawEntry {
        entry(name, false, false, size, id)
    }
    fn dir(name: &str) -> RawEntry {
        entry(name, true, false, 0, 0)
    }

    /// Maps paths to listings or errors; unknown paths are NotFound.
    #[derive(Default)]
    struct FakeSource {
        dirs: HashMap<PathBuf, Result<Vec<RawEntry>, io::ErrorKind>>,
        delay: Duration,
        calls: Mutex<Vec<PathBuf>>,
        /// Reading this path panics.
        panic_on: Option<PathBuf>,
        /// Set to true during every read (simulates a cancel racing the scan).
        flag_on_read: Option<Arc<AtomicBool>>,
    }

    impl FakeSource {
        fn with(mut self, rel: &str, entries: Vec<RawEntry>) -> Self {
            self.dirs.insert(p(rel), Ok(entries));
            self
        }
        fn failing(mut self, rel: &str, kind: io::ErrorKind) -> Self {
            self.dirs.insert(p(rel), Err(kind));
            self
        }
    }

    impl DirSource for FakeSource {
        fn read_dir(&self, dir: &Path) -> io::Result<Vec<RawEntry>> {
            self.calls.lock().unwrap().push(dir.to_path_buf());
            std::thread::sleep(self.delay);
            if let Some(f) = &self.flag_on_read {
                f.store(true, Ordering::Relaxed);
            }
            if self.panic_on.as_deref() == Some(dir) {
                panic!("fake source panic");
            }
            match self.dirs.get(dir) {
                Some(Ok(v)) => Ok(v.clone()),
                Some(Err(k)) => Err(io::Error::from(*k)),
                None => Err(io::Error::from(io::ErrorKind::NotFound)),
            }
        }
    }

    fn wait(h: &ScanHandle) -> ScanState {
        let start = Instant::now();
        loop {
            let s = h.state();
            if s != ScanState::Running {
                return s;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "scan timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn scan(src: FakeSource, drive_used: Option<u64>) -> (ScanHandle, Arc<FakeSource>) {
        let src = Arc::new(src);
        let h = start_scan(p(""), src.clone(), drive_used);
        (h, src)
    }

    fn child(t: &Tree, parent: NodeId, name: &str) -> NodeId {
        t.node(parent)
            .children
            .iter()
            .copied()
            .find(|&c| &*t.node(c).name == name)
            .unwrap_or_else(|| panic!("missing child {name}"))
    }

    fn sample_source() -> FakeSource {
        FakeSource::default()
            .with("", vec![file("a.txt", 100, 1), dir("sub")])
            .with("sub", vec![file("b.txt", 200, 2), dir("deep")])
            .with(r"sub\deep", vec![file("c.txt", 300, 3)])
    }

    /// d0\d1\...\d{n-1}: a chain, so the scan is strictly sequential.
    fn chain_source(n: usize, delay: Duration) -> FakeSource {
        let mut src = FakeSource {
            delay,
            ..Default::default()
        };
        let mut rel = PathBuf::new();
        for i in 0..n {
            src = src.with(rel.to_str().unwrap(), vec![dir(&format!("d{i}"))]);
            rel.push(format!("d{i}"));
        }
        src.with(rel.to_str().unwrap(), vec![])
    }

    #[test]
    fn scans_fake_tree_totals() {
        let (h, _) = scan(sample_source(), None);
        assert_eq!(wait(&h), ScanState::Done);
        let t = h.tree.read().unwrap();
        assert_eq!(t.node(Tree::ROOT).size, 600);
        assert_eq!(t.node(Tree::ROOT).items, 5);
        let sub = child(&t, Tree::ROOT, "sub");
        assert_eq!(t.node(sub).kind, NodeKind::Dir);
        assert_eq!(t.node(sub).size, 500);
        let deep = child(&t, sub, "deep");
        assert_eq!(t.node(child(&t, deep, "c.txt")).size, 300);
        assert_eq!(h.progress.items.load(Ordering::Relaxed), 5);
        assert_eq!(h.progress.bytes.load(Ordering::Relaxed), 600);
    }

    #[test]
    fn does_not_follow_reparse() {
        let src = FakeSource::default()
            .with("", vec![entry("j", true, true, 0, 0), file("f", 10, 1)])
            .with("j", vec![file("huge", 1 << 40, 2)]);
        let (h, src) = scan(src, None);
        assert_eq!(wait(&h), ScanState::Done);
        let t = h.tree.read().unwrap();
        let j = child(&t, Tree::ROOT, "j");
        assert_eq!(t.node(j).kind, NodeKind::Link);
        assert!(t.node(j).children.is_empty());
        assert_eq!(t.node(Tree::ROOT).size, 10);
        assert_eq!(*src.calls.lock().unwrap(), vec![p("")]);
    }

    #[test]
    fn hardlink_counted_once() {
        let src = FakeSource::default()
            .with("", vec![file("a", 500, 7), dir("s")])
            .with("s", vec![file("b", 500, 7)]);
        let (h, _) = scan(src, None);
        assert_eq!(wait(&h), ScanState::Done);
        let t = h.tree.read().unwrap();
        assert_eq!(t.node(Tree::ROOT).size, 500);
        assert_eq!(t.node(Tree::ROOT).items, 3);
    }

    #[test]
    fn zero_file_ids_are_not_deduplicated() {
        let src = FakeSource::default().with("", vec![file("a", 5, 0), file("b", 5, 0)]);
        let (h, _) = scan(src, None);
        assert_eq!(wait(&h), ScanState::Done);
        assert_eq!(h.tree.read().unwrap().node(Tree::ROOT).size, 10);
    }

    #[test]
    fn invalid_file_ids_are_not_deduplicated() {
        let bad = u64::MAX as u128;
        let src = FakeSource::default()
            .with("", vec![file("a", 5, bad), file("b", 7, bad), dir("s")])
            .with("s", vec![file("c", 11, bad), file("d", 13, u128::MAX)]);
        let (h, _) = scan(src, None);
        assert_eq!(wait(&h), ScanState::Done);
        assert_eq!(h.tree.read().unwrap().node(Tree::ROOT).size, 36);
        assert_eq!(dedupe_id(0), None);
        assert_eq!(dedupe_id(bad), None);
        assert_eq!(dedupe_id(u128::MAX), None);
        assert_eq!(dedupe_id(42), Some(42));
    }

    #[test]
    fn subdir_error_marks_unreadable() {
        let src = FakeSource::default()
            .with("", vec![dir("bad"), file("ok", 1, 1)])
            .failing("bad", io::ErrorKind::PermissionDenied);
        let (h, _) = scan(src, None);
        assert_eq!(wait(&h), ScanState::Done);
        let t = h.tree.read().unwrap();
        assert_eq!(t.unreadable_dirs(), vec![child(&t, Tree::ROOT, "bad")]);
    }

    #[test]
    fn root_unreadable_fails_cleanly() {
        let (h, _) = scan(FakeSource::default(), Some(10_000));
        assert!(matches!(wait(&h), ScanState::Failed(_)));
        let t = h.tree.read().unwrap();
        assert!(t.node(Tree::ROOT).children.is_empty());
        assert_eq!(t.node(Tree::ROOT).size, 0);
    }

    #[test]
    fn cancel_stops_scan() {
        let (h, _) = scan(chain_source(100, Duration::from_millis(50)), None);
        let start = Instant::now();
        std::thread::sleep(Duration::from_millis(100));
        h.cancel();
        assert_eq!(wait(&h), ScanState::Cancelled);
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(h.progress.items.load(Ordering::Relaxed) < 100);
    }

    fn unseen_of(t: &Tree) -> Vec<NodeId> {
        t.node(Tree::ROOT)
            .children
            .iter()
            .copied()
            .filter(|&c| t.node(c).kind == NodeKind::Unseen)
            .collect()
    }

    #[test]
    fn unseen_added_only_when_done() {
        let (h, _) = scan(sample_source(), Some(10_000));
        assert_eq!(wait(&h), ScanState::Done);
        {
            let t = h.tree.read().unwrap();
            let u = unseen_of(&t);
            assert_eq!(u.len(), 1);
            assert_eq!(t.node(u[0]).size, 9_400);
            assert_eq!(t.node(Tree::ROOT).size, 10_000);
        }
        let (h, _) = scan(chain_source(100, Duration::from_millis(50)), Some(10_000));
        std::thread::sleep(Duration::from_millis(100));
        h.cancel();
        assert_eq!(wait(&h), ScanState::Cancelled);
        assert!(unseen_of(&h.tree.read().unwrap()).is_empty());
    }

    #[test]
    fn rescan_subtree_replaces_children() {
        let (h, _) = scan(sample_source(), None);
        assert_eq!(wait(&h), ScanState::Done);
        let mut t = h.tree.write().unwrap();
        let sub = child(&t, Tree::ROOT, "sub");
        let fresh = FakeSource::default()
            .with("sub", vec![file("n1", 40, 2), dir("nd")])
            .with(r"sub\nd", vec![file("n2", 60, 9)]);
        rescan_subtree(&mut t, sub, &fresh);
        // file id 2 was seen by the original scan; the rescan uses a fresh set.
        assert_eq!(t.node(sub).size, 100);
        assert_eq!(t.node(sub).items, 3);
        assert_eq!(t.node(Tree::ROOT).size, 200);
        assert_eq!(t.node(Tree::ROOT).items, 5);
        let names: Vec<&str> = t
            .children_by_size(sub)
            .iter()
            .map(|&c| &*t.node(c).name)
            .collect();
        assert_eq!(names, vec!["nd", "n1"]);

        rescan_subtree(&mut t, sub, &FakeSource::default());
        assert!(t.node(sub).unreadable);
        assert_eq!(t.node(sub).size, 0);
        assert_eq!(t.node(Tree::ROOT).size, 100);
    }

    #[test]
    fn panicking_source_fails_and_tree_stays_readable() {
        let mut src = FakeSource::default()
            .with("", vec![dir("boom"), dir("ok"), file("f", 10, 1)])
            .with("ok", vec![file("g", 20, 2)]);
        src.panic_on = Some(p("boom"));
        let (h, _) = scan(src, Some(10_000));
        assert!(matches!(wait(&h), ScanState::Failed(_)));
        let t = h.read_tree();
        assert!(t.node(Tree::ROOT).size >= 10);
        assert!(unseen_of(&t).is_empty());
        drop(t);
        assert!(!h.tree.is_poisoned());
        assert_eq!(*h.progress.current.lock().unwrap(), PathBuf::new());
    }

    #[test]
    fn poisoned_tree_lock_is_recovered_for_readers() {
        let (h, _) = scan(sample_source(), None);
        assert_eq!(wait(&h), ScanState::Done);
        let tree = h.tree.clone();
        let _ = std::thread::spawn(move || {
            let _g = tree.write().unwrap();
            panic!("poison the tree lock");
        })
        .join();
        assert!(h.tree.is_poisoned());
        assert_eq!(h.read_tree().node(Tree::ROOT).size, 600);
    }

    #[test]
    fn cancel_after_all_work_finished_stays_done() {
        let flag = Arc::new(AtomicBool::new(false));
        let src = FakeSource {
            flag_on_read: Some(flag.clone()),
            ..Default::default()
        }
        .with("", vec![file("a", 100, 1)]);
        // The cancel flag is raised during the only read, but nothing is
        // skipped, so the scan is complete.
        let h = start_scan_with_cancel(p(""), Arc::new(src), Some(1_000), flag);
        assert_eq!(wait(&h), ScanState::Done);
        assert_eq!(unseen_of(&h.read_tree()).len(), 1);
    }

    #[test]
    fn progress_current_cleared_when_finished() {
        let (h, _) = scan(sample_source(), None);
        assert_eq!(wait(&h), ScanState::Done);
        assert_eq!(*h.progress.current.lock().unwrap(), PathBuf::new());
        let (h, _) = scan(FakeSource::default(), None);
        assert!(matches!(wait(&h), ScanState::Failed(_)));
        assert_eq!(*h.progress.current.lock().unwrap(), PathBuf::new());
    }

    #[test]
    fn reparse_file_is_zero_size_link() {
        let src = FakeSource::default().with(
            "",
            vec![entry("lnk", false, true, 4096, 5), file("f", 10, 1)],
        );
        let (h, _) = scan(src, None);
        assert_eq!(wait(&h), ScanState::Done);
        let t = h.read_tree();
        let l = child(&t, Tree::ROOT, "lnk");
        assert_eq!(t.node(l).kind, NodeKind::Link);
        assert_eq!(t.node(l).size, 0);
        assert_eq!(t.node(Tree::ROOT).size, 10);
    }

    #[test]
    fn graft_in_small_chunks_matches_a_blocking_rescan() {
        let (h, _) = scan(sample_source(), None);
        assert_eq!(wait(&h), ScanState::Done);
        let sub = child(&h.read_tree(), Tree::ROOT, "sub");
        let old_b = child(&h.read_tree(), sub, "b.txt");
        let fresh_src = FakeSource::default()
            .with("sub", vec![file("n1", 40, 2), dir("nd"), dir("bad")])
            .with(r"sub\nd", vec![file("n2", 60, 9)])
            .failing(r"sub\bad", io::ErrorKind::PermissionDenied);
        let detached = scan_detached(p("sub"), &fresh_src);
        assert!(graft_children(&h.tree, sub, &detached, 2));
        let t = h.read_tree();
        assert!(!t.is_live(old_b));
        assert_eq!(t.node(sub).size, 100);
        assert_eq!(t.node(sub).items, 4);
        assert_eq!(t.node(Tree::ROOT).size, 200);
        assert_eq!(t.unreadable_dirs(), vec![child(&t, sub, "bad")]);
        let nd = child(&t, sub, "nd");
        assert_eq!(t.node(child(&t, nd, "n2")).size, 60);
    }

    #[test]
    fn graft_into_a_removed_node_changes_nothing() {
        let (h, _) = scan(sample_source(), None);
        assert_eq!(wait(&h), ScanState::Done);
        let sub = child(&h.read_tree(), Tree::ROOT, "sub");
        h.tree.write().unwrap().remove_subtree(sub);
        let detached = scan_detached(p("sub"), &sample_source());
        assert!(!graft_children(&h.tree, sub, &detached, 10));
        assert_eq!(h.read_tree().node(Tree::ROOT).size, 100);
    }

    #[test]
    fn rescan_of_link_or_file_does_not_read() {
        let src =
            FakeSource::default().with("", vec![entry("j", true, true, 0, 0), file("f", 10, 1)]);
        let (h, _) = scan(src, None);
        assert_eq!(wait(&h), ScanState::Done);
        let mut t = h.tree.write().unwrap();
        let fresh = FakeSource::default().with("j", vec![file("x", 1, 9)]);
        for name in ["j", "f"] {
            let id = child(&t, Tree::ROOT, name);
            rescan_subtree(&mut t, id, &fresh);
        }
        assert!(fresh.calls.lock().unwrap().is_empty());
        assert_eq!(t.node(Tree::ROOT).size, 10);
        assert_eq!(t.node(Tree::ROOT).items, 2);
    }
}
