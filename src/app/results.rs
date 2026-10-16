//! Results screen: header (back, breadcrumb, rescan, chart toggle), tree
//! table, chart area and status line.

use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, Color32, FontId, Sense, vec2};

use super::WinBaobabApp;
use super::charts::ChartEvent;
use crate::format::{format_count, format_size};
use crate::model::{NodeId, NodeKind, Tree};
use crate::scan::ScanState;
use crate::settings::Chart;
use crate::winfs::local_datetime_string;

const INDENT: f32 = 14.0;
const SIZE_W: f32 = 84.0;
const CONTENTS_W: f32 = 76.0;
const MODIFIED_W: f32 = 120.0;
const BAR_W: f32 = 90.0;
const NAME_MIN_W: f32 = 120.0;
const REBUILD_EVERY: Duration = Duration::from_millis(500);

/// One visible line of the flattened tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FlatRow {
    pub id: NodeId,
    pub depth: u32,
}

/// Per-scan UI state of the Results screen.
pub(crate) struct ResultsState {
    pub expanded: HashSet<NodeId>,
    pub(crate) expanded_version: u64,
    pub(crate) rows: Vec<FlatRow>,
    /// Ids of the tree rows drawn last frame (the scrolled-to range only).
    pub(crate) on_screen: Vec<NodeId>,
    /// (tree generation, expanded version) the cached data was built for.
    cache_key: Option<(u64, u64, bool)>,
    cache_focus: NodeId,
    last_rebuild: Option<Instant>,
    /// Expanded folders (as names below the root) to re-expand once a rescan finds them.
    pub(crate) pending_expand: Vec<Vec<String>>,
    unreadable_count: usize,
    unreadable_ids: Vec<NodeId>,
    /// Paths shown in the unreadable-folders window, when open.
    unreadable_window: Option<Vec<String>>,
    pub chart: super::charts::ChartCache,
}

impl Default for ResultsState {
    fn default() -> Self {
        ResultsState {
            expanded: HashSet::from([Tree::ROOT]),
            expanded_version: 0,
            rows: Vec::new(),
            on_screen: Vec::new(),
            cache_key: None,
            cache_focus: Tree::ROOT,
            last_rebuild: None,
            pending_expand: Vec::new(),
            unreadable_count: 0,
            unreadable_ids: Vec::new(),
            unreadable_window: None,
            chart: Default::default(),
        }
    }
}

/// Node ids from the scan root down to `focus` (inclusive).
pub(crate) fn breadcrumb_ids(tree: &Tree, focus: NodeId) -> Vec<NodeId> {
    let mut ids = vec![focus];
    let mut cur = focus;
    while let Some(p) = tree.node(cur).parent {
        ids.push(p);
        cur = p;
    }
    ids.reverse();
    ids
}

/// Expanded nodes flattened depth-first, children largest first. Only
/// expanded directories are descended, so cost is proportional to what is shown.
pub(crate) fn flatten_rows(tree: &Tree, expanded: &HashSet<NodeId>) -> Vec<FlatRow> {
    let mut out = vec![FlatRow {
        id: Tree::ROOT,
        depth: 0,
    }];
    if !expanded.contains(&Tree::ROOT) {
        return out;
    }
    // Children are pushed reversed so pops come out largest first.
    let mut stack: Vec<(NodeId, u32)> = tree
        .children_by_size(Tree::ROOT)
        .into_iter()
        .rev()
        .map(|c| (c, 1))
        .collect();
    while let Some((id, depth)) = stack.pop() {
        out.push(FlatRow { id, depth });
        if expanded.contains(&id) && !tree.node(id).children.is_empty() {
            stack.extend(
                tree.children_by_size(id)
                    .into_iter()
                    .rev()
                    .map(|c| (c, depth + 1)),
            );
        }
    }
    out
}

/// Finds the node reached by following `names` down from the root.
pub(crate) fn find_by_names(tree: &Tree, names: &[String]) -> Option<NodeId> {
    let mut cur = Tree::ROOT;
    for name in names {
        cur = *tree
            .node(cur)
            .children
            .iter()
            .find(|&&c| !tree.node(c).removed && *tree.node(c).name == **name)?;
    }
    Some(cur)
}

