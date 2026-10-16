//! Pure layout and colour code for the ring (sunburst) chart.

pub mod color;
pub mod rings;
pub mod treemap;

pub use color::{PALETTE, UNSEEN, branch_color, node_color};
pub use rings::{
    MAX_DEPTH, MIN_SWEEP, RingGeometry, RingHit, RingSegment, ring_geometry, ring_hit,
    ring_segments,
};
pub use treemap::{LABEL_H, MIN_PX, PAD, TreemapBlock, squarify, treemap_blocks, treemap_hit};

use crate::model::{NodeId, Tree};

/// The leading part of `Tree::children_by_size(parent)` that satisfies `keep`
/// (called with a child's size and the sum of all live children's sizes),
/// in the same order, plus the summed size of every other live child.
/// `keep` must be monotone in size (larger sizes keep whenever smaller ones
/// do). Only the kept children are sorted, so a folder with 100k tiny
/// entries costs one linear pass instead of a full sort.
pub(crate) fn top_children(
    tree: &Tree,
    parent: NodeId,
    keep: impl Fn(u64, u64) -> bool,
) -> (Vec<NodeId>, u64) {
    let live: Vec<NodeId> = tree
        .node(parent)
        .children
        .iter()
        .copied()
        .filter(|&c| !tree.node(c).removed)
        .collect();
    let total: u64 = live.iter().map(|&c| tree.node(c).size).sum();
    let (mut kept, mut rest) = (Vec::new(), 0u64);
    for c in live {
        let size = tree.node(c).size;
        if size > 0 && keep(size, total) {
            kept.push(c);
        } else {
            rest += size;
        }
    }
    kept.sort_by_cached_key(|&c| {
        let n = tree.node(c);
        (std::cmp::Reverse(n.size), n.name.to_lowercase())
    });
    (kept, rest)
}
