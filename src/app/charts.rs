//! Interactive Rings and Treemap charts for the Results screen.
//!
//! The tree lock is only held while a layout snapshot is built (at most every
//! 250 ms while scanning) and for the brief lookups behind a hover tooltip.
//! Painting, labels and hit-testing read from the cached snapshot.

use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, Color32, FontId, Mesh, Pos2, Rect, Sense, Shape, Stroke, pos2};

use super::WinBaobabApp;
use crate::format::{format_count, format_size};
use crate::layout::{
    LABEL_H, MAX_DEPTH, MIN_SWEEP, PAD, RingGeometry, RingHit, RingSegment, TreemapBlock, branch_color,
    ring_geometry, ring_hit, ring_segments, treemap_blocks, treemap_hit,
};
use crate::model::{NodeId, NodeKind, Tree};
use crate::scan::ScanState;
use crate::settings::Chart;

/// Recompute at most this often while a scan is running.
const RECOMPUTE_EVERY: Duration = Duration::from_millis(250);
/// Nesting levels drawn by the treemap.
const TREEMAP_DEPTH: u8 = 3;
/// Ring arcs are tessellated at about one point per this many degrees.
const DEG_PER_POINT: f32 = 2.0;
/// Smallest treemap block that gets a label.
const LABEL_MIN: egui::Vec2 = egui::vec2(60.0, 28.0);
const DARK_TEXT: Color32 = Color32::from_rgb(20, 20, 20);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChartEvent {
    /// Left click on a folder: select it and focus it.
    DrillInto(NodeId),
    /// Left click on anything that can't be drilled into: select it.
    Select(NodeId),
    Up,
    ContextMenu(NodeId),
}

/// What the cached layout was computed for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ChartKey {
    pub focus: NodeId,
    pub generation: u64,
    /// Rounded chart rect size.
    pub size: (i32, i32),
    /// Rounded chart rect top-left, so a pane shift cannot leave stale rects.
    pub origin: (i32, i32),
    pub chart: Chart,
    /// Separator colour baked into the cached shapes (changes with the theme).
    pub bg: Color32,
}

/// A treemap label to draw on top of the cached shapes.
struct BlockLabel {
    rect: Rect,
    name: String,
    /// Second line; absent for folders whose children are drawn inside.
    size: Option<String>,
}

/// Everything painting and hit-testing need, built once per recompute.
#[derive(Default)]
struct Snapshot {
    /// Pre-built fills and separators.
    shapes: Vec<Shape>,
    rings: Vec<RingSegment>,
    /// Kind of each ring segment / treemap block (parallel to the layout vecs).
    ring_kinds: Vec<NodeKind>,
    blocks: Vec<TreemapBlock>,
    block_kinds: Vec<NodeKind>,
    labels: Vec<BlockLabel>,
    /// Ring centre text: focus name and formatted size.
    center_name: String,
    center_size: String,
}

impl Snapshot {
    fn kind_of(&self, node: NodeId) -> Option<NodeKind> {
        if let Some(i) = self.rings.iter().position(|s| s.node == node) {
            return Some(self.ring_kinds[i]);
        }
        self.blocks
            .iter()
            .rposition(|b| b.node == node)
            .map(|i| self.block_kinds[i])
    }
}

/// Cached layout, kept in `ResultsState`.
#[derive(Default)]
pub(crate) struct ChartCache {
    key: Option<ChartKey>,
    computed_at: Option<Instant>,
    snap: Snapshot,
    last_click_frame: Option<u64>,
    /// Whether the last frame actually drew the cached snapshot.
    shown: bool,
}

impl ChartCache {
    /// True if the chart on screen, drawn for `focus`, shows `node`: as the
    /// focus itself (ring centre / whole treemap) or as a segment or block.
    pub(crate) fn shows(&self, node: NodeId, focus: NodeId) -> bool {
        self.shown
            && self.key.is_some_and(|k| k.focus == focus)
            && (node == focus || self.snap.kind_of(node).is_some())
    }
}

/// Whether the layout must be rebuilt. Only a tree change is throttled (and
/// only while scanning); focus, size, position, theme or chart changes apply
/// immediately.
pub(crate) fn should_recompute(
    prev: Option<&ChartKey>,
    new: &ChartKey,
    running: bool,
    since_last: Option<Duration>,
) -> bool {
    let Some(prev) = prev else { return true };
    if prev == new {
        return false;
    }
    let only_generation = ChartKey {
        generation: new.generation,
        ..*prev
    } == *new;
    if only_generation && running {
        return since_last.is_none_or(|d| d >= RECOMPUTE_EVERY);
    }
    true
}