/// `{n} folders couldn't be read — access denied` (singular for 1).
pub(crate) fn unreadable_text(n: usize) -> String {
    let noun = if n == 1 { "folder" } else { "folders" };
    format!("{n} {noun} couldn't be read — access denied")
}

/// Parent of `focus`, or None at the scan root.
pub(crate) fn parent_of(tree: &Tree, focus: NodeId) -> Option<NodeId> {
    tree.node(focus).parent
}

/// The node "up" navigation (chart centre click, Parent button) moves to.
pub(crate) fn focus_parent(app: &WinBaobabApp) -> Option<NodeId> {
    let h = app.scan.as_ref()?;
    let tree = h.read_tree();
    if tree.is_live(app.focus) {
        parent_of(&tree, app.focus)
    } else {
        None
    }
}

/// Moves the chart focus to `id`; ignores removed nodes and out-of-range ids.
/// A selection that isn't `id` or inside it is dropped, so Delete can never
/// hit something left behind by navigation.
pub(crate) fn set_focus(app: &mut WinBaobabApp, id: NodeId) {
    let Some(h) = app.scan.as_ref() else { return };
    let (ok, keep) = {
        let tree = h.read_tree();
        let keep = app
            .actions
            .selected
            .is_some_and(|s| super::actions::selection_survives_focus(&tree, s, id));
        (tree.is_live(id), keep)
    };
    if ok {
        app.focus = id;
        if !keep {
            app.actions.selected = None;
        }
    }
}

struct RowView {
    id: NodeId,
    depth: u32,
    name: String,
    kind: NodeKind,
    size: u64,
    items: u64,
    modified: Option<String>,
    parent_size: u64,
    expandable: bool,
    expanded: bool,
    /// Node a click focuses: the row itself for dirs, else its parent dir.
    focus_target: NodeId,
}

#[derive(Default)]
struct Actions {
    focus: Option<NodeId>,
    toggle: Option<NodeId>,
    rescan: bool,
    back: bool,
    cancel: bool,
    chart: Option<Chart>,
    open_unreadable: bool,
    select: Option<NodeId>,
    menu: Option<(NodeId, egui::Pos2)>,
    settings: bool,
}

