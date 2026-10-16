//! Squarified treemap layout (Bruls, Huizing, van Wijk) and hit testing.

use super::top_children;
use crate::model::{NodeId, NodeKind, Tree};
use eframe::egui::{Pos2, Rect, pos2};

/// Inset between a folder block and its children.
pub const PAD: f32 = 2.0;
/// Blocks smaller than this in either dimension are not emitted.
pub const MIN_PX: f32 = 3.0;
/// Header strip reserved for a folder label when the block is tall enough.
pub const LABEL_H: f32 = 16.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TreemapBlock {
    pub node: NodeId,
    pub depth: u8,
    pub rect: Rect,
    /// Index of the depth-1 ancestor in the focus's `children_by_size` order
    /// (the colour branch); inherited unchanged by descendants.
    pub branch: u32,
}

/// Squarified layout of `sizes` (descending) inside `area`; rect `i` belongs
/// to `sizes[i]`. Zero-size entries (trailing, given the ordering) get no
/// rect, so the output may be shorter than `sizes`; callers must pair by
/// index and stop at the shorter list. A zero or negative total or an empty
/// `area` yields an empty vec.
pub fn squarify(sizes: &[u64], area: Rect) -> Vec<Rect> {
    let (aw, ah) = (area.width() as f64, area.height() as f64);
    let total: f64 = sizes.iter().map(|&s| s as f64).sum();
    if !(aw > 0.0 && ah > 0.0 && total > 0.0) {
        return Vec::new();
    }
    let scale = aw * ah / total;
    let n = sizes.iter().take_while(|&&s| s > 0).count();
    let mut out = Vec::with_capacity(n);
    let (mut x, mut y, mut w, mut h) = (area.min.x as f64, area.min.y as f64, aw, ah);
    let mut i = 0;
    while i < n {
        let side = w.min(h);
        if side.is_nan() || side <= 0.0 {
            break; // remaining area collapsed; the rest get no rects
        }
        // Grow the row while the worst aspect ratio does not get worse
        // (running sum/min/max keep each step O(1)).
        let (mut sum, mut lo, mut hi) = (0.0, f64::MAX, 0.0f64);
        let mut j = i;
        let mut best = f64::MAX;
        while j < n {
            let a = sizes[j] as f64 * scale;
            let (nsum, nlo, nhi) = (sum + a, lo.min(a), hi.max(a));
            let worst = (side * side * nhi / (nsum * nsum)).max(nsum * nsum / (side * side * nlo));
            if j > i && worst > best {
                break;
            }
            best = worst;
            (sum, lo, hi) = (nsum, nlo, nhi);
            j += 1;
        }
        let thick = sum / side;
        if !(thick > 0.0 && thick.is_finite()) {
            break;
        }
        let mut off = 0.0;
        for &s in &sizes[i..j] {
            let len = s as f64 * scale / thick;
            let (rx, ry, rw, rh) = if w >= h {
                (x, y + off, thick, len) // column on the left
            } else {
                (x + off, y, len, thick) // row on the top
            };
            out.push(Rect::from_min_max(
                pos2(rx as f32, ry as f32),
                pos2((rx + rw) as f32, (ry + rh) as f32),
            ));
            off += len;
        }
        if w >= h {
            x += thick;
            w -= thick;
        } else {
            y += thick;
            h -= thick;
        }
        i = j;
    }
    out
}

/// Blocks for the children of `focus` (depth 1), nested up to `max_depth`.
/// Parents come before their children. Rects are paired with nodes by index
/// (`squarify` may return fewer rects than nodes); folders are only expanded
/// when their own block passes the `MIN_PX` test.
pub fn treemap_blocks(tree: &Tree, focus: NodeId, area: Rect, max_depth: u8) -> Vec<TreemapBlock> {
    let mut out = Vec::new();
    if max_depth > 0 {
        layout(tree, focus, area, 1, 0, max_depth, &mut out);
    }
    out
}