/// Tooltip text: `name / size / items / % of focus` (items only for folders).
pub(crate) fn tooltip_text(
    name: &str,
    size: u64,
    items: u64,
    is_dir: bool,
    focus_size: u64,
) -> String {
    let pct = if focus_size == 0 {
        0.0
    } else {
        size as f64 / focus_size as f64 * 100.0
    };
    let mut s = format!("{name}\n{}", format_size(size));
    if is_dir {
        s.push_str(&format!("\n{} items", format_count(items)));
    }
    s.push_str(&format!("\n{pct:.1}% of focus"));
    s
}

/// True the first time a click is seen in `frame`; repeats (extra passes of
/// the same frame) are rejected so one click is one navigation step.
pub(crate) fn accept_click(last: &mut Option<u64>, frame: u64) -> bool {
    if *last == Some(frame) {
        return false;
    }
    *last = Some(frame);
    true
}

/// Maps a click on the hovered node / centre to at most one event.
pub(crate) fn click_event(
    hovered: Option<(NodeId, NodeKind)>,
    over_center: bool,
    left: bool,
    right: bool,
) -> Option<ChartEvent> {
    match hovered {
        Some((n, kind)) => {
            if left && kind == NodeKind::Dir {
                Some(ChartEvent::DrillInto(n))
            } else if left {
                Some(ChartEvent::Select(n))
            } else if right {
                Some(ChartEvent::ContextMenu(n))
            } else {
                None
            }
        }
        None if over_center && left => Some(ChartEvent::Up),
        None => None,
    }
}

fn hover_text(tree: &Tree, focus: NodeId, node: NodeId) -> Option<String> {
    if !tree.is_live(node) {
        return None;
    }
    let n = tree.node(node);
    Some(tooltip_text(
        &n.name,
        n.size,
        n.items,
        n.kind == NodeKind::Dir,
        tree.node(focus).size,
    ))
}

fn arc_point(c: Pos2, r: f32, angle: f32) -> Pos2 {
    pos2(c.x + r * angle.sin(), c.y - r * angle.cos())
}

/// Points along an arc at radius `r`, about one per `DEG_PER_POINT` degrees.
fn arc_points(c: Pos2, r: f32, start: f32, sweep: f32) -> Vec<Pos2> {
    let n = ((sweep.to_degrees() / DEG_PER_POINT).ceil() as usize).max(1);
    (0..=n)
        .map(|i| arc_point(c, r, start + sweep * i as f32 / n as f32))
        .collect()
}

fn ring_outline(g: &RingGeometry, s: &RingSegment) -> (Vec<Pos2>, Vec<Pos2>) {
    let inner = g.center_radius + (s.depth as f32 - 1.0) * g.ring_width;
    (
        arc_points(g.center, inner, s.start, s.sweep),
        arc_points(g.center, inner + g.ring_width, s.start, s.sweep),
    )
}

/// Appends the strip between two equally long arcs to `mesh`.
fn add_strip(mesh: &mut Mesh, inner: &[Pos2], outer: &[Pos2], color: Color32) {
    let base = mesh.vertices.len() as u32;
    for (&a, &b) in inner.iter().zip(outer) {
        mesh.colored_vertex(a, color);
        mesh.colored_vertex(b, color);
    }
    for i in 0..inner.len().saturating_sub(1) as u32 {
        let k = base + i * 2;
        mesh.add_triangle(k, k + 1, k + 2);
        mesh.add_triangle(k + 1, k + 3, k + 2);
    }
}

fn closed_outline(inner: &[Pos2], outer: &[Pos2]) -> Vec<Pos2> {
    let mut pts = outer.to_vec();
    pts.extend(inner.iter().rev());
    pts
}

/// Appends a 1 px wide line as a quad.
fn add_line(mesh: &mut Mesh, a: Pos2, b: Pos2, color: Color32) {
    let d = b - a;
    let len = d.length();
    if len < 1e-3 {
        return;
    }
    let n = egui::vec2(-d.y, d.x) / len * 0.5;
    let base = mesh.vertices.len() as u32;
    for p in [a + n, a - n, b - n, b + n] {
        mesh.colored_vertex(p, color);
    }
    mesh.add_triangle(base, base + 1, base + 2);
    mesh.add_triangle(base, base + 2, base + 3);
}