pub fn results_ui(app: &mut WinBaobabApp, ui: &mut egui::Ui) {
    let ctx = ui.ctx().clone();
    let mut act = Actions::default();

    let (running, state) = match &app.scan {
        Some(h) => {
            let s = h.state();
            (matches!(s, ScanState::Running), Some(s))
        }
        None => (false, None),
    };
    let busy = running || app.is_starting();
    let modal = app.actions.is_modal_open();
    let typing = ctx.egui_wants_keyboard_input();
    let f5 = ui.input(|i| i.key_pressed(egui::Key::F5));
    if super::actions::rescan_key_allowed(busy, modal, typing, f5) {
        act.rescan = true;
    }

    // Phase 1: everything the frame needs from the tree, under one short read lock.
    let mut crumbs: Vec<(NodeId, String)> = Vec::new();
    let mut totals = (0u64, 0u64);
    if let Some(h) = &app.scan {
        let tree = h.read_tree();
        let r = &mut app.results;
        let key = (tree.generation, r.expanded_version, running);
        let force =
            r.cache_key.is_none_or(|k| k.1 != key.1 || k.2 != key.2) || r.cache_focus != app.focus;
        let changed = r.cache_key != Some(key);
        let due =
            changed && (!running || r.last_rebuild.is_none_or(|t| t.elapsed() >= REBUILD_EVERY));
        if force || due {
            // Re-expand folders remembered from before a rescan, as they appear.
            if !r.pending_expand.is_empty() {
                let before = r.pending_expand.len();
                let mut found = Vec::new();
                r.pending_expand
                    .retain(|names| match find_by_names(&tree, names) {
                        Some(id) => {
                            found.push(id);
                            false
                        }
                        None => running,
                    });
                r.expanded.extend(found);
                if r.pending_expand.len() != before {
                    r.expanded_version += 1;
                }
            }
            let key = (tree.generation, r.expanded_version, running);
            r.last_rebuild = Some(Instant::now());
            r.cache_focus = app.focus;
            app.results.rows = flatten_rows(&tree, &app.results.expanded);
            // The unreadable list walks the whole tree; skip it while it churns.
            app.results.unreadable_ids = if running {
                Vec::new()
            } else {
                tree.unreadable_dirs()
            };
            app.results.unreadable_count = app.results.unreadable_ids.len();
            app.results.cache_key = Some(key);
        }
        if !tree.is_live(app.focus) {
            app.focus = Tree::ROOT;
        }
        for id in breadcrumb_ids(&tree, app.focus) {
            let label = if id == Tree::ROOT {
                tree.root_path.display().to_string()
            } else {
                tree.node(id).name.to_string()
            };
            crumbs.push((id, label));
        }
        let root = tree.node(Tree::ROOT);
        totals = (root.size, root.items);
    }
    // A selection that scrolled out of view (collapsed, refocused) is dropped.
    super::actions::prune_selection(app);

    egui::Panel::top("results_header").show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            if ui.button("Back").clicked() {
                act.back = true;
            }
            // The up-arrow glyph is missing from egui's proportional fonts: plain text.
            if ui
                .add_enabled(app.focus != Tree::ROOT, egui::Button::new("Parent"))
                .on_hover_text("Go up to the parent folder (Backspace)")
                .clicked()
            {
                act.focus = focus_parent(app);
            }
            for (i, (id, label)) in crumbs.iter().enumerate() {
                if i > 0 {
                    ui.label("›");
                }
                if ui.link(label).clicked() {
                    act.focus = Some(*id);
                }
            }
            if crumbs.is_empty() {
                ui.heading(app.scan_path.display().to_string());
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // "Settings" in text: the gear glyph may be missing from egui's fonts.
                if ui.button("Settings").clicked() {
                    act.settings = true;
                }
                for (chart, label) in [(Chart::Treemap, "Treemap"), (Chart::Rings, "Rings")] {
                    if ui.selectable_label(app.chart == chart, label).clicked() {
                        act.chart = Some(chart);
                    }
                }
                if ui
                    .add_enabled(
                        super::actions::rescan_enabled(busy, modal),
                        egui::Button::new("Rescan (F5)"),
                    )
                    .clicked()
                {
                    act.rescan = true;
                }
            });
        });
        if let Some(err) = &app.save_error {
            ui.colored_label(ui.visuals().warn_fg_color, err);
        }
    });

    egui::Panel::bottom("results_status").show(ui, |ui| {
        ui.horizontal(|ui| match (&app.scan, &state) {
            (None, _) => {
                if app.is_starting() {
                    ui.label("Starting…");
                }
            }
            (Some(h), Some(ScanState::Running)) => {
                let items = h.progress.items.load(Ordering::Relaxed);
                let bytes = h.progress.bytes.load(Ordering::Relaxed);
                let cur = h
                    .progress
                    .current
                    .lock()
                    .map(|c| c.clone())
                    .unwrap_or_default();
                if ui.button("Cancel").clicked() {
                    act.cancel = true;
                }
                ui.add(
                    egui::Label::new(format!(
                        "{} items · {} · {}",
                        format_count(items),
                        format_size(bytes),
                        cur.display()
                    ))
                    .truncate(),
                );
            }
            (Some(_), Some(s)) => {
                let mut text = format!(
                    "{} · {} items",
                    format_size(totals.0),
                    format_count(totals.1)
                );
                match s {
                    ScanState::Cancelled => text.push_str(" · scan cancelled, partial results"),
                    ScanState::Failed(e) => text.push_str(&format!(" · scan failed: {e}")),
                    _ => {}
                }
                ui.label(text);
                let n = app.results.unreadable_count;
                if n > 0 && ui.link(unreadable_text(n)).clicked() {
                    act.open_unreadable = true;
                }
            }
            _ => {}
        });
    });

    egui::Panel::left("results_tree")
        .resizable(true)
        .default_size(560.0)
        .show(ui, |ui| tree_table(app, ui, &mut act));

    let mut chart_event = None;
    egui::CentralPanel::default().show(ui, |ui| {
        chart_event = super::charts::chart_ui(app, ui);
    });

    unreadable_window(app, &ctx);
    super::actions::dialogs_ui(app, &ctx);

    if running || app.is_starting() {
        ctx.request_repaint_after(Duration::from_millis(100));
    }

    // Phase 3: apply the frame's actions.
    if act.cancel
        && let Some(h) = &app.scan
    {
        h.cancel();
    }
    if let Some(chart) = act.chart
        && (app.chart != chart || app.settings.default_chart != chart)
    {
        app.chart = chart;
        app.settings.default_chart = chart;
        app.save_settings();
    }
    if let Some(id) = act.toggle {
        if !app.results.expanded.remove(&id) {
            app.results.expanded.insert(id);
        }
        app.results.expanded_version += 1;
    }
    match chart_event {
        // A chart click selects what was clicked (and drills into folders).
        Some(ChartEvent::DrillInto(id)) => {
            act.select = Some(id);
            act.focus = Some(id);
        }
        Some(ChartEvent::Select(id)) => act.select = Some(id),
        Some(ChartEvent::Up) => {
            act.focus = focus_parent(app);
        }
        Some(ChartEvent::ContextMenu(id)) => {
            let pos = ctx.input(|i| i.pointer.latest_pos()).unwrap_or_default();
            super::actions::open_menu(app, id, pos);
        }
        None => {}
    }
    if let Some(id) = act.select {
        app.actions.selected = Some(id);
    }
    if let Some((id, pos)) = act.menu {
        super::actions::open_menu(app, id, pos);
    }
    if act.settings {
        app.actions.settings_open = !app.actions.settings_open;
    }
    if let Some(id) = act.focus {
        set_focus(app, id);
    }
    if act.open_unreadable
        && let Some(h) = &app.scan
    {
        let tree = h.read_tree();
        app.results.unreadable_window = Some(
            app.results
                .unreadable_ids
                .iter()
                .filter(|&&id| tree.is_live(id))
                .map(|&id| tree.path_of(id).display().to_string())
                .collect(),
        );
    }
    if act.back {
        app.back_to_locations();
    } else if act.rescan {
        let p = app.scan_path.clone();
        // Remember expanded folders by name so the same ones reopen after the rescan.
        let pending: Vec<Vec<String>> = app.scan.as_ref().map_or_else(Vec::new, |h| {
            let tree = h.read_tree();
            app.results
                .expanded
                .iter()
                .filter(|&&id| id != Tree::ROOT && tree.is_live(id))
                .map(|&id| {
                    let ids = breadcrumb_ids(&tree, id);
                    ids[1..]
                        .iter()
                        .map(|&n| tree.node(n).name.to_string())
                        .collect()
                })
                .collect()
        });
        app.begin_scan(p);
        app.results.pending_expand = pending;
    }
}