fn layout(
    tree: &Tree,
    parent: NodeId,
    area: Rect,
    depth: u8,
    branch: u32,
    max_depth: u8,
    out: &mut Vec<TreemapBlock>,
) {
    // A block smaller than MIN_PX x MIN_PX in area can never be drawn, so
    // only larger children are sorted; the rest is represented by one
    // trailing aggregate entry that keeps the area scale intact.
    let area_px = area.width() as f64 * area.height() as f64;
    let min_area = (MIN_PX * MIN_PX) as f64 * 0.99;
    let (kids, rest) = top_children(tree, parent, |size, total| {
        size as f64 * area_px / total as f64 >= min_area
    });
    let mut sizes: Vec<u64> = kids.iter().map(|&c| tree.node(c).size).collect();
    if rest > 0 {
        sizes.push(rest);
    }
    // zip stops at the shorter list: the aggregate has no node.
    for (i, (&c, rect)) in kids.iter().zip(squarify(&sizes, area)).enumerate() {
        if rect.width() < MIN_PX || rect.height() < MIN_PX {
            continue;
        }
        let child_branch = if depth == 1 { i as u32 } else { branch };
        out.push(TreemapBlock {
            node: c,
            depth,
            rect,
            branch: child_branch,
        });
        if depth < max_depth && tree.node(c).kind == NodeKind::Dir {
            let mut inner = rect.shrink(PAD);
            if inner.height() > 3.0 * LABEL_H {
                inner.min.y += LABEL_H;
            }
            layout(tree, c, inner, depth + 1, child_branch, max_depth, out);
        }
    }
}

