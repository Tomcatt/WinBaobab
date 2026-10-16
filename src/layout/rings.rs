//! Ring (sunburst) chart geometry: segment layout and hit testing.

use crate::model::{NodeId, Tree};
use eframe::egui::{Pos2, Rect};
use std::f64::consts::TAU;

pub const MAX_DEPTH: u8 = 5;
/// Radians; smaller segments are not emitted.
pub const MIN_SWEEP: f32 = 0.01;

/// Angles in radians, 0 = 12 o'clock, clockwise.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RingSegment {
    pub node: NodeId,
    pub depth: u8,
    /// Index of the depth-1 ancestor in the focus's `children_by_size` order
    /// (the colour branch); inherited unchanged by descendants.
    pub branch: u32,
    pub start: f32,
    pub sweep: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RingGeometry {
    pub center: Pos2,
    pub center_radius: f32,
    pub ring_width: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RingHit {
    Center,
    Segment(NodeId),
}

/// The centre disc has the same radius as one ring; the whole chart
/// (centre plus `max_depth` rings) fits inside the smaller side of `rect`.
pub fn ring_geometry(rect: Rect, max_depth: u8) -> RingGeometry {
    let radius = (rect.width().min(rect.height()) / 2.0).max(0.0);
    let ring_width = radius / (max_depth as f32 + 1.0);
    RingGeometry {
        center: rect.center(),
        center_radius: ring_width,
        ring_width,
    }
}

/// Segments for the rings around `focus` (depth 1 = its direct children).
/// Children whose sweep is below `min_sweep` are skipped with their subtree.
pub fn ring_segments(
    tree: &Tree,
    focus: NodeId,
    max_depth: u8,
    min_sweep: f32,
) -> Vec<RingSegment> {
    let mut out = Vec::new();
    if max_depth > 0 {
        layout(
            tree,
            focus,
            1,
            0,
            0.0,
            TAU,
            max_depth,
            min_sweep as f64,
            &mut out,
        );
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn layout(
    tree: &Tree,
    parent: NodeId,
    depth: u8,
    branch: u32,
    start: f64,
    sweep: f64,
    max_depth: u8,
    min_sweep: f64,
    out: &mut Vec<RingSegment>,
) {
    let total = tree.node(parent).size;
    if total == 0 {
        return;
    }
    let mut child_start = start;
    // Largest first, so the first too-small child ends the scan.
    // Only children that reach the minimum sweep are sorted (same prefix
    // `children_by_size` would give; the rest would break the loop anyway).
    let (kids, _) = super::top_children(tree, parent, |size, _| {
        sweep * size as f64 / total as f64 >= min_sweep
    });
    for (i, c) in kids.into_iter().enumerate() {
        let child_branch = if depth == 1 { i as u32 } else { branch };
        let size = tree.node(c).size;
        let child_sweep = sweep * size as f64 / total as f64;
        if size == 0 || child_sweep < min_sweep {
            break;
        }
        out.push(RingSegment {
            node: c,
            depth,
            branch: child_branch,
            start: child_start as f32,
            sweep: child_sweep as f32,
        });
        if depth < max_depth {
            layout(
                tree,
                c,
                depth + 1,
                child_branch,
                child_start,
                child_sweep,
                max_depth,
                min_sweep,
                out,
            );
        }
        child_start += child_sweep;
    }
}

/// Which part of the chart is under `p`: the centre disc, a segment, or nothing.
pub fn ring_hit(segs: &[RingSegment], g: &RingGeometry, p: Pos2) -> Option<RingHit> {
    let (dx, dy) = ((p.x - g.center.x) as f64, (p.y - g.center.y) as f64);
    let dist = dx.hypot(dy);
    if dist <= g.center_radius as f64 {
        return Some(RingHit::Center);
    }
    if g.ring_width <= 0.0 {
        return None;
    }
    let depth = ((dist - g.center_radius as f64) / g.ring_width as f64).ceil();
    // Clockwise from 12 o'clock; screen y points down.
    let mut angle = dx.atan2(-dy).rem_euclid(TAU);
    if angle >= TAU {
        angle = 0.0; // rem_euclid can round a tiny negative angle up to TAU
    }
    segs.iter()
        .find(|s| {
            s.depth as f64 == depth
                && angle >= s.start as f64
                && angle < s.start as f64 + s.sweep as f64
        })
        .map(|s| RingHit::Segment(s.node))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::NodeKind;
    use eframe::egui::pos2;
    use std::f32::consts::{FRAC_1_SQRT_2 as K, FRAC_PI_2, FRAC_PI_4, PI, TAU as TAU32};
    use std::path::PathBuf;

    fn two_children() -> (Tree, NodeId, NodeId) {
        let mut t = Tree::new(PathBuf::from("r"));
        let a = t.add_child(Tree::ROOT, "a", NodeKind::File, 300, None);
        let b = t.add_child(Tree::ROOT, "b", NodeKind::File, 100, None);
        (t, a, b)
    }

    fn geom() -> RingGeometry {
        ring_geometry(Rect::from_min_max(pos2(0.0, 0.0), pos2(600.0, 600.0)), 5)
    }

    #[test]
    fn sweeps_proportional() {
        let (t, a, b) = two_children();
        let s = ring_segments(&t, Tree::ROOT, MAX_DEPTH, MIN_SWEEP);
        assert_eq!(s.len(), 2);
        assert_eq!((s[0].node, s[0].depth, s[0].start), (a, 1, 0.0));
        assert!((s[0].sweep - 3.0 * PI / 2.0).abs() < 1e-5);
        assert_eq!(s[1].node, b);
        assert!((s[1].start - 3.0 * PI / 2.0).abs() < 1e-5);
        assert!((s[1].sweep - FRAC_PI_2).abs() < 1e-5);
    }

    #[test]
    fn depth_limited() {
        let mut t = Tree::new(PathBuf::from("r"));
        let mut cur = Tree::ROOT;
        for i in 0..6 {
            cur = t.add_child(cur, &format!("d{i}"), NodeKind::Dir, 1, None);
        }
        let s = ring_segments(&t, Tree::ROOT, 5, MIN_SWEEP);
        assert_eq!(s.iter().map(|x| x.depth).max(), Some(5));
        assert_eq!(s.len(), 5);
    }

    #[test]
    fn tiny_segments_skipped() {
        let mut t = Tree::new(PathBuf::from("r"));
        let big = t.add_child(Tree::ROOT, "big", NodeKind::Dir, 0, None);
        t.add_child(big, "f", NodeKind::File, 1_000_000, None);
        let tiny = t.add_child(Tree::ROOT, "tiny", NodeKind::Dir, 0, None);
        t.add_child(tiny, "g", NodeKind::File, 1, None);
        let s = ring_segments(&t, Tree::ROOT, 5, MIN_SWEEP);
        assert!(s.iter().all(|x| x.node != tiny));
        assert!(s.iter().all(|x| t.node(x.node).name.as_ref() != "g"));
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn hit_center_and_segment() {
        let (t, a, _) = two_children();
        let g = geom();
        let s = ring_segments(&t, Tree::ROOT, MAX_DEPTH, MIN_SWEEP);
        assert_eq!(ring_hit(&s, &g, g.center), Some(RingHit::Center));
        let r = g.center_radius + g.ring_width * 0.5; // middle of depth 1
        let p = pos2(
            g.center.x + r * FRAC_PI_4.sin(),
            g.center.y - r * FRAC_PI_4.cos(),
        );
        assert_eq!(ring_hit(&s, &g, p), Some(RingHit::Segment(a)));
    }

    #[test]
    fn hit_outside_none() {
        let (t, ..) = two_children();
        let g = geom();
        let s = ring_segments(&t, Tree::ROOT, MAX_DEPTH, MIN_SWEEP);
        assert_eq!(
            ring_hit(&s, &g, pos2(g.center.x + 1000.0, g.center.y)),
            None
        );
        // inside a ring but where no segment exists (depth 2, nothing there)
        let r = g.center_radius + g.ring_width * 1.5;
        assert_eq!(ring_hit(&s, &g, pos2(g.center.x + r, g.center.y)), None);
    }

    #[test]
    fn rings_empty_root() {
        let t = Tree::new(PathBuf::from("r"));
        let g = geom();
        let s = ring_segments(&t, Tree::ROOT, MAX_DEPTH, MIN_SWEEP);
        assert!(s.is_empty());
        assert_eq!(ring_hit(&s, &g, g.center), Some(RingHit::Center));
        assert!(g.center.x.is_finite() && g.center_radius.is_finite() && g.ring_width.is_finite());
        // zero-size rect must not produce NaN either
        let z = ring_geometry(Rect::from_min_max(pos2(5.0, 5.0), pos2(5.0, 5.0)), 5);
        assert!(z.ring_width == 0.0 && z.center_radius == 0.0);
        assert_eq!(ring_hit(&s, &z, pos2(9.0, 9.0)), None);
    }

    #[test]
    fn rings_bounded_for_huge_dir() {
        let mut t = Tree::new(PathBuf::from("r"));
        let d = t.add_child(Tree::ROOT, "d", NodeKind::Dir, 0, None);
        for i in 0..100_000 {
            t.add_child(d, &format!("f{i}"), NodeKind::File, 1, None);
        }
        let s = ring_segments(&t, Tree::ROOT, MAX_DEPTH, MIN_SWEEP);
        assert!(s.len() <= (TAU32 / MIN_SWEEP) as usize * MAX_DEPTH as usize);
        assert!(s.len() < 10);
    }

    fn at(g: &RingGeometry, r: f32, dx: f32, dy: f32) -> Pos2 {
        pos2(g.center.x + r * dx, g.center.y + r * dy)
    }

    #[test]
    fn hit_cardinal_directions() {
        // a [0, 0.8pi) holds 12 and 3 o'clock; b [0.8pi, 1.4pi) holds 6;
        // c [1.4pi, 2pi) holds 9. No probe sits on a segment boundary.
        let mut t = Tree::new(PathBuf::from("r"));
        let a = t.add_child(Tree::ROOT, "a", NodeKind::File, 40, None);
        let b = t.add_child(Tree::ROOT, "b", NodeKind::File, 30, None);
        let c = t.add_child(Tree::ROOT, "c", NodeKind::File, 30, None);
        let g = geom();
        let s = ring_segments(&t, Tree::ROOT, MAX_DEPTH, MIN_SWEEP);
        let r = g.center_radius + g.ring_width * 0.5;
        let h = |dx, dy| ring_hit(&s, &g, at(&g, r, dx, dy));
        assert_eq!(h(0.0, -1.0), Some(RingHit::Segment(a)));
        assert_eq!(h(1.0, 0.0), Some(RingHit::Segment(a)));
        assert_eq!(h(0.0, 1.0), Some(RingHit::Segment(b)));
        assert_eq!(h(-1.0, 0.0), Some(RingHit::Segment(c)));
        // four equal children: one per quadrant, clockwise from 12
        let mut t = Tree::new(PathBuf::from("r"));
        let ids: Vec<_> = ["a", "b", "c", "d"]
            .iter()
            .map(|n| t.add_child(Tree::ROOT, n, NodeKind::File, 10, None))
            .collect();
        let s = ring_segments(&t, Tree::ROOT, MAX_DEPTH, MIN_SWEEP);
        let q = |dx, dy| ring_hit(&s, &g, at(&g, r, dx, dy));
        assert_eq!(q(K, -K), Some(RingHit::Segment(ids[0]))); // NE
        assert_eq!(q(K, K), Some(RingHit::Segment(ids[1]))); // SE
        assert_eq!(q(-K, K), Some(RingHit::Segment(ids[2]))); // SW
        assert_eq!(q(-K, -K), Some(RingHit::Segment(ids[3]))); // NW
    }

    #[test]
    fn hit_just_left_of_twelve_wraps_to_zero() {
        let (t, a, _) = two_children();
        let rect = Rect::from_min_max(pos2(-300.0, -300.0), pos2(300.0, 300.0));
        let g = ring_geometry(rect, 5);
        assert_eq!(g.center, pos2(0.0, 0.0));
        let s = ring_segments(&t, Tree::ROOT, MAX_DEPTH, MIN_SWEEP);
        // the angle is -1e-30, which rem_euclid rounds to exactly TAU
        assert_eq!((-1e-30f64).rem_euclid(TAU), TAU);
        let p = pos2(-1e-30, -(g.center_radius + g.ring_width * 0.5));
        assert_eq!(ring_hit(&s, &g, p), Some(RingHit::Segment(a)));
    }

    #[test]
    fn hit_radius_edges() {
        let (t, a, _) = two_children();
        let g = geom();
        let s = ring_segments(&t, Tree::ROOT, MAX_DEPTH, MIN_SWEEP);
        let on = |r: f32| pos2(g.center.x + r, g.center.y);
        assert_eq!(ring_hit(&s, &g, on(g.center_radius)), Some(RingHit::Center));
        let outer = g.center_radius + g.ring_width;
        assert_eq!(ring_hit(&s, &g, on(outer)), Some(RingHit::Segment(a)));
    }

    #[test]
    fn branch_is_depth1_index_inherited() {
        let mut t = Tree::new(PathBuf::from("r"));
        let a = t.add_child(Tree::ROOT, "a", NodeKind::Dir, 0, None);
        t.add_child(a, "a1", NodeKind::File, 10, None);
        let b = t.add_child(Tree::ROOT, "b", NodeKind::Dir, 0, None);
        t.add_child(b, "b1", NodeKind::File, 5, None);
        let s = ring_segments(&t, Tree::ROOT, 5, MIN_SWEEP);
        let br = |n: &str| {
            s.iter()
                .find(|x| &*t.node(x.node).name == n)
                .unwrap()
                .branch
        };
        assert_eq!((br("a"), br("a1"), br("b"), br("b1")), (0, 0, 1, 1));
    }

    #[test]
    fn geometry_fits_smaller_side() {
        let g = ring_geometry(Rect::from_min_max(pos2(0.0, 0.0), pos2(600.0, 300.0)), 5);
        assert_eq!(g.center, pos2(300.0, 150.0));
        assert_eq!(g.center_radius, g.ring_width);
        assert!((g.center_radius + 5.0 * g.ring_width - 150.0).abs() < 1e-3);
    }
}