/// Appends the edges of a closed polygon as 1 px lines.
fn push_closed(lines: &mut Mesh, pts: &[Pos2], color: Color32) {
    for i in 0..pts.len() {
        add_line(lines, pts[i], pts[(i + 1) % pts.len()], color);
    }
}

/// Where a treemap block's label may go without touching any child block.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum LabelSlot {
    None,
    /// Name only, inside the strip the layout reserved above the children.
    Header(Rect),
    /// Name and size on the whole (childless) block.
    Full(Rect),
}

/// Pure label placement. Mirrors the layout: children are inset by `PAD` and
/// a `LABEL_H` strip is reserved only when the inset area is tall enough.
pub(crate) fn label_slot(rect: Rect, has_children: bool) -> LabelSlot {
    if has_children {
        let inner = rect.shrink(PAD);
        if inner.height() > 3.0 * LABEL_H && inner.width() >= 24.0 {
            return LabelSlot::Header(Rect::from_min_size(
                inner.min,
                egui::vec2(inner.width(), LABEL_H),
            ));
        }
        return LabelSlot::None;
    }
    if rect.width() >= LABEL_MIN.x && rect.height() >= LABEL_MIN.y {
        LabelSlot::Full(rect)
    } else {
        LabelSlot::None
    }
}

/// Text shown in the ring centre for the scan root: the last path component
/// (`C:\Users\TomCatT` -> `TomCatT`); drive roots such as `C:\` stay as is.
pub(crate) fn center_display_name(path: &str) -> String {
    let trimmed = path.trim_end_matches(['\\', '/']);
    match trimmed.rsplit(['\\', '/']).next() {
        Some(last) if !last.is_empty() && !last.ends_with(':') => last.to_string(),
        _ => path.to_string(),
    }
}