/// The deepest block containing `p`: parents precede children, so the last
/// match wins.
pub fn treemap_hit(blocks: &[TreemapBlock], p: Pos2) -> Option<NodeId> {
    blocks
        .iter()
        .rev()
        .find(|b| b.rect.contains(p))
        .map(|b| b.node)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn area(w: f32, h: f32) -> Rect {
        Rect::from_min_max(pos2(0.0, 0.0), pos2(w, h))
    }

    #[test]
    fn squarify_areas_proportional() {
        let sizes = [6, 6, 4, 3, 2, 2, 1];
        let r = squarify(&sizes, area(6.0, 4.0));
        assert_eq!(r.len(), sizes.len());
        for (rect, &s) in r.iter().zip(&sizes) {
            let want = s as f32; // total 24 == area 24
            assert!((rect.area() - want).abs() <= want * 0.01, "{rect:?} {s}");
            assert!(rect.min.x >= -1e-4 && rect.min.y >= -1e-4);
            assert!(rect.max.x <= 6.0 + 1e-4 && rect.max.y <= 4.0 + 1e-4);
        }
        for i in 0..r.len() {
            for j in i + 1..r.len() {
                let o = r[i].intersect(r[j]);
                assert!(o.width() <= 1e-4 || o.height() <= 1e-4, "{i} overlaps {j}");
            }
        }
    }

    #[test]
    fn squarify_aspect_reasonable() {
        for rect in squarify(&[6, 6, 4, 3, 2, 2, 1], area(6.0, 4.0)) {
            let (w, h) = (rect.width(), rect.height());
            assert!(w.max(h) / w.min(h) <= 3.0, "{rect:?}");
        }
    }

    #[test]
    fn squarify_degenerate_inputs() {
        assert!(squarify(&[], area(10.0, 10.0)).is_empty());
        assert!(squarify(&[0, 0], area(10.0, 10.0)).is_empty());
        assert!(squarify(&[5], area(0.0, 10.0)).is_empty());
        assert_eq!(squarify(&[5, 0], area(10.0, 10.0)).len(), 1);
    }

    fn nested() -> (Tree, NodeId, NodeId) {
        let mut t = Tree::new(PathBuf::from("r"));
        let d = t.add_child(Tree::ROOT, "d", NodeKind::Dir, 0, None);
        let f = t.add_child(d, "f", NodeKind::File, 600, None);
        t.add_child(Tree::ROOT, "g", NodeKind::File, 200, None);
        (t, d, f)
    }

    #[test]
    fn nested_blocks_inside_parent() {
        let (t, d, f) = nested();
        let b = treemap_blocks(&t, Tree::ROOT, area(400.0, 300.0), 4);
        assert_eq!(b.len(), 3);
        let pd = b.iter().position(|x| x.node == d).unwrap();
        let pf = b.iter().position(|x| x.node == f).unwrap();
        assert!(pd < pf, "parents before children");
        let (bd, bf) = (b[pd], b[pf]);
        assert_eq!((bd.depth, bf.depth), (1, 2));
        assert_eq!((bd.branch, bf.branch), (0, 0));
        assert!(bd.rect.contains_rect(bf.rect));
        assert!(bf.rect.min.y >= bd.rect.min.y + PAD + LABEL_H - 1e-3);
        let g = b.iter().find(|x| x.depth == 1 && x.node != d).unwrap();
        assert_eq!(g.branch, 1);
    }

    #[test]
    fn hit_returns_deepest() {
        let (t, d, f) = nested();
        let b = treemap_blocks(&t, Tree::ROOT, area(400.0, 300.0), 4);
        let bf = b.iter().find(|x| x.node == f).unwrap();
        assert_eq!(treemap_hit(&b, bf.rect.center()), Some(f));
        let bd = b.iter().find(|x| x.node == d).unwrap();
        let near_corner = bd.rect.min + eframe::egui::vec2(0.5, 0.5);
        assert_eq!(treemap_hit(&b, near_corner), Some(d));
        assert_eq!(treemap_hit(&b, pos2(-5.0, -5.0)), None);
    }

    #[test]
    fn treemap_zero_sizes() {
        let mut t = Tree::new(PathBuf::from("r"));
        t.add_child(Tree::ROOT, "a", NodeKind::File, 0, None);
        let d = t.add_child(Tree::ROOT, "d", NodeKind::Dir, 0, None);
        t.add_child(d, "b", NodeKind::File, 0, None);
        assert!(treemap_blocks(&t, Tree::ROOT, area(400.0, 300.0), 4).is_empty());
        let one = Tree::new(PathBuf::from("r"));
        assert!(treemap_blocks(&one, Tree::ROOT, area(400.0, 300.0), 4).is_empty());
    }

    #[test]
    fn treemap_bounded_for_huge_dir() {
        let mut t = Tree::new(PathBuf::from("r"));
        for i in 0..100_000 {
            t.add_child(Tree::ROOT, &format!("f{i}"), NodeKind::File, 1, None);
        }
        let b = treemap_blocks(&t, Tree::ROOT, area(1000.0, 700.0), 4);
        assert!(b.len() as f32 <= 1000.0 * 700.0 / (MIN_PX * MIN_PX));
    }

    #[test]
    fn squarify_extreme_ratio_terminates() {
        for sizes in [[u64::MAX / 2, 1], [u64::MAX, u64::MAX]] {
            let r = squarify(&sizes, area(1000.0, 1000.0));
            assert!(!r.is_empty());
            for rect in r {
                let v = [rect.min.x, rect.min.y, rect.max.x, rect.max.y];
                assert!(v.iter().all(|c| c.is_finite() && *c >= 0.0), "{rect:?}");
            }
        }
    }

    fn inside(outer: Rect, inner: Rect) -> bool {
        outer.expand(1e-3).contains_rect(inner)
    }

    #[test]
    fn removed_child_has_no_block() {
        let mut t = Tree::new(PathBuf::from("r"));
        let a = t.add_child(Tree::ROOT, "a", NodeKind::File, 300, None);
        let b = t.add_child(Tree::ROOT, "b", NodeKind::File, 100, None);
        t.remove_subtree(a);
        let blocks = treemap_blocks(&t, Tree::ROOT, area(400.0, 300.0), 4);
        assert_eq!(blocks.iter().map(|x| x.node).collect::<Vec<_>>(), vec![b]);
    }

    #[test]
    fn max_depth_limits_output() {
        let (t, d, _) = nested();
        assert!(treemap_blocks(&t, Tree::ROOT, area(400.0, 300.0), 0).is_empty());
        let b = treemap_blocks(&t, Tree::ROOT, area(400.0, 300.0), 1);
        assert_eq!(b.len(), 2);
        assert!(b.iter().all(|x| x.depth == 1));
        assert!(b.iter().any(|x| x.node == d));
    }

    #[test]
    fn tiny_children_produce_no_block() {
        let mut t = Tree::new(PathBuf::from("r"));
        t.add_child(Tree::ROOT, "a", NodeKind::File, 1, None);
        t.add_child(Tree::ROOT, "b", NodeKind::File, 1, None);
        // each block is 1x2 px: below MIN_PX
        assert!(treemap_blocks(&t, Tree::ROOT, area(2.0, 2.0), 4).is_empty());
    }

    #[test]
    fn one_huge_many_tiny() {
        let mut t = Tree::new(PathBuf::from("r"));
        let huge = t.add_child(Tree::ROOT, "huge", NodeKind::File, 1_000_000_000, None);
        for i in 0..1000 {
            t.add_child(Tree::ROOT, &format!("t{i}"), NodeKind::File, 1, None);
        }
        let a = area(800.0, 600.0);
        let b = treemap_blocks(&t, Tree::ROOT, a, 4);
        assert!(b.iter().any(|x| x.node == huge));
        assert!(b.iter().all(|x| inside(a, x.rect)));
    }

    #[test]
    fn two_level_siblings_disjoint_and_nested() {
        let mut t = Tree::new(PathBuf::from("r"));
        for (n, sizes) in [("d1", [300u64, 200, 100]), ("d2", [250, 150, 50])] {
            let d = t.add_child(Tree::ROOT, n, NodeKind::Dir, 0, None);
            for (i, s) in sizes.iter().enumerate() {
                t.add_child(d, &format!("{n}f{i}"), NodeKind::File, *s, None);
            }
        }
        t.add_child(Tree::ROOT, "top", NodeKind::File, 400, None);
        let b = treemap_blocks(&t, Tree::ROOT, area(600.0, 400.0), 4);
        assert_eq!(b.len(), 3 + 6);
        for (i, x) in b.iter().enumerate() {
            for y in &b[i + 1..] {
                let (px, py) = (t.node(x.node).parent, t.node(y.node).parent);
                if px == py {
                    let o = x.rect.intersect(y.rect);
                    assert!(o.width() <= 1e-3 || o.height() <= 1e-3, "siblings overlap");
                }
            }
            if x.depth == 2 {
                let parent = b.iter().find(|p| Some(p.node) == t.node(x.node).parent);
                assert!(inside(parent.unwrap().rect, x.rect));
            }
        }
    }

    #[test]
    fn treemap_blocks_deterministic() {
        let (t, _, _) = nested();
        let a = treemap_blocks(&t, Tree::ROOT, area(400.0, 300.0), 4);
        let b = treemap_blocks(&t, Tree::ROOT, area(400.0, 300.0), 4);
        assert_eq!(a, b);
    }

    #[test]
    fn treemap_bounded_for_20k_children() {
        let mut t = Tree::new(PathBuf::from("r"));
        for i in 0..20_000 {
            t.add_child(Tree::ROOT, &format!("f{i}"), NodeKind::File, 1, None);
        }
        let b = treemap_blocks(&t, Tree::ROOT, area(1000.0, 700.0), 4);
        assert!(!b.is_empty());
        assert!(b.len() as f32 <= 1000.0 * 700.0 / (MIN_PX * MIN_PX));
    }
}
