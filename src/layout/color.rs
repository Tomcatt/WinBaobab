//! Branch and depth colouring for the ring chart.

use crate::model::{NodeId, NodeKind, Tree};
use eframe::egui::Color32;

/// Tango: scarlet, plum, sky, chameleon, butter, orange.
pub const PALETTE: [Color32; 6] = [
    Color32::from_rgb(0xEF, 0x29, 0x29),
    Color32::from_rgb(0xAD, 0x7F, 0xA8),
    Color32::from_rgb(0x72, 0x9F, 0xCF),
    Color32::from_rgb(0x8A, 0xE2, 0x34),
    Color32::from_rgb(0xFC, 0xE9, 0x4F),
    Color32::from_rgb(0xFC, 0xAF, 0x3E),
];
pub const UNSEEN: Color32 = Color32::from_rgb(0x9A, 0x9A, 0x9A);

fn mix_toward_white(c: Color32, t: f32) -> Color32 {
    let f = |v: u8| {
        (v as f32 + (255.0 - v as f32) * t)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    Color32::from_rgb(f(c.r()), f(c.g()), f(c.b()))
}

/// Colour for a ring segment: `branch` is `RingSegment::branch`, `depth` its
/// ring (1 = the focus's direct children). `PALETTE[branch % 6]`, mixed
/// toward white by 15% per level below the first (capped at 60%). Cheap and
/// pure; use this per segment.
pub fn branch_color(branch: u32, depth: u8, unseen: bool) -> Color32 {
    if unseen {
        return UNSEEN;
    }
    let t = (0.15 * depth.saturating_sub(1) as f32).min(0.60);
    mix_toward_white(PALETTE[branch as usize % PALETTE.len()], t)
}

/// One-off colour of `node` when `focus` is the chart's centre. This is
/// O(n log n) in the focus's child count (it sorts them), so per-segment
/// rendering must use `RingSegment::branch` with [`branch_color`] instead.
/// Unseen nodes, and nodes that are not a strict descendant of `focus` (or
/// whose branch was removed), return [`UNSEEN`] grey.
pub fn node_color(tree: &Tree, focus: NodeId, node: NodeId) -> Color32 {
    let unseen = tree.node(node).kind == NodeKind::Unseen;
    // Walk up to the focus's direct child, counting levels.
    let (mut top, mut depth) = (node, 1u8);
    loop {
        match tree.node(top).parent {
            Some(p) if p == focus => break,
            Some(p) => {
                top = p;
                depth = depth.saturating_add(1);
            }
            None => return UNSEEN,
        }
    }
    match tree.children_by_size(focus).iter().position(|&c| c == top) {
        Some(branch) => branch_color(branch as u32, depth, unseen),
        None => UNSEEN,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn colors_by_branch_and_depth() {
        let mut t = Tree::new(PathBuf::from("r"));
        let a = t.add_child(Tree::ROOT, "a", NodeKind::Dir, 0, None);
        let b = t.add_child(Tree::ROOT, "b", NodeKind::Dir, 0, None);
        let a1 = t.add_child(a, "a1", NodeKind::File, 100, None);
        let b1 = t.add_child(b, "b1", NodeKind::File, 10, None);
        let a2 = t.add_child(a, "a2", NodeKind::Dir, 0, None);
        let a3 = t.add_child(a2, "a3", NodeKind::File, 5, None);
        t.set_unseen(1000);
        let u = t.children_by_size(Tree::ROOT)[0];
        assert_eq!(t.node(u).kind, NodeKind::Unseen);

        assert_eq!(node_color(&t, Tree::ROOT, u), UNSEEN);
        // sorted: unseen, a, b -> a is index 1, b index 2
        assert_eq!(node_color(&t, Tree::ROOT, a), PALETTE[1]);
        assert_eq!(node_color(&t, Tree::ROOT, b), PALETTE[2]);
        let (c, g) = (PALETTE[1], node_color(&t, Tree::ROOT, a1));
        assert_ne!(g, c);
        // lighter, same hue family: every channel >= the branch colour
        assert!(g.r() >= c.r() && g.g() >= c.g() && g.b() >= c.b());
        // deeper is lighter still
        let g3 = node_color(&t, Tree::ROOT, a3);
        assert!(g3.r() >= g.r() && g3.g() >= g.g() && g3.b() >= g.b());
        assert_ne!(g3, g);
        // sibling branch keeps its own palette entry
        let bc = node_color(&t, Tree::ROOT, b1);
        assert!(bc.b() >= PALETTE[2].b() && bc != node_color(&t, Tree::ROOT, a1));
    }

    #[test]
    fn branch_color_levels() {
        assert_eq!(branch_color(0, 1, false), PALETTE[0]);
        assert_eq!(branch_color(7, 1, false), PALETTE[1]); // wraps mod 6
        assert_eq!(branch_color(0, 3, true), UNSEEN);
        let c = PALETTE[2];
        assert_eq!(branch_color(2, 2, false), mix_toward_white(c, 0.15));
        assert_eq!(branch_color(2, 5, false), mix_toward_white(c, 0.60));
        assert_eq!(branch_color(2, 40, false), mix_toward_white(c, 0.60));
    }

    #[test]
    fn node_color_outside_focus_is_grey() {
        let mut t = Tree::new(PathBuf::from("r"));
        let a = t.add_child(Tree::ROOT, "a", NodeKind::Dir, 1, None);
        let b = t.add_child(a, "b", NodeKind::File, 1, None);
        assert_eq!(node_color(&t, a, a), UNSEEN);
        assert_eq!(node_color(&t, b, a), UNSEEN);
        assert_eq!(node_color(&t, a, b), PALETTE[0]);
    }

    #[test]
    fn white_mix_is_capped() {
        let mut t = Tree::new(PathBuf::from("r"));
        let mut cur = Tree::ROOT;
        for i in 0..8 {
            cur = t.add_child(cur, &format!("d{i}"), NodeKind::Dir, 1, None);
        }
        let deep = node_color(&t, Tree::ROOT, cur);
        assert_eq!(deep, mix_toward_white(PALETTE[0], 0.60));
    }
}