/// Shortens `text` with a trailing "…" until `measure` says it fits `max_w`.
pub(crate) fn elide_to_width(text: &str, max_w: f32, measure: impl Fn(&str) -> f32) -> String {
    if measure(text) <= max_w {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let make = |n: usize| {
        let mut s: String = chars[..n].iter().collect();
        s.push('…');
        s
    };
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if measure(&make(mid)) <= max_w {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    make(lo)
}

/// Builds the whole drawable snapshot for one chart. Takes the tree by
/// reference so the caller decides how long the lock is held.
fn build_snapshot(tree: &Tree, focus: NodeId, rect: Rect, chart: Chart, bg: Color32) -> Snapshot {
    let mut snap = Snapshot::default();
    let fnode = tree.node(focus);
    snap.center_name = if focus == Tree::ROOT {
        center_display_name(&tree.root_path.display().to_string())
    } else {
        fnode.name.to_string()
    };
    snap.center_size = format_size(fnode.size);
    let mut mesh = Mesh::default();
    let mut lines = Mesh::default();
    match chart {
        Chart::Rings => {
            let g = ring_geometry(rect, MAX_DEPTH);
            snap.rings = ring_segments(tree, focus, MAX_DEPTH, MIN_SWEEP);
            for s in &snap.rings {
                let kind = tree.node(s.node).kind;
                snap.ring_kinds.push(kind);
                let color = branch_color(s.branch, s.depth, kind == NodeKind::Unseen);
                let (inner, outer) = ring_outline(&g, s);
                add_strip(&mut mesh, &inner, &outer, color);
                push_closed(&mut lines, &closed_outline(&inner, &outer), bg);
            }
        }
        Chart::Treemap => {
            snap.blocks = treemap_blocks(tree, focus, rect, TREEMAP_DEPTH);
            for (i, b) in snap.blocks.iter().enumerate() {
                let n = tree.node(b.node);
                snap.block_kinds.push(n.kind);
                // Blocks are in pre-order, so a deeper next block is a child.
                let has_children = snap.blocks.get(i + 1).is_some_and(|c| c.depth > b.depth);
                let color = branch_color(b.branch, b.depth, n.kind == NodeKind::Unseen);
                mesh.add_colored_rect(b.rect, color);
                let r = b.rect.shrink(0.5);
                push_closed(
                    &mut lines,
                    &[r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom()],
                    bg,
                );
                match label_slot(b.rect, has_children) {
                    LabelSlot::None => {}
                    LabelSlot::Header(strip) => snap.labels.push(BlockLabel {
                        rect: strip,
                        name: n.name.to_string(),
                        size: None,
                    }),
                    LabelSlot::Full(r) => snap.labels.push(BlockLabel {
                        rect: r,
                        name: n.name.to_string(),
                        size: Some(format_size(n.size)),
                    }),
                }
            }
        }
    }
    snap.shapes.push(Shape::mesh(mesh));
    snap.shapes.push(Shape::mesh(lines));
    snap
}

pub(crate) fn chart_ui(app: &mut WinBaobabApp, ui: &mut egui::Ui) -> Option<ChartEvent> {
    let rect = ui.available_rect_before_wrap();
    let bg = ui.visuals().extreme_bg_color;
    ui.painter().rect_filled(rect, 4.0, bg);
    // Cleared until this frame really draws the chart.
    app.results.chart.shown = false;
    let selected = app.actions.selected;
    let h = app.scan.as_ref()?;
    if rect.width() < 8.0 || rect.height() < 8.0 {
        return None;
    }
    let resp = ui.allocate_rect(rect, Sense::click());
    let running = matches!(h.state(), ScanState::Running);
    let focus = app.focus;
    let chart = app.chart;
    let cache = &mut app.results.chart;

    // Short lock: just the generation and focus liveness.
    let (generation, live) = {
        let tree = h.read_tree();
        (tree.generation, tree.is_live(focus))
    };
    if !live {
        return None;
    }
    let key = ChartKey {
        focus,
        generation,
        size: (rect.width().round() as i32, rect.height().round() as i32),
        origin: (rect.min.x.round() as i32, rect.min.y.round() as i32),
        chart,
        bg,
    };
    let since = cache.computed_at.map(|t| t.elapsed());
    if should_recompute(cache.key.as_ref(), &key, running, since) {
        // The lock is held only while the snapshot is built.
        cache.snap = build_snapshot(&h.read_tree(), focus, rect, chart, bg);
        cache.key = Some(key);
        cache.computed_at = Some(Instant::now());
    } else if cache.key != Some(key) {
        // Throttled: make sure a later frame picks the change up.
        let wait = RECOMPUTE_EVERY.saturating_sub(since.unwrap_or_default());
        ui.ctx().request_repaint_after(wait);
    }

    cache.shown = true;
    let snap = &cache.snap;
    let painter = ui.painter_at(rect);
    painter.extend(snap.shapes.iter().cloned());
    let strong = ui.visuals().strong_text_color();
    // The selection: a dark halo under a selection-coloured line, so it
    // reads on any fill and differs from the plain hover outline.
    let sel_col = ui.visuals().selection.stroke.color;
    let sel_strokes = [Stroke::new(5.0, bg), Stroke::new(2.5, sel_col)];
    let pointer = resp.hover_pos();
    let mut hovered: Option<NodeId> = None;
    let mut over_center = false;

    match chart {
        Chart::Rings => {
            let g = ring_geometry(rect, MAX_DEPTH);
            let hit = pointer.and_then(|p| ring_hit(&snap.rings, &g, p));
            if let Some(RingHit::Segment(n)) = hit {
                hovered = Some(n);
            }
            over_center = hit == Some(RingHit::Center);
            if let Some(n) = hovered
                && let Some(s) = snap.rings.iter().find(|s| s.node == n)
            {
                let (inner, outer) = ring_outline(&g, s);
                painter.add(Shape::closed_line(
                    closed_outline(&inner, &outer),
                    Stroke::new(2.0, strong),
                ));
            }
            if let Some(n) = selected
                && let Some(s) = snap.rings.iter().find(|s| s.node == n)
            {
                let (inner, outer) = ring_outline(&g, s);
                let line = closed_outline(&inner, &outer);
                for st in sel_strokes {
                    painter.add(Shape::closed_line(line.clone(), st));
                }
            }
            painter.circle_filled(g.center, g.center_radius, ui.visuals().panel_fill);
            if selected == Some(focus) {
                for st in sel_strokes {
                    painter.circle_stroke(g.center, g.center_radius, st);
                }
            } else {
                let ring_col = if over_center && focus != Tree::ROOT {
                    strong
                } else {
                    ui.visuals().widgets.noninteractive.bg_stroke.color
                };
                painter.circle_stroke(g.center, g.center_radius, Stroke::new(1.5, ring_col));
            }
            let clip = Rect::from_center_size(g.center, egui::Vec2::splat(g.center_radius * 1.6));
            let cp = painter.with_clip_rect(clip);
            let name_font = FontId::proportional(12.0);
            let text_col = ui.visuals().text_color();
            let max_w = 1.84 * (g.center_radius.powi(2) - 64.0).max(0.0).sqrt();
            let name = elide_to_width(&snap.center_name, max_w, |s| {
                painter
                    .layout_no_wrap(s.to_string(), name_font.clone(), text_col)
                    .size()
                    .x
            });
            cp.text(
                g.center - egui::vec2(0.0, 7.0),
                Align2::CENTER_CENTER,
                name,
                name_font,
                text_col,
            );
            cp.text(
                g.center + egui::vec2(0.0, 8.0),
                Align2::CENTER_CENTER,
                &snap.center_size,
                FontId::proportional(11.0),
                ui.visuals().weak_text_color(),
            );
        }
        Chart::Treemap => {
            let font = FontId::proportional(12.0);
            for l in &snap.labels {
                let lp = painter.with_clip_rect(l.rect.shrink(1.0));
                let at = l.rect.min + egui::vec2(4.0, 2.0);
                let r = lp.text(at, Align2::LEFT_TOP, &l.name, font.clone(), DARK_TEXT);
                if let Some(size) = &l.size {
                    lp.text(
                        pos2(at.x, r.bottom()),
                        Align2::LEFT_TOP,
                        size,
                        font.clone(),
                        DARK_TEXT,
                    );
                }
            }
            hovered = pointer.and_then(|p| treemap_hit(&snap.blocks, p));
            if let Some(n) = hovered
                && let Some(b) = snap.blocks.iter().rev().find(|b| b.node == n)
            {
                painter.rect_stroke(
                    b.rect,
                    0.0,
                    Stroke::new(2.0, strong),
                    egui::StrokeKind::Inside,
                );
            }
            // The focused folder fills the whole treemap.
            let sel_rect = match selected {
                Some(n) if n == focus => Some(rect),
                Some(n) => snap
                    .blocks
                    .iter()
                    .rev()
                    .find(|b| b.node == n)
                    .map(|b| b.rect),
                None => None,
            };
            if let Some(r) = sel_rect {
                for st in sel_strokes {
                    painter.rect_stroke(r, 0.0, st, egui::StrokeKind::Inside);
                }
            }
        }
    }

    let hover_kind = hovered.and_then(|n| snap.kind_of(n).map(|k| (n, k)));
    if let Some((n, _)) = hover_kind {
        // Brief lock for the tooltip's live numbers.
        let text = hover_text(&h.read_tree(), focus, n);
        if let Some(text) = text {
            resp.clone().on_hover_ui_at_pointer(|ui| {
                for line in text.lines() {
                    ui.label(line);
                }
            });
        }
    }
    // egui may run several passes per frame with the same input, so a click
    // is consumed at most once per frame.
    let frame = ui.ctx().cumulative_frame_nr();
    let left = resp.clicked() && accept_click(&mut cache.last_click_frame, frame);
    let right = resp.secondary_clicked() && accept_click(&mut cache.last_click_frame, frame);
    click_event(hover_kind, over_center, left, right)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn key(generation: u64) -> ChartKey {
        ChartKey {
            focus: 0,
            generation,
            size: (100, 100),
            origin: (10, 10),
            chart: Chart::Rings,
            bg: Color32::BLACK,
        }
    }

    const FRESH: Option<Duration> = Some(Duration::from_millis(10));
    const OLD: Option<Duration> = Some(Duration::from_millis(300));

    #[test]
    fn repeated_pass_click_is_ignored() {
        let mut last = None;
        assert!(accept_click(&mut last, 5));
        assert!(!accept_click(&mut last, 5));
        assert!(accept_click(&mut last, 6));
    }

    #[test]
    fn one_click_one_event() {
        let d = Some((3, NodeKind::Dir));
        assert_eq!(
            click_event(d, false, true, false),
            Some(ChartEvent::DrillInto(3))
        );
        assert_eq!(click_event(d, false, false, false), None);
        assert_eq!(click_event(None, true, true, false), Some(ChartEvent::Up));
        assert_eq!(click_event(None, false, true, false), None);
    }

    #[test]
    fn files_and_unseen_never_drill() {
        for kind in [NodeKind::File, NodeKind::Unseen, NodeKind::Link] {
            let h = Some((4, kind));
            // A left click selects what was clicked instead.
            assert_eq!(
                click_event(h, false, true, false),
                Some(ChartEvent::Select(4))
            );
            // Right-click still reaches the context menu.
            assert_eq!(
                click_event(h, false, false, true),
                Some(ChartEvent::ContextMenu(4))
            );
        }
    }

    #[test]
    fn label_slot_rules() {
        let tall = Rect::from_min_size(pos2(0.0, 0.0), egui::vec2(200.0, 100.0));
        let short = Rect::from_min_size(pos2(0.0, 0.0), egui::vec2(200.0, 40.0));
        match label_slot(tall, true) {
            LabelSlot::Header(s) => {
                assert!(tall.contains_rect(s));
                assert_eq!(s.height(), LABEL_H);
            }
            other => panic!("{other:?}"),
        }
        // Children but no reserved header: no label at all.
        assert_eq!(label_slot(short, true), LabelSlot::None);
        // Leaves keep the 60x28 rule.
        assert_eq!(label_slot(short, false), LabelSlot::Full(short));
        let tiny = Rect::from_min_size(pos2(0.0, 0.0), egui::vec2(59.0, 40.0));
        assert_eq!(label_slot(tiny, false), LabelSlot::None);
    }

    #[test]
    fn nested_labels_never_intersect() {
        let mut t = Tree::new(PathBuf::from("r"));
        let a = t.add_child(Tree::ROOT, "a", NodeKind::Dir, 0, None);
        let b = t.add_child(a, "b", NodeKind::Dir, 0, None);
        t.add_child(b, "f1", NodeKind::File, 500, None);
        t.add_child(b, "f2", NodeKind::File, 300, None);
        t.add_child(a, "f3", NodeKind::File, 400, None);
        t.add_child(Tree::ROOT, "g", NodeKind::File, 300, None);
        for (w, h) in [(900.0, 700.0), (400.0, 300.0), (200.0, 90.0)] {
            let rect = Rect::from_min_size(pos2(0.0, 0.0), egui::vec2(w, h));
            let blocks = treemap_blocks(&t, Tree::ROOT, rect, TREEMAP_DEPTH);
            let slots: Vec<Rect> = blocks
                .iter()
                .enumerate()
                .filter_map(|(i, b)| {
                    let kids = blocks.get(i + 1).is_some_and(|c| c.depth > b.depth);
                    match label_slot(b.rect, kids) {
                        LabelSlot::None => None,
                        LabelSlot::Header(r) | LabelSlot::Full(r) => Some(r),
                    }
                })
                .collect();
            for (i, x) in slots.iter().enumerate() {
                for y in &slots[i + 1..] {
                    // Touching edges are fine; only a shared area counts.
                    let i = x.intersect(*y);
                    let overlap = i.width() > 0.01 && i.height() > 0.01;
                    assert!(!overlap, "{x:?} overlaps {y:?} at {w}x{h}");
                }
            }
        }
    }

    #[test]
    fn centre_name_is_last_component() {
        assert_eq!(center_display_name(r"C:\Users\TomCatT"), "TomCatT");
        assert_eq!(center_display_name(r"C:\Users\TomCatT\"), "TomCatT");
        assert_eq!(center_display_name(r"C:\"), r"C:\");
        assert_eq!(center_display_name(r"\\nas\share"), "share");
    }

    #[test]
    fn elide_adds_ellipsis_only_when_needed() {
        let m = |s: &str| s.chars().count() as f32;
        assert_eq!(elide_to_width("TomCatT", 10.0, m), "TomCatT");
        assert_eq!(elide_to_width("abcdefghij", 5.0, m), "abcd…");
        assert_eq!(elide_to_width("abc", 0.0, m), "…");
    }

    #[test]
    fn first_compute_always() {
        assert!(should_recompute(None, &key(1), true, FRESH));
    }

    #[test]
    fn unchanged_key_is_cached() {
        assert!(!should_recompute(Some(&key(1)), &key(1), false, OLD));
    }

    #[test]
    fn generation_change_is_throttled_only_while_running() {
        assert!(!should_recompute(Some(&key(1)), &key(2), true, FRESH));
        assert!(should_recompute(Some(&key(1)), &key(2), true, OLD));
        assert!(should_recompute(Some(&key(1)), &key(2), false, FRESH));
    }

    #[test]
    fn focus_size_origin_theme_or_chart_change_is_immediate() {
        let base = key(1);
        let variants = [
            ChartKey { focus: 3, ..base },
            ChartKey {
                size: (120, 100),
                ..base
            },
            ChartKey {
                origin: (40, 10),
                ..base
            },
            ChartKey {
                bg: Color32::WHITE,
                ..base
            },
            ChartKey {
                chart: Chart::Treemap,
                ..base
            },
        ];
        for k in variants {
            assert!(should_recompute(Some(&base), &k, true, FRESH));
        }
    }

    #[test]
    fn tooltip_formats_dir_and_file() {
        let d = tooltip_text("src", 2048, 1312, true, 4096);
        assert_eq!(d, "src\n2.0 KB\n1,312 items\n50.0% of focus");
        let f = tooltip_text("a.txt", 1, 0, false, 0);
        assert_eq!(f, "a.txt\n1 byte\n0.0% of focus");
    }

    #[test]
    fn snapshot_has_shapes_kinds_and_labels() {
        let mut t = Tree::new(PathBuf::from("r"));
        let d = t.add_child(Tree::ROOT, "dir", NodeKind::Dir, 0, None);
        t.add_child(d, "f", NodeKind::File, 800, None);
        t.add_child(Tree::ROOT, "g", NodeKind::File, 200, None);
        let rect = Rect::from_min_size(pos2(0.0, 0.0), egui::vec2(400.0, 300.0));
        let rings = build_snapshot(&t, Tree::ROOT, rect, Chart::Rings, Color32::BLACK);
        assert_eq!(rings.rings.len(), rings.ring_kinds.len());
        assert_eq!(rings.kind_of(d), Some(NodeKind::Dir));
        assert_eq!(rings.shapes.len(), 2);
        let tm = build_snapshot(&t, Tree::ROOT, rect, Chart::Treemap, Color32::BLACK);
        assert_eq!(tm.blocks.len(), tm.block_kinds.len());
        assert!(!tm.labels.is_empty());
        // The folder's block has a child inside, so no size line.
        assert!(tm.labels.iter().any(|l| l.name == "dir" && l.size.is_none()));
    }

    #[test]
    fn chart_shows_only_what_it_drew_for_the_current_focus() {
        let mut t = Tree::new(PathBuf::from("r"));
        let d = t.add_child(Tree::ROOT, "dir", NodeKind::Dir, 0, None);
        let f = t.add_child(d, "f", NodeKind::File, 800, None);
        let g = t.add_child(Tree::ROOT, "g", NodeKind::File, 200, None);
        let rect = Rect::from_min_size(pos2(0.0, 0.0), egui::vec2(400.0, 300.0));
        let mut cache = ChartCache {
            key: Some(ChartKey {
                focus: d,
                ..key(t.generation)
            }),
            snap: build_snapshot(&t, d, rect, Chart::Rings, Color32::BLACK),
            ..Default::default()
        };
        // Not drawn this frame: nothing counts as shown.
        assert!(!cache.shows(f, d));
        cache.shown = true;
        assert!(cache.shows(f, d), "segment under the focus");
        assert!(cache.shows(d, d), "the focus itself (ring centre)");
        assert!(!cache.shows(g, d), "outside the focus: not drawn");
        // Drawn for another focus: stale, so nothing counts.
        assert!(!cache.shows(f, Tree::ROOT));
    }

    #[test]
    fn huge_directory_recompute_time() {
        let mut t = Tree::new(PathBuf::from("r"));
        for i in 0..100_000u64 {
            t.add_child(Tree::ROOT, &format!("f{i}"), NodeKind::File, 1 + i % 977, None);
        }
        let rect = Rect::from_min_size(pos2(0.0, 0.0), egui::vec2(900.0, 700.0));
        for chart in [Chart::Rings, Chart::Treemap] {
            let t0 = Instant::now();
            let snap = build_snapshot(&t, Tree::ROOT, rect, chart, Color32::BLACK);
            let dt = t0.elapsed();
            println!("100k children {chart:?}: {dt:?}, shapes {}", snap.shapes.len());
            assert!(dt < Duration::from_secs(2), "{chart:?} took {dt:?}");
        }
    }
}