fn unreadable_window(app: &mut WinBaobabApp, ctx: &egui::Context) {
    let mut open = app.results.unreadable_window.is_some();
    if let Some(paths) = &app.results.unreadable_window {
        egui::Window::new(unreadable_text(paths.len()))
            .id(egui::Id::unique("unreadable_win"))
            .open(&mut open)
            .default_size([520.0, 320.0])
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for p in paths {
                        ui.add(egui::Label::new(p).truncate());
                    }
                });
            });
    }
    if !open {
        app.results.unreadable_window = None;
    }
}

/// Scroll-area id for the tree list, salted with the scan generation.
///
/// egui keeps a ScrollArea's offset in its memory keyed by id. With a fixed id
/// the offset from the previous scan survived (the area is not drawn while the
/// new scan is still starting, so it was never clamped), and the next scan's
/// list opened scrolled down. A fresh id per scan always starts at the top.
fn tree_scroll_id(scan_gen: u64) -> egui::IdSalt {
    egui::IdSalt::new(("results_tree_scroll", scan_gen))
}

fn tree_table(app: &mut WinBaobabApp, ui: &mut egui::Ui, act: &mut Actions) {
    let row_h = ui.text_style_height(&egui::TextStyle::Body) + 4.0;
    let width = ui.available_width();
    let name_w = (width - SIZE_W - CONTENTS_W - MODIFIED_W - BAR_W - 16.0).max(NAME_MIN_W);

    // Header row.
    let (hrect, _) = ui.allocate_exact_size(vec2(width, row_h), Sense::hover());
    let cols = columns(hrect, name_w);
    let weak = ui.visuals().weak_text_color();
    let font = FontId::proportional(13.0);
    let p = ui.painter();
    p.text(
        cols.name.left_center(),
        Align2::LEFT_CENTER,
        "Name",
        font.clone(),
        weak,
    );
    p.text(
        cols.size.right_center(),
        Align2::RIGHT_CENTER,
        "Size",
        font.clone(),
        weak,
    );
    p.text(
        cols.contents.right_center(),
        Align2::RIGHT_CENTER,
        "Contents",
        font.clone(),
        weak,
    );
    p.text(
        cols.modified.left_center(),
        Align2::LEFT_CENTER,
        "Modified",
        font.clone(),
        weak,
    );
    p.text(
        cols.bar.left_center(),
        Align2::LEFT_CENTER,
        "Share",
        font,
        weak,
    );
    ui.separator();

    app.results.on_screen.clear();
    let Some(h) = &app.scan else { return };
    let total = app.results.rows.len();
    egui::ScrollArea::vertical()
        .id_salt(tree_scroll_id(app.scan_gen))
        .auto_shrink([false, false])
        .show_rows(ui, row_h, total, |ui, range| {
            // Phase 2: row data for just the visible range, under a short read lock.
            let views = {
                let tree = h.read_tree();
                app.results.rows[range.clone()]
                    .iter()
                    .filter(|r| tree.is_live(r.id))
                    .map(|r| {
                        let n = tree.node(r.id);
                        RowView {
                            id: r.id,
                            depth: r.depth,
                            name: if r.id == Tree::ROOT {
                                tree.root_path.display().to_string()
                            } else {
                                n.name.to_string()
                            },
                            kind: n.kind,
                            size: n.size,
                            items: n.items,
                            modified: n.modified.and_then(local_datetime_string),
                            parent_size: n.parent.map_or(n.size, |p| tree.node(p).size),
                            expandable: n.kind == NodeKind::Dir && !n.children.is_empty(),
                            expanded: app.results.expanded.contains(&r.id),
                            focus_target: if n.kind == NodeKind::Dir {
                                r.id
                            } else {
                                n.parent.unwrap_or(Tree::ROOT)
                            },
                        }
                    })
                    .collect::<Vec<_>>()
            };
            app.results.on_screen = views.iter().map(|v| v.id).collect();
            for v in &views {
                draw_row(ui, v, (app.focus, app.actions.selected), row_h, name_w, act);
            }
        });
}

