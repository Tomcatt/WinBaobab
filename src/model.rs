//! Arena-based directory tree with aggregated sizes and item counts.

use std::path::PathBuf;
use std::time::SystemTime;

pub type NodeId = u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeKind {
    Dir,
    File,
    Link,
    /// Synthetic node for space used on the drive but not found by the scan.
    Unseen,
}

#[derive(Clone, Debug)]
pub struct Node {
    pub name: Box<str>,
    pub parent: Option<NodeId>,
    pub children: Vec<NodeId>,
    pub kind: NodeKind,
    /// Aggregate size of this node and all descendants.
    pub size: u64,
    /// Number of descendants (Unseen nodes are not counted).
    pub items: u64,
    /// Latest modification time in this subtree.
    pub modified: Option<SystemTime>,
    pub unreadable: bool,
    pub removed: bool,
}

pub struct Tree {
    nodes: Vec<Node>,
    pub root_path: PathBuf,
    /// Bumped on every mutation so views can detect changes cheaply.
    pub generation: u64,
}

impl Tree {
    pub const ROOT: NodeId = 0;

    pub fn new(root_path: PathBuf) -> Tree {
        let name = root_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| root_path.to_string_lossy().into_owned());
        let root = Node {
            name: name.into_boxed_str(),
            parent: None,
            children: Vec::new(),
            kind: NodeKind::Dir,
            size: 0,
            items: 0,
            modified: None,
            unreadable: false,
            removed: false,
        };
        Tree {
            nodes: vec![root],
            root_path,
            generation: 0,
        }
    }

    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id as usize]
    }

    /// True when `id` exists and neither it nor any ancestor has been removed.
    pub fn is_live(&self, id: NodeId) -> bool {
        if id as usize >= self.nodes.len() {
            return false;
        }
        let mut cur = Some(id);
        while let Some(c) = cur {
            let n = self.node(c);
            if n.removed {
                return false;
            }
            cur = n.parent;
        }
        true
    }

    fn node_mut(&mut self, id: NodeId) -> &mut Node {
        &mut self.nodes[id as usize]
    }

    pub fn add_child(
        &mut self,
        parent: NodeId,
        name: &str,
        kind: NodeKind,
        own_size: u64,
        modified: Option<SystemTime>,
    ) -> NodeId {
        let own_size = if kind == NodeKind::Link { 0 } else { own_size };
        let id = self.push_node(parent, name, kind, own_size, modified);
        let mut cur = Some(parent);
        while let Some(a) = cur {
            let n = self.node_mut(a);
            n.size += own_size;
            n.items += 1;
            n.modified = n.modified.max(modified);
            cur = n.parent;
        }
        self.generation += 1;
        id
    }

    fn push_node(
        &mut self,
        parent: NodeId,
        name: &str,
        kind: NodeKind,
        size: u64,
        modified: Option<SystemTime>,
    ) -> NodeId {
        let id = self.nodes.len() as NodeId;
        self.nodes.push(Node {
            name: name.into(),
            parent: Some(parent),
            children: Vec::new(),
            kind,
            size,
            items: 0,
            modified,
            unreadable: false,
            removed: false,
        });
        self.node_mut(parent).children.push(id);
        id
    }

    pub fn mark_unreadable(&mut self, id: NodeId) {
        self.node_mut(id).unreadable = true;
        self.generation += 1;
    }

    /// Reachable, non-removed directories that could not be read.
    pub fn unreadable_dirs(&self) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut stack = vec![Self::ROOT];
        while let Some(id) = stack.pop() {
            let n = self.node(id);
            if n.unreadable && n.kind == NodeKind::Dir {
                out.push(id);
            }
            stack.extend(n.children.iter().copied());
        }
        out.sort_unstable();
        out
    }

    pub fn path_of(&self, id: NodeId) -> PathBuf {
        let mut names = Vec::new();
        let mut cur = id;
        while let Some(p) = self.node(cur).parent {
            names.push(&*self.node(cur).name);
            cur = p;
        }
        let mut path = self.root_path.clone();
        for name in names.iter().rev() {
            path.push(name);
        }
        path
    }

    /// Non-removed children, largest first; ties by name (case-insensitive).
    pub fn children_by_size(&self, id: NodeId) -> Vec<NodeId> {
        let mut v: Vec<NodeId> = self
            .node(id)
            .children
            .iter()
            .copied()
            .filter(|&c| !self.node(c).removed)
            .collect();
        v.sort_by_cached_key(|&c| {
            let n = self.node(c);
            (std::cmp::Reverse(n.size), n.name.to_lowercase())
        });
        v
    }

    /// Detaches `id` and subtracts it from all ancestors. Returns bytes freed.
    /// No-op (returns 0) for the root or an already-removed node.
    pub fn remove_subtree(&mut self, id: NodeId) -> u64 {
        if id == Self::ROOT || self.node(id).removed {
            return 0;
        }
        let (size, items, kind, parent) = {
            let n = self.node(id);
            (
                n.size,
                n.items,
                n.kind,
                n.parent.expect("non-root has parent"),
            )
        };
        // Unseen nodes never contributed to `items`.
        let item_delta = if kind == NodeKind::Unseen {
            0
        } else {
            items + 1
        };
        self.node_mut(parent).children.retain(|&c| c != id);
        self.node_mut(id).removed = true;
        let mut cur = Some(parent);
        while let Some(a) = cur {
            let n = self.node_mut(a);
            n.size -= size;
            n.items -= item_delta;
            cur = n.parent;
        }
        self.generation += 1;
        size
    }

    /// Removes all children of `id` in one linear pass, leaving it with only
    /// its own size, no items, and readable (for rescans).
    pub fn clear_children(&mut self, id: NodeId) {
        let children = std::mem::take(&mut self.node_mut(id).children);
        let (mut size, mut items) = (0u64, 0u64);
        for c in children {
            let n = self.node_mut(c);
            n.removed = true;
            size += n.size;
            // Unseen nodes never contributed to `items`.
            items += if n.kind == NodeKind::Unseen {
                0
            } else {
                n.items + 1
            };
        }
        let mut cur = Some(id);
        while let Some(a) = cur {
            let n = self.node_mut(a);
            n.size -= size;
            n.items -= items;
            cur = n.parent;
        }
        self.node_mut(id).unreadable = false;
        self.generation += 1;
    }

    /// Adds/updates/removes the Unseen child of the root so the root's size
    /// equals `drive_used` (when the scan found less than that).
    pub fn set_unseen(&mut self, drive_used: u64) {
        let existing = self
            .node(Self::ROOT)
            .children
            .iter()
            .copied()
            .find(|&c| self.node(c).kind == NodeKind::Unseen);
        let prev = existing.map_or(0, |c| self.node(c).size);
        let scanned = self.node(Self::ROOT).size - prev;
        let unseen = drive_used.saturating_sub(scanned);
        match (existing, unseen) {
            (Some(c), 0) => {
                self.remove_subtree(c);
            }
            (Some(c), u) => {
                self.node_mut(c).size = u;
                self.node_mut(Self::ROOT).size = scanned + u;
            }
            (None, 0) => {}
            (None, u) => {
                self.push_node(Self::ROOT, "Unseen", NodeKind::Unseen, u, None);
                self.node_mut(Self::ROOT).size += u;
            }
        }
        self.generation += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn sample() -> (Tree, NodeId, NodeId, NodeId, NodeId) {
        let mut t = Tree::new(PathBuf::from(r"C:\root"));
        let a = t.add_child(Tree::ROOT, "a", NodeKind::Dir, 0, None);
        let f1 = t.add_child(a, "f1", NodeKind::File, 100, None);
        let f2 = t.add_child(a, "f2", NodeKind::File, 50, None);
        let f3 = t.add_child(Tree::ROOT, "f3", NodeKind::File, 10, None);
        (t, a, f1, f2, f3)
    }

    #[test]
    fn aggregates_sizes_and_items() {
        let (t, a, ..) = sample();
        assert_eq!(t.node(Tree::ROOT).size, 160);
        assert_eq!(t.node(Tree::ROOT).items, 4);
        assert_eq!(t.node(a).size, 150);
        assert_eq!(t.node(a).items, 2);
    }

    #[test]
    fn link_has_zero_size() {
        let mut t = Tree::new(PathBuf::from("r"));
        let l = t.add_child(Tree::ROOT, "l", NodeKind::Link, 99, None);
        assert_eq!(t.node(l).size, 0);
        assert_eq!(t.node(Tree::ROOT).size, 0);
        assert_eq!(t.node(Tree::ROOT).items, 1);
    }

    #[test]
    fn modified_is_max_of_subtree() {
        let base = SystemTime::UNIX_EPOCH;
        let t1 = base + Duration::from_secs(10);
        let t2 = base + Duration::from_secs(99);
        let mut t = Tree::new(PathBuf::from("r"));
        let a = t.add_child(Tree::ROOT, "a", NodeKind::Dir, 0, None);
        t.add_child(a, "x", NodeKind::File, 1, Some(t2));
        t.add_child(a, "y", NodeKind::File, 1, Some(t1));
        assert_eq!(t.node(a).modified, Some(t2));
        assert_eq!(t.node(Tree::ROOT).modified, Some(t2));
    }

    #[test]
    fn path_of_nested() {
        let (t, _, f1, ..) = sample();
        assert_eq!(
            t.path_of(f1),
            PathBuf::from(r"C:\root").join("a").join("f1")
        );
        assert_eq!(t.path_of(Tree::ROOT), PathBuf::from(r"C:\root"));
    }

    #[test]
    fn children_by_size_sorted() {
        let (t, a, _, _, f3) = sample();
        assert_eq!(t.children_by_size(Tree::ROOT), vec![a, f3]);
    }

    #[test]
    fn children_by_size_ties_by_name_case_insensitive() {
        let mut t = Tree::new(PathBuf::from("r"));
        let b = t.add_child(Tree::ROOT, "b", NodeKind::File, 5, None);
        let c = t.add_child(Tree::ROOT, "C", NodeKind::File, 5, None);
        let a = t.add_child(Tree::ROOT, "A", NodeKind::File, 5, None);
        assert_eq!(t.children_by_size(Tree::ROOT), vec![a, b, c]);
    }

    #[test]
    fn remove_subtree_updates_ancestors() {
        let (mut t, a, _, _, f3) = sample();
        let g = t.generation;
        assert_eq!(t.remove_subtree(a), 150);
        assert_eq!(t.node(Tree::ROOT).size, 10);
        assert_eq!(t.node(Tree::ROOT).items, 1);
        assert_eq!(t.children_by_size(Tree::ROOT), vec![f3]);
        assert!(t.node(a).removed);
        assert!(t.generation > g);
        // second removal and root removal are no-ops
        let g = t.generation;
        assert_eq!(t.remove_subtree(a), 0);
        assert_eq!(t.remove_subtree(Tree::ROOT), 0);
        assert_eq!(t.generation, g);
        assert_eq!(t.node(Tree::ROOT).size, 10);
    }

    #[test]
    fn clear_children_large_is_linear() {
        let mut t = Tree::new(PathBuf::from("r"));
        let d = t.add_child(Tree::ROOT, "d", NodeKind::Dir, 7, None);
        for i in 0..100_000 {
            t.add_child(d, &format!("f{i}"), NodeKind::File, 1, None);
        }
        assert_eq!(t.node(d).items, 100_000);
        let g = t.generation;
        t.clear_children(d);
        assert!(t.generation > g);
        assert_eq!(t.node(d).size, 7);
        assert_eq!(t.node(d).items, 0);
        assert_eq!(t.node(Tree::ROOT).size, 7);
        assert_eq!(t.node(Tree::ROOT).items, 1);
    }

    #[test]
    fn clear_children_resets_node() {
        let (mut t, a, ..) = sample();
        t.mark_unreadable(a);
        t.clear_children(a);
        assert_eq!(t.node(a).size, 0);
        assert_eq!(t.node(a).items, 0);
        assert!(!t.node(a).unreadable);
        assert!(t.node(a).children.is_empty());
        assert_eq!(t.node(Tree::ROOT).size, 10);
        assert_eq!(t.node(Tree::ROOT).items, 2);
    }

    #[test]
    fn unreadable_dirs_lists_reachable_only() {
        let (mut t, a, _, f2, _) = sample();
        let d = t.add_child(a, "d", NodeKind::Dir, 0, None);
        t.mark_unreadable(d);
        t.mark_unreadable(f2); // not a dir
        assert_eq!(t.unreadable_dirs(), vec![d]);
        t.remove_subtree(a);
        assert!(t.unreadable_dirs().is_empty());
    }

    #[test]
    fn is_live_checks_range_and_ancestors() {
        let (mut t, a, f1, _, f3) = sample();
        assert!(t.is_live(Tree::ROOT) && t.is_live(f1) && t.is_live(f3));
        assert!(!t.is_live(999));
        t.remove_subtree(a);
        assert!(!t.is_live(a));
        assert!(!t.is_live(f1)); // descendant of a removed node
        assert!(t.is_live(f3));
    }

    #[test]
    fn unseen_fills_gap() {
        let (mut t, ..) = sample();
        let unseen_of = |t: &Tree| {
            t.node(Tree::ROOT)
                .children
                .iter()
                .copied()
                .filter(|&c| t.node(c).kind == NodeKind::Unseen)
                .collect::<Vec<_>>()
        };
        t.set_unseen(1000);
        let u = unseen_of(&t);
        assert_eq!(u.len(), 1);
        assert_eq!(t.node(u[0]).size, 840);
        assert_eq!(t.node(Tree::ROOT).size, 1000);
        assert_eq!(t.node(Tree::ROOT).items, 4);
        t.set_unseen(1100);
        assert_eq!(unseen_of(&t), u);
        assert_eq!(t.node(u[0]).size, 940);
        assert_eq!(t.node(Tree::ROOT).size, 1100);
        t.set_unseen(100);
        assert!(unseen_of(&t).is_empty());
        assert_eq!(t.node(Tree::ROOT).size, 160);
        assert_eq!(t.node(Tree::ROOT).items, 4);
    }
}