struct Cols {
    name: egui::Rect,
    size: egui::Rect,
    contents: egui::Rect,
    modified: egui::Rect,
    bar: egui::Rect,
}

fn columns(row: egui::Rect, name_w: f32) -> Cols {
    let mut x = row.left() + 4.0;
    let mut next = |w: f32| {
        let r = egui::Rect::from_min_size(egui::pos2(x, row.top()), vec2(w, row.height()));
        x += w + 4.0;
        r
    };
    Cols {
        name: next(name_w),
        size: next(SIZE_W),
        contents: next(CONTENTS_W),
        modified: next(MODIFIED_W),
        bar: next(BAR_W),
    }
}

fn draw_row(
    ui: &mut egui::Ui,
    v: &RowView,
    (focus, selected): (NodeId, Option<NodeId>),
    row_h: f32,
    name_w: f32,
    act: &mut Actions,
) {
    // One rect per row: it is both what gets painted and what takes input.
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::click());
    let pointer = ui.input(|i| i.pointer.interact_pos());
    let hovered = pointer.is_some_and(|p| rect.contains(p)) && ui.rect_contains_pointer(rect);
    let cols = columns(rect, name_w);
    let visuals = ui.visuals().clone();
    if v.id == focus {
        ui.painter()
            .rect_filled(rect, 2.0, visuals.selection.bg_fill.linear_multiply(0.5));
    } else if hovered {
        ui.painter().rect_filled(
            rect,
            2.0,
            visuals.widgets.hovered.bg_fill.linear_multiply(0.4),
        );
    }
    if selected == Some(v.id) {
        ui.painter().rect_stroke(
            rect,
            2.0,
            egui::Stroke::new(1.5, visuals.selection.stroke.color),
            egui::StrokeKind::Inside,
        );
    }
    let text_col = visuals.text_color();
    let font = FontId::proportional(13.0);
    let p = ui.painter();

    // Name: indent, arrow for expandable dirs, then the name (clipped to its column).
    let indent = v.depth as f32 * INDENT;
    let arrow = egui::Rect::from_min_size(
        egui::pos2(cols.name.left() + indent, rect.top()),
        vec2(INDENT, row_h),
    );
    let np = p.with_clip_rect(cols.name);
    if v.expandable {
        np.text(
            arrow.center(),
            Align2::CENTER_CENTER,
            if v.expanded { "▼" } else { "▶" },
            FontId::proportional(10.0),
            visuals.weak_text_color(),
        );
    }
    let label_col = if v.kind == NodeKind::Unseen {
        visuals.weak_text_color()
    } else {
        text_col
    };
    np.text(
        egui::pos2(arrow.right() + 2.0, rect.center().y),
        Align2::LEFT_CENTER,
        &v.name,
        font.clone(),
        label_col,
    );

    p.text(
        cols.size.right_center(),
        Align2::RIGHT_CENTER,
        format_size(v.size),
        font.clone(),
        text_col,
    );
    if v.kind == NodeKind::Dir {
        p.text(
            cols.contents.right_center(),
            Align2::RIGHT_CENTER,
            format_count(v.items),
            font.clone(),
            text_col,
        );
    }
    if let Some(m) = &v.modified {
        p.text(
            cols.modified.left_center(),
            Align2::LEFT_CENTER,
            m,
            font.clone(),
            text_col,
        );
    }
    // Share of parent.
    let frac = if v.parent_size == 0 {
        0.0
    } else {
        (v.size as f32 / v.parent_size as f32).clamp(0.0, 1.0)
    };
    let track = cols.bar.shrink2(vec2(0.0, 4.0));
    p.rect_filled(track, 2.0, visuals.widgets.noninteractive.bg_fill);
    let mut fill = track;
    fill.set_width(track.width() * frac);
    p.rect_filled(fill, 2.0, Color32::from_rgb(70, 130, 200));
    p.text(
        track.center(),
        Align2::CENTER_CENTER,
        format!("{:.0}%", frac * 100.0),
        FontId::proportional(11.0),
        text_col,
    );

    if resp.secondary_clicked() {
        act.select = Some(v.id);
        act.menu = resp.interact_pointer_pos().map(|p| (v.id, p));
    }
    if resp.clicked() {
        act.select = Some(v.id);
        let on_arrow = v.expandable
            && resp
                .interact_pointer_pos()
                .is_some_and(|p| arrow.contains(p));
        if on_arrow {
            act.toggle = Some(v.id);
            return;
        }
        act.focus = Some(v.focus_target);
    }
    if resp.double_clicked() && v.expandable {
        act.toggle = Some(v.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn tree_scroll_id_differs_per_scan() {
        assert_ne!(tree_scroll_id(1), tree_scroll_id(2));
        assert_eq!(tree_scroll_id(3), tree_scroll_id(3));
    }

    /// root: a(dir: b(dir: f1 5), f2 50), f3 10, g(dir, empty)
    fn sample() -> (Tree, NodeId, NodeId, NodeId) {
        let mut t = Tree::new(PathBuf::from(r"C:\root"));
        let a = t.add_child(Tree::ROOT, "a", NodeKind::Dir, 0, None);
        let b = t.add_child(a, "b", NodeKind::Dir, 0, None);
        t.add_child(b, "f1", NodeKind::File, 5, None);
        t.add_child(a, "f2", NodeKind::File, 50, None);
        let f3 = t.add_child(Tree::ROOT, "f3", NodeKind::File, 10, None);
        t.add_child(Tree::ROOT, "g", NodeKind::Dir, 0, None);
        (t, a, b, f3)
    }

    fn ids(rows: &[FlatRow]) -> Vec<NodeId> {
        rows.iter().map(|r| r.id).collect()
    }

    #[test]
    fn breadcrumb_runs_root_to_focus() {
        let (t, a, b, _) = sample();
        assert_eq!(breadcrumb_ids(&t, Tree::ROOT), vec![Tree::ROOT]);
        assert_eq!(breadcrumb_ids(&t, b), vec![Tree::ROOT, a, b]);
    }

    #[test]
    fn flatten_only_root_when_collapsed() {
        let (t, ..) = sample();
        let rows = flatten_rows(&t, &HashSet::new());
        assert_eq!(ids(&rows), vec![Tree::ROOT]);
    }

    #[test]
    fn flatten_default_shows_root_children_by_size() {
        let (t, a, _, f3) = sample();
        let g = 6; // last added node
        let rows = flatten_rows(&t, &HashSet::from([Tree::ROOT]));
        assert_eq!(ids(&rows), vec![Tree::ROOT, a, f3, g]);
        assert_eq!(rows[1].depth, 1);
    }

    #[test]
    fn flatten_descends_expanded_dirs_in_order() {
        let (t, a, b, f3) = sample();
        let f2 = t.node(a).children[1];
        let f1 = t.node(b).children[0];
        let g = 6;
        let ex = HashSet::from([Tree::ROOT, a, b]);
        let rows = flatten_rows(&t, &ex);
        assert_eq!(ids(&rows), vec![Tree::ROOT, a, f2, b, f1, f3, g]);
        let depths: Vec<u32> = rows.iter().map(|r| r.depth).collect();
        assert_eq!(depths, vec![0, 1, 2, 2, 3, 1, 1]);
    }

    #[test]
    fn flatten_hides_removed_nodes_and_their_subtrees() {
        let (mut t, a, b, f3) = sample();
        t.remove_subtree(b);
        let rows = flatten_rows(&t, &HashSet::from([Tree::ROOT, a, b]));
        assert!(!ids(&rows).contains(&b));
        t.remove_subtree(a);
        let rows = flatten_rows(&t, &HashSet::from([Tree::ROOT, a, b]));
        assert_eq!(ids(&rows), vec![Tree::ROOT, f3, 6]);
    }

    #[test]
    fn parent_of_child_and_root() {
        let (t, a, b, _) = sample();
        assert_eq!(parent_of(&t, b), Some(a));
        assert_eq!(parent_of(&t, a), Some(Tree::ROOT));
        assert_eq!(parent_of(&t, Tree::ROOT), None);
    }

    #[test]
    fn find_by_names_follows_path() {
        let (t, a, b, _) = sample();
        let n = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(find_by_names(&t, &[]), Some(Tree::ROOT));
        assert_eq!(find_by_names(&t, &n(&["a", "b"])), Some(b));
        assert_eq!(find_by_names(&t, &n(&["a"])), Some(a));
        assert_eq!(find_by_names(&t, &n(&["a", "zz"])), None);
    }

    #[test]
    fn unreadable_text_singular_and_plural() {
        assert_eq!(
            unreadable_text(1),
            "1 folder couldn't be read — access denied"
        );
        assert_eq!(
            unreadable_text(3),
            "3 folders couldn't be read — access denied"
        );
    }

    #[test]
    fn default_state_expands_root() {
        assert!(ResultsState::default().expanded.contains(&Tree::ROOT));
    }
}
