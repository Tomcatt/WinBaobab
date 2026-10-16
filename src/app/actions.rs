//! Context menu, keyboard shortcuts and the delete flows (confirm dialogs,
//! background delete, settings window).
//!
//! The decisions (what may be deleted, which dialog to show, what a delete
//! outcome does to the tree) are plain functions so they are tested without
//! egui; the egui code below only draws and routes input.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::sync::{Arc, PoisonError, RwLock};

use eframe::egui::{self, Pos2};

use super::{Screen, WinBaobabApp};
use crate::delete::{DeleteError, can_delete, delete_permanently, recycle, recycle_block_reason};
use crate::drives::{DriveKind, location_kind, unseen_used};
use crate::explorer::open_in_explorer;
use crate::format::format_size;
use crate::model::{NodeId, NodeKind, Tree};
use crate::scan::{DirSource, ScanState, WinDirSource, graft_children, scan_detached};
use crate::settings::Settings;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Recycle,
    Permanent,
}

/// Everything about the item being deleted, captured when the delete is requested.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Target {
    pub id: NodeId,
    pub parent: NodeId,
    pub name: String,
    pub path: PathBuf,
    pub size: u64,
    pub items: u64,
    pub mode: Mode,
    pub network: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Dialog {
    ConfirmRecycle(Target),
    ConfirmPermanent { target: Target, dont_show: bool },
    Message { title: String, text: String },
}

/// Result of a delete job, applied to the tree by the worker.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Outcome {
    Deleted,
    /// `refreshed`: the parent folder was re-read, so old node ids under it are gone.
    Failed {
        title: String,
        text: String,
        refreshed: bool,
    },
}

struct Job {
    rx: Receiver<(Target, Outcome)>,
    name: String,
    /// Set while the worker re-reads a folder after a failed delete.
    refreshing: Arc<AtomicBool>,
}

/// Nodes per write-lock hold when a refreshed folder is grafted into the tree.
const GRAFT_CHUNK: usize = 2000;

#[derive(Default)]
pub(crate) struct ActionsState {
    /// Row / chart item that Delete and Shift+Delete act on.
    pub selected: Option<NodeId>,
    /// Open context menu: item and where to draw it.
    pub menu: Option<(NodeId, Pos2)>,
    pub dialog: Option<Dialog>,
    pub settings_open: bool,
    job: Option<Job>,
}

impl ActionsState {
    /// A new scan replaces the tree, so everything tied to the old one goes.
    pub(crate) fn on_new_scan(&mut self) {
        self.selected = None;
        self.menu = None;
        self.dialog = None;
        self.job = None;
    }

    pub(crate) fn is_modal_open(&self) -> bool {
        self.dialog.is_some() || self.job.is_some()
    }
}

// ---------------------------------------------------------------- texts

fn items_text(n: u64) -> String {
    if n == 1 {
        "1 item".to_string()
    } else {
        format!("{} items", crate::format::format_count(n))
    }
}

fn describe(t: &Target) -> String {
    format!(
        "{} ({}, {})",
        t.name,
        format_size(t.size),
        items_text(t.items)
    )
}

pub(crate) fn recycle_prompt(t: &Target) -> String {
    format!(
        "Move {} ({}, {}) to the Recycle Bin?",
        t.name,
        format_size(t.size),
        items_text(t.items)
    )
}

pub(crate) fn permanent_body(t: &Target) -> String {
    let mut s = format!(
        "{} will be deleted permanently. This skips the Recycle Bin and cannot be undone.",
        describe(t)
    );
    if t.network {
        s.push_str(
            " Whether it can be recovered depends on the NAS (for example its recycle folder or snapshots).",
        );
    }
    s
}

pub(crate) fn refused_text(name: &str) -> String {
    format!("Windows couldn't move {name} to the Recycle Bin, so nothing was deleted.")
}

// ------------------------------------------------------- delete decisions

pub(crate) enum Plan {
    /// Not allowed (or nothing to do); change nothing.
    Ignore,
    Confirm(Dialog),
    Run(Target),
}

fn target_of(tree: &Tree, id: NodeId, mode: Mode) -> Target {
    let n = tree.node(id);
    let path = tree.path_of(id);
    let network = location_kind(&path) == DriveKind::Network;
    Target {
        id,
        parent: n.parent.unwrap_or(Tree::ROOT),
        name: n.name.to_string(),
        path,
        size: n.size,
        // A file counts as one item; a folder as its contents.
        items: if n.kind == NodeKind::Dir { n.items } else { 1 },
        mode,
        network,
    }
}

/// Decides what a delete request does. Recycle is never offered on network or
/// removable locations; permanent delete skips its warning when `warn` is off.
pub(crate) fn plan_delete(
    tree: &Tree,
    id: NodeId,
    scan_running: bool,
    mode: Mode,
    warn: bool,
) -> Plan {
    if !tree.is_live(id) || !can_delete(tree, id, scan_running) {
        return Plan::Ignore;
    }
    let target = target_of(tree, id, mode);
    match mode {
        Mode::Recycle => {
            if recycle_block_reason(location_kind(&target.path)).is_some() {
                Plan::Ignore
            } else {
                Plan::Confirm(Dialog::ConfirmRecycle(target))
            }
        }
        Mode::Permanent => {
            if warn {
                Plan::Confirm(Dialog::ConfirmPermanent {
                    target,
                    dont_show: false,
                })
            } else {
                Plan::Run(target)
            }
        }
    }
}

/// "Don't show this warning again": sets the flag and says whether it changed.
pub(crate) fn set_dont_show(settings: &mut Settings, dont_show: bool) -> bool {
    let warn = !dont_show;
    let changed = settings.warn_permanent_delete != warn;
    settings.warn_permanent_delete = warn;
    changed
}

/// "Don't show this warning again" takes effect only when the dialog is
/// confirmed. Returns the target to delete and whether settings need saving.
pub(crate) fn finish_dialog(
    settings: &mut Settings,
    dialog: &Dialog,
    confirmed: bool,
) -> (Option<Target>, bool) {
    if !confirmed {
        return (None, false);
    }
    match dialog {
        Dialog::ConfirmRecycle(t) => (Some(t.clone()), false),
        Dialog::ConfirmPermanent { target, dont_show } => {
            let save = *dont_show && set_dont_show(settings, true);
            (Some(target.clone()), save)
        }
        Dialog::Message { .. } => (None, false),
    }
}

/// Runs a delete and applies the result to the tree.
/// - success: the subtree is removed (no rescan);
/// - `NotRecyclable`: nothing changes, the refused message is returned;
/// - `Io`: the item's parent is re-read so sizes match the disk again
///   (re-adding the Unseen block when that parent is the scan root). The
///   folder is enumerated without any tree lock, then grafted in chunks, so
///   readers are never blocked for the length of a directory walk.
pub(crate) fn execute(
    tree: &RwLock<Tree>,
    t: &Target,
    op: impl FnOnce(&Path) -> Result<(), DeleteError>,
    source: &dyn DirSource,
    drive_used: impl FnOnce() -> Option<u64>,
    refreshing: &AtomicBool,
) -> Outcome {
    let write = || tree.write().unwrap_or_else(PoisonError::into_inner);
    match op(&t.path) {
        Ok(()) => {
            write().remove_subtree(t.id);
            Outcome::Deleted
        }
        Err(DeleteError::NotRecyclable) => Outcome::Failed {
            title: "Couldn't move to the Recycle Bin".into(),
            text: refused_text(&t.name),
            refreshed: false,
        },
        Err(DeleteError::Io(e)) => {
            refreshing.store(true, Ordering::Relaxed);
            let used = if t.parent == Tree::ROOT {
                drive_used()
            } else {
                None
            };
            let dir = {
                let r = tree.read().unwrap_or_else(PoisonError::into_inner);
                r.is_live(t.parent).then(|| r.path_of(t.parent))
            };
            let mut refreshed = false;
            if let Some(dir) = dir {
                let fresh = scan_detached(dir, source);
                refreshed = graft_children(tree, t.parent, &fresh, GRAFT_CHUNK);
                if refreshed
                    && t.parent == Tree::ROOT
                    && let Some(u) = used
                {
                    write().set_unseen(u);
                }
            }
            Outcome::Failed {
                title: "Couldn't delete".into(),
                text: format!(
                    "{} could not be deleted.\n{e}\nThe sizes shown were refreshed.",
                    t.name
                ),
                refreshed,
            }
        }
    }
}

// ------------------------------------------------------- selection rules

/// Delete may act only on what the user can see highlighted: a live node
/// whose tree row is on screen (`on_screen`: the rows actually drawn, not
/// ones scrolled away or collapsed) or which the chart is drawing
/// (`in_chart`).
pub(crate) fn selection_visible(
    tree: &Tree,
    sel: NodeId,
    on_screen: &[NodeId],
    in_chart: bool,
) -> bool {
    tree.is_live(sel) && (in_chart || on_screen.contains(&sel))
}

/// Whether a selection survives a focus change to `focus`: only when it is
/// that folder or lies inside it.
pub(crate) fn selection_survives_focus(tree: &Tree, sel: NodeId, focus: NodeId) -> bool {
    if !tree.is_live(sel) || !tree.is_live(focus) {
        return false;
    }
    let mut cur = Some(sel);
    while let Some(n) = cur {
        if n == focus {
            return true;
        }
        cur = tree.node(n).parent;
    }
    false
}

/// The selection, if the user can currently see it.
fn visible_selection(app: &WinBaobabApp) -> Option<NodeId> {
    let sel = app.actions.selected?;
    let h = app.scan.as_ref()?;
    let in_chart = app.results.chart.shows(sel, app.focus);
    selection_visible(&h.read_tree(), sel, &app.results.on_screen, in_chart).then_some(sel)
}

/// Drops a selection that has scrolled out of view (collapsed, refocused, removed).
pub(crate) fn prune_selection(app: &mut WinBaobabApp) {
    if app.actions.selected.is_some() && visible_selection(app).is_none() {
        app.actions.selected = None;
    }
}

/// Paths (names below the root) of expanded folders whose nodes no longer
/// exist, so they can be re-expanded once a refresh re-creates them.
pub(crate) fn stale_expanded_paths(tree: &Tree, expanded: &HashSet<NodeId>) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = expanded
        .iter()
        .filter(|&&id| id != Tree::ROOT && !tree.is_live(id))
        .map(|&id| {
            super::results::breadcrumb_ids(tree, id)[1..]
                .iter()
                .map(|&n| tree.node(n).name.to_string())
                .collect()
        })
        .collect();
    out.sort();
    out
}

/// F5 rescans only when nothing else owns the keyboard or the screen.
pub(crate) fn rescan_key_allowed(
    scan_busy: bool,
    modal: bool,
    typing: bool,
    pressed: bool,
) -> bool {
    pressed && rescan_enabled(scan_busy, modal) && !typing
}

/// The Rescan button / F5 is available when no scan, dialog or job is active.
pub(crate) fn rescan_enabled(scan_busy: bool, modal: bool) -> bool {
    !scan_busy && !modal
}

// --------------------------------------------------------------- keyboard

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Keys {
    pub delete: bool,
    pub backspace: bool,
    pub left: bool,
    pub l: bool,
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
    /// A text field has focus: only Ctrl+L still works.
    pub typing: bool,
    /// A dialog is open or a delete job is running: no shortcut works.
    pub modal: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyCmd {
    Recycle,
    Permanent,
    Up,
    FocusPath,
}

pub(crate) fn classify_keys(k: &Keys, on_results: bool) -> Option<KeyCmd> {
    if k.modal {
        return None;
    }
    if k.l && k.ctrl && !k.alt {
        return Some(KeyCmd::FocusPath);
    }
    if k.typing || !on_results || k.ctrl {
        return None;
    }
    if k.delete && !k.alt {
        return Some(if k.shift {
            KeyCmd::Permanent
        } else {
            KeyCmd::Recycle
        });
    }
    if k.backspace && !k.alt && !k.shift {
        return Some(KeyCmd::Up);
    }
    if k.left && k.alt && !k.shift {
        return Some(KeyCmd::Up);
    }
    None
}

fn scan_busy(app: &WinBaobabApp) -> bool {
    app.is_starting()
        || app
            .scan
            .as_ref()
            .is_some_and(|h| matches!(h.state(), ScanState::Running))
}

pub(crate) fn handle_keys(app: &mut WinBaobabApp, ctx: &egui::Context) {
    // Never call other ctx methods inside ctx.input: it holds the context lock.
    let typing = ctx.egui_wants_keyboard_input();
    let modal = app.actions.is_modal_open();
    let keys = ctx.input(|i| Keys {
        delete: i.key_pressed(egui::Key::Delete),
        backspace: i.key_pressed(egui::Key::Backspace),
        left: i.key_pressed(egui::Key::ArrowLeft),
        l: i.key_pressed(egui::Key::L),
        shift: i.modifiers.shift,
        alt: i.modifiers.alt,
        ctrl: i.modifiers.ctrl,
        typing,
        modal,
    });
    match classify_keys(&keys, app.screen == Screen::Results) {
        Some(KeyCmd::FocusPath) => {
            app.locations.focus_path_box = true;
            if app.screen == Screen::Results {
                app.back_to_locations();
            }
        }
        Some(KeyCmd::Up) => {
            if let Some(p) = super::results::focus_parent(app) {
                super::results::set_focus(app, p);
            }
        }
        Some(KeyCmd::Recycle) => delete_selected(app, Mode::Recycle),
        Some(KeyCmd::Permanent) => delete_selected(app, Mode::Permanent),
        None => {}
    }
}

fn delete_selected(app: &mut WinBaobabApp, mode: Mode) {
    if let Some(id) = visible_selection(app) {
        begin_delete(app, id, mode);
    }
}

// ---------------------------------------------------------------- flows

/// Starts the flow for deleting `id`: a confirm dialog, or straight to work.
pub(crate) fn begin_delete(app: &mut WinBaobabApp, id: NodeId, mode: Mode) {
    if app.actions.is_modal_open() {
        return;
    }
    let running = scan_busy(app);
    let warn = app.settings.warn_permanent_delete;
    let Some(h) = &app.scan else { return };
    let plan = plan_delete(&h.read_tree(), id, running, mode, warn);
    match plan {
        Plan::Ignore => {}
        Plan::Confirm(d) => app.actions.dialog = Some(d),
        Plan::Run(t) => start_job(app, t),
    }
}

/// Runs the delete on a worker thread; the result is picked up by `dialogs_ui`.
fn start_job(app: &mut WinBaobabApp, t: Target) {
    let Some(h) = &app.scan else { return };
    let tree = h.tree.clone();
    let ctx = app.repaint_ctx.clone();
    let (tx, rx) = channel();
    let refreshing = Arc::new(AtomicBool::new(false));
    app.actions.job = Some(Job {
        rx,
        name: t.name.clone(),
        refreshing: refreshing.clone(),
    });
    std::thread::spawn(move || {
        let op = |p: &Path| match t.mode {
            Mode::Recycle => recycle(p),
            Mode::Permanent => delete_permanently(p),
        };
        let root = tree
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .root_path
            .clone();
        // Same rule as at scan start: only a local fixed volume root.
        let used = || unseen_used(&root, None);
        let outcome = execute(&tree, &t, op, &WinDirSource, used, &refreshing);
        let _ = tx.send((t, outcome));
        if let Some(c) = ctx {
            c.request_repaint();
        }
    });
}

fn poll_job(app: &mut WinBaobabApp) {
    let Some(job) = &app.actions.job else { return };
    let result = match job.rx.try_recv() {
        Ok(r) => Some(Some(r)),
        Err(TryRecvError::Disconnected) => Some(None),
        Err(TryRecvError::Empty) => None,
    };
    let Some(result) = result else { return };
    app.actions.job = None;
    match result {
        Some((t, outcome)) => {
            if let Some(h) = &app.scan {
                let tree = h.read_tree();
                if app.actions.selected == Some(t.id) && !tree.is_live(t.id) {
                    app.actions.selected = None;
                }
                // Keep the view somewhere valid: the nearest live ancestor.
                if !tree.is_live(app.focus) && tree.is_live(t.parent) {
                    app.focus = t.parent;
                }
                // A refreshed folder got new node ids: reopen what was expanded in it.
                if matches!(
                    outcome,
                    Outcome::Failed {
                        refreshed: true,
                        ..
                    }
                ) {
                    let stale = stale_expanded_paths(&tree, &app.results.expanded);
                    if !stale.is_empty() {
                        app.results.expanded.retain(|&id| tree.is_live(id));
                        app.results.pending_expand.extend(stale);
                        app.results.expanded_version += 1;
                    }
                }
            }
            if let Outcome::Failed { title, text, .. } = outcome {
                app.actions.dialog = Some(Dialog::Message { title, text });
            }
        }
        None => {
            app.actions.dialog = Some(Dialog::Message {
                title: "Couldn't delete".into(),
                text: "The delete stopped unexpectedly. Rescan to refresh the sizes.".into(),
            });
        }
    }
}

// ------------------------------------------------------------ context menu

/// What the context menu offers for one item.
#[derive(Debug, PartialEq)]
pub(crate) struct MenuInfo {
    pub path: PathBuf,
    pub is_dir: bool,
    pub has_path: bool,
    /// Why every delete is unavailable, if it is.
    pub delete_block: Option<&'static str>,
    /// Why only the Recycle Bin is unavailable, if it is.
    pub recycle_block: Option<&'static str>,
}

pub(crate) fn menu_info(
    tree: &Tree,
    id: NodeId,
    scan_running: bool,
    job_running: bool,
) -> MenuInfo {
    let path = tree.path_of(id);
    let delete_block = if job_running {
        Some("Another delete is in progress.")
    } else if scan_running {
        Some("Wait for the scan to finish.")
    } else if !can_delete(tree, id, false) {
        Some("This item can't be deleted.")
    } else {
        None
    };
    MenuInfo {
        is_dir: tree.node(id).kind == NodeKind::Dir,
        has_path: tree.node(id).kind != NodeKind::Unseen,
        recycle_block: recycle_block_reason(location_kind(&path)),
        path,
        delete_block,
    }
}

/// Opens the context menu for `id` at `pos`. Ignores nodes that are no longer
/// live (the chart layout can be a little stale).
pub(crate) fn open_menu(app: &mut WinBaobabApp, id: NodeId, pos: Pos2) {
    let live = app.scan.as_ref().is_some_and(|h| h.read_tree().is_live(id));
    if live {
        app.actions.selected = Some(id);
        app.actions.menu = Some((id, pos));
    }
}

/// Menu contents for `id`; closes the menu when an item is chosen.
pub(crate) fn context_menu(app: &mut WinBaobabApp, ui: &mut egui::Ui, id: NodeId) {
    let running = scan_busy(app);
    let job = app.actions.job.is_some();
    let Some(info) = app
        .scan
        .as_ref()
        .map(|h| menu_info(&h.read_tree(), id, running, job))
    else {
        return;
    };
    let mut close = false;

    let explorer = ui.add_enabled(info.has_path, egui::Button::new("Open in Explorer"));
    if explorer.clicked() {
        open_in_explorer(&info.path, info.is_dir);
        close = true;
    }
    let copy = ui.add_enabled(info.has_path, egui::Button::new("Copy path"));
    if copy.clicked() {
        ui.ctx().copy_text(info.path.display().to_string());
        close = true;
    }
    ui.separator();

    let recycle_reason = info.delete_block.or(info.recycle_block);
    let recycle_btn = ui.add_enabled(
        recycle_reason.is_none(),
        egui::Button::new("Move to Recycle Bin").shortcut_text("Delete"),
    );
    let recycle_btn = match recycle_reason {
        Some(r) => recycle_btn.on_disabled_hover_text(r),
        None => recycle_btn,
    };
    if recycle_btn.clicked() {
        close = true;
        begin_delete(app, id, Mode::Recycle);
    }
    let perm_btn = ui.add_enabled(
        info.delete_block.is_none(),
        egui::Button::new("Delete permanently").shortcut_text("Shift+Delete"),
    );
    let perm_btn = match info.delete_block {
        Some(r) => perm_btn.on_disabled_hover_text(r),
        None => perm_btn,
    };
    if perm_btn.clicked() {
        close = true;
        begin_delete(app, id, Mode::Permanent);
    }
    if close {
        app.actions.menu = None;
    }
}

fn menu_popup(app: &mut WinBaobabApp, ctx: &egui::Context) {
    let Some((id, pos)) = app.actions.menu else {
        return;
    };
    let live = app.scan.as_ref().is_some_and(|h| h.read_tree().is_live(id));
    if !live {
        app.actions.menu = None;
        return;
    }
    let area = egui::Area::new(egui::Id::unique("item_context_menu"))
        .order(egui::Order::Foreground)
        .fixed_pos(pos)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_min_width(210.0);
                context_menu(app, ui, id);
            });
        });
    let outside_press = ctx.input(|i| {
        i.pointer.any_pressed()
            && i.pointer
                .interact_pos()
                .is_some_and(|p| !area.response.rect.contains(p))
    });
    if outside_press || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        app.actions.menu = None;
    }
}

// ----------------------------------------------------------------- dialogs

/// Menu, delete dialogs, the delete-in-progress notice and the settings window.
pub(crate) fn dialogs_ui(app: &mut WinBaobabApp, ctx: &egui::Context) {
    poll_job(app);
    menu_popup(app, ctx);
    dialog_ui(app, ctx);
    working_ui(app, ctx);
    settings_window(app, ctx);
}

fn dialog_ui(app: &mut WinBaobabApp, ctx: &egui::Context) {
    let Some(mut dialog) = app.actions.dialog.take() else {
        return;
    };
    let mut keep = true;
    let mut confirmed = false;
    let modal = egui::Modal::new(egui::Id::unique("delete_dialog")).show(ctx, |ui| {
        ui.set_max_width(440.0);
        match &mut dialog {
            Dialog::ConfirmRecycle(t) => {
                ui.heading("Move to the Recycle Bin?");
                ui.label(recycle_prompt(t));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Move to Recycle Bin").clicked() {
                        confirmed = true;
                        keep = false;
                    }
                    if ui.button("Cancel").clicked() {
                        keep = false;
                    }
                });
            }
            Dialog::ConfirmPermanent { target, dont_show } => {
                ui.heading("Delete permanently?");
                ui.label(permanent_body(target));
                ui.add_space(4.0);
                ui.checkbox(dont_show, "Don't show this warning again");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Delete permanently").clicked() {
                        confirmed = true;
                        keep = false;
                    }
                    if ui.button("Cancel").clicked() {
                        keep = false;
                    }
                });
            }
            Dialog::Message { title, text } => {
                ui.heading(title.as_str());
                ui.label(text.as_str());
                ui.add_space(8.0);
                if ui.button("OK").clicked() {
                    keep = false;
                }
            }
        }
    });
    if modal.should_close() {
        keep = false;
    }
    if keep {
        app.actions.dialog = Some(dialog);
        return;
    }
    let (run, save) = finish_dialog(&mut app.settings, &dialog, confirmed);
    if save {
        app.save_settings();
    }
    if let Some(t) = run {
        start_job(app, t);
    }
}

fn working_ui(app: &mut WinBaobabApp, ctx: &egui::Context) {
    let Some(job) = &app.actions.job else { return };
    egui::Modal::new(egui::Id::unique("delete_working")).show(ctx, |ui| {
        ui.horizontal(|ui| {
            ui.spinner();
            let verb = if job.refreshing.load(Ordering::Relaxed) {
                "Refreshing"
            } else {
                "Deleting"
            };
            ui.label(format!("{verb} {}…", job.name));
        });
    });
}

fn settings_window(app: &mut WinBaobabApp, ctx: &egui::Context) {
    let mut open = app.actions.settings_open;
    if !open {
        return;
    }
    let mut warn = app.settings.warn_permanent_delete;
    egui::Window::new("Settings")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .show(ctx, |ui| {
            ui.checkbox(&mut warn, "Warn before permanent delete");
        });
    app.actions.settings_open = open;
    if warn != app.settings.warn_permanent_delete {
        app.settings.warn_permanent_delete = warn;
        app.save_settings();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::winfs::RawEntry;
    use std::collections::HashMap;
    use std::io;
    use std::time::SystemTime;

    fn raw(name: &str, is_dir: bool, size: u64, id: u128) -> RawEntry {
        RawEntry {
            name: name.into(),
            is_dir,
            is_reparse: false,
            alloc_size: size,
            file_id: id,
            modified: SystemTime::UNIX_EPOCH,
        }
    }

    #[derive(Default)]
    struct Fake(HashMap<PathBuf, Vec<RawEntry>>);
    impl DirSource for Fake {
        fn read_dir(&self, dir: &Path) -> io::Result<Vec<RawEntry>> {
            self.0
                .get(dir)
                .cloned()
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
        }
    }

    /// C:\data: d (f 10), g 5, plus an Unseen block.
    fn sample(root: &str) -> (Tree, NodeId, NodeId, NodeId, NodeId) {
        let mut t = Tree::new(PathBuf::from(root));
        let d = t.add_child(Tree::ROOT, "d", NodeKind::Dir, 0, None);
        let f = t.add_child(d, "f", NodeKind::File, 10, None);
        let g = t.add_child(Tree::ROOT, "g", NodeKind::File, 5, None);
        t.set_unseen(100);
        let u = t
            .node(Tree::ROOT)
            .children
            .iter()
            .copied()
            .find(|&c| t.node(c).kind == NodeKind::Unseen)
            .unwrap();
        (t, d, f, g, u)
    }

    fn confirm_target(p: Plan) -> Dialog {
        match p {
            Plan::Confirm(d) => d,
            Plan::Ignore => panic!("ignored"),
            Plan::Run(_) => panic!("ran"),
        }
    }

    // ---- which items are deletable

    #[test]
    fn root_unseen_and_scanning_are_ignored() {
        let (t, _, f, _, u) = sample(r"C:\data");
        for mode in [Mode::Recycle, Mode::Permanent] {
            assert!(matches!(
                plan_delete(&t, Tree::ROOT, false, mode, true),
                Plan::Ignore
            ));
            assert!(matches!(
                plan_delete(&t, u, false, mode, true),
                Plan::Ignore
            ));
            assert!(matches!(plan_delete(&t, f, true, mode, true), Plan::Ignore));
        }
    }

    #[test]
    fn removed_and_out_of_range_ids_are_ignored() {
        let (mut t, _, f, _, _) = sample(r"C:\data");
        t.remove_subtree(f);
        assert!(matches!(
            plan_delete(&t, f, false, Mode::Recycle, true),
            Plan::Ignore
        ));
        assert!(matches!(
            plan_delete(&t, 9999, false, Mode::Recycle, true),
            Plan::Ignore
        ));
    }

    #[test]
    fn recycle_on_fixed_asks_to_confirm() {
        let (t, d, ..) = sample(r"C:\data");
        match confirm_target(plan_delete(&t, d, false, Mode::Recycle, true)) {
            Dialog::ConfirmRecycle(tg) => {
                assert_eq!(tg.path, PathBuf::from(r"C:\data\d"));
                assert_eq!(tg.parent, Tree::ROOT);
                assert_eq!(tg.size, 10);
                assert!(!tg.network);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn recycle_on_network_is_unavailable_but_permanent_is_not() {
        let (t, d, ..) = sample(r"\\srv\share\data");
        assert!(matches!(
            plan_delete(&t, d, false, Mode::Recycle, true),
            Plan::Ignore
        ));
        match confirm_target(plan_delete(&t, d, false, Mode::Permanent, true)) {
            Dialog::ConfirmPermanent { target, dont_show } => {
                assert!(target.network);
                assert!(!dont_show);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn permanent_skips_warning_when_disabled() {
        let (t, _, f, ..) = sample(r"C:\data");
        match plan_delete(&t, f, false, Mode::Permanent, false) {
            Plan::Run(tg) => assert_eq!(tg.id, f),
            _ => panic!("expected Run"),
        }
        assert!(matches!(
            plan_delete(&t, f, false, Mode::Permanent, true),
            Plan::Confirm(_)
        ));
    }

    #[test]
    fn recycle_always_confirms_even_when_warning_is_off() {
        let (t, _, f, ..) = sample(r"C:\data");
        assert!(matches!(
            plan_delete(&t, f, false, Mode::Recycle, false),
            Plan::Confirm(Dialog::ConfirmRecycle(_))
        ));
    }

    // ---- settings

    #[test]
    fn dont_show_again_clears_flag_and_reports_change() {
        let mut s = Settings::default();
        assert!(s.warn_permanent_delete);
        assert!(set_dont_show(&mut s, true));
        assert!(!s.warn_permanent_delete);
        assert!(!set_dont_show(&mut s, true));
        assert!(set_dont_show(&mut s, false));
        assert!(s.warn_permanent_delete);
    }

    #[test]
    fn dont_show_applies_only_when_confirmed() {
        let (t, _, f, ..) = sample(r"C:\data");
        let dialog = Dialog::ConfirmPermanent {
            target: target_of(&t, f, Mode::Permanent),
            dont_show: true,
        };
        let mut s = Settings::default();
        assert_eq!(finish_dialog(&mut s, &dialog, false), (None, false));
        assert!(s.warn_permanent_delete, "cancel leaves the setting alone");

        let (run, save) = finish_dialog(&mut s, &dialog, true);
        assert_eq!(run.map(|t| t.id), Some(f));
        assert!(save);
        assert!(!s.warn_permanent_delete);

        // Ticked off: confirming changes nothing and saves nothing.
        let mut s = Settings::default();
        let plain = Dialog::ConfirmPermanent {
            target: target_of(&t, f, Mode::Permanent),
            dont_show: false,
        };
        let (run, save) = finish_dialog(&mut s, &plain, true);
        assert!(run.is_some() && !save && s.warn_permanent_delete);
    }

    // ---- selection and expansion

    #[test]
    fn selection_visibility_rules() {
        let (mut t, d, f, g, _) = sample(r"C:\data");
        // f's row isn't on screen (collapsed or scrolled away) and the chart
        // isn't drawing it -> not visible, whatever the focus.
        let on_screen = [Tree::ROOT, d, g];
        assert!(!selection_visible(&t, f, &on_screen, false));
        // Its row is on screen -> visible.
        let open = [Tree::ROOT, d, f, g];
        assert!(selection_visible(&t, f, &open, false));
        // Drawn (and so highlighted) in the chart -> visible.
        assert!(selection_visible(&t, f, &on_screen, true));
        // Nothing on screen at all.
        assert!(!selection_visible(&t, f, &[], false));
        // Removed -> never visible.
        t.remove_subtree(f);
        assert!(!selection_visible(&t, f, &open, true));
    }

    #[test]
    fn focus_changes_keep_only_a_selection_at_or_under_the_focus() {
        let (mut t, d, f, g, _) = sample(r"C:\data");
        assert!(selection_survives_focus(&t, f, d));
        assert!(selection_survives_focus(&t, f, Tree::ROOT));
        assert!(selection_survives_focus(&t, d, d), "the new focus itself");
        assert!(!selection_survives_focus(&t, f, g), "other branch");
        assert!(!selection_survives_focus(&t, d, f), "above the new focus");
        t.remove_subtree(f);
        assert!(!selection_survives_focus(&t, f, d), "removed");
    }

    #[test]
    fn stale_expanded_folders_are_found_by_name() {
        let mut t = Tree::new(PathBuf::from(r"C:\data"));
        let a = t.add_child(Tree::ROOT, "a", NodeKind::Dir, 0, None);
        let b = t.add_child(a, "b", NodeKind::Dir, 0, None);
        let c = t.add_child(Tree::ROOT, "c", NodeKind::Dir, 0, None);
        let expanded = HashSet::from([Tree::ROOT, a, b, c]);
        assert!(stale_expanded_paths(&t, &expanded).is_empty());
        t.clear_children(a);
        assert_eq!(
            stale_expanded_paths(&t, &expanded),
            vec![vec!["a".to_string(), "b".to_string()]]
        );
    }

    // ---- copy

    fn tgt(name: &str, size: u64, items: u64, network: bool) -> Target {
        Target {
            id: 1,
            parent: 0,
            name: name.into(),
            path: PathBuf::from(name),
            size,
            items,
            mode: Mode::Permanent,
            network,
        }
    }

    #[test]
    fn dialog_copy_is_verbatim() {
        let t = tgt("Photos", 4_509_715_660, 1312, false);
        assert_eq!(
            recycle_prompt(&t),
            "Move Photos (4.2 GB, 1,312 items) to the Recycle Bin?"
        );
        assert_eq!(
            permanent_body(&t),
            "Photos (4.2 GB, 1,312 items) will be deleted permanently. This skips the Recycle Bin and cannot be undone."
        );
        let n = tgt("Photos", 2048, 5, true);
        assert!(permanent_body(&n).ends_with(
            " Whether it can be recovered depends on the NAS (for example its recycle folder or snapshots)."
        ));
        assert_eq!(
            refused_text("a.txt"),
            "Windows couldn't move a.txt to the Recycle Bin, so nothing was deleted."
        );
    }

    // ---- outcomes

    #[test]
    fn success_removes_subtree_without_rescan() {
        let (t, d, ..) = sample(r"C:\data");
        let before = t.node(Tree::ROOT).size;
        let tg = target_of(&t, d, Mode::Recycle);
        let tree = RwLock::new(t);
        let out = execute(
            &tree,
            &tg,
            |_| Ok(()),
            &Fake::default(),
            || None,
            &AtomicBool::new(false),
        );
        assert_eq!(out, Outcome::Deleted);
        let t = tree.read().unwrap();
        assert!(!t.is_live(d));
        assert_eq!(t.node(Tree::ROOT).size, before - 10);
    }

    #[test]
    fn not_recyclable_changes_nothing_and_shows_refused_message() {
        let (t, _, f, ..) = sample(r"C:\data");
        let gen_before = t.generation;
        let size_before = t.node(Tree::ROOT).size;
        let tg = target_of(&t, f, Mode::Recycle);
        let tree = RwLock::new(t);
        // An empty fake source would mark the parent unreadable if it were rescanned.
        let out = execute(
            &tree,
            &tg,
            |_| Err(DeleteError::NotRecyclable),
            &Fake::default(),
            || None,
            &AtomicBool::new(false),
        );
        match out {
            Outcome::Failed { text, .. } => assert_eq!(
                text,
                "Windows couldn't move f to the Recycle Bin, so nothing was deleted."
            ),
            other => panic!("{other:?}"),
        }
        let t = tree.read().unwrap();
        assert_eq!(t.generation, gen_before);
        assert_eq!(t.node(Tree::ROOT).size, size_before);
        assert!(t.is_live(f));
    }

    #[test]
    fn io_error_rescans_the_parent() {
        let (t, d, f, ..) = sample(r"C:\data");
        let tg = target_of(&t, f, Mode::Permanent);
        let tree = RwLock::new(t);
        // On disk, d now holds f (10) and h (3): the sizes must follow the disk.
        let mut fake = Fake::default();
        fake.0.insert(
            PathBuf::from(r"C:\data\d"),
            vec![raw("f", false, 10, 1), raw("h", false, 3, 2)],
        );
        let out = execute(
            &tree,
            &tg,
            |_| Err(DeleteError::Io("in use".into())),
            &fake,
            || None,
            &AtomicBool::new(false),
        );
        match out {
            Outcome::Failed { text, .. } => assert!(text.contains("in use")),
            other => panic!("{other:?}"),
        }
        let t = tree.read().unwrap();
        assert_eq!(t.node(d).size, 13);
        assert_eq!(t.node(d).items, 2);
        // The old f node is gone; the rescan made fresh ones.
        assert!(!t.is_live(f));
    }

    #[test]
    fn io_error_reports_refresh_and_raises_the_flag() {
        let (t, _, f, ..) = sample(r"C:\data");
        let tg = target_of(&t, f, Mode::Permanent);
        let tree = RwLock::new(t);
        let mut fake = Fake::default();
        fake.0
            .insert(PathBuf::from(r"C:\data\d"), vec![raw("f", false, 10, 1)]);
        let flag = AtomicBool::new(false);
        let out = execute(
            &tree,
            &tg,
            |_| Err(DeleteError::Io("busy".into())),
            &fake,
            || None,
            &flag,
        );
        assert!(flag.load(Ordering::Relaxed));
        assert!(matches!(
            out,
            Outcome::Failed {
                refreshed: true,
                ..
            }
        ));
        // The tree is free again afterwards (no lock left behind).
        assert!(tree.try_write().is_ok());
    }

    #[test]
    fn io_error_under_root_restores_unseen() {
        let (t, _, _, g, u) = sample(r"C:\data");
        let tg = target_of(&t, g, Mode::Permanent);
        let tree = RwLock::new(t);
        let mut fake = Fake::default();
        fake.0.insert(
            PathBuf::from(r"C:\data"),
            vec![raw("g", false, 5, 1), raw("d", true, 0, 0)],
        );
        fake.0
            .insert(PathBuf::from(r"C:\data\d"), vec![raw("f", false, 10, 2)]);
        execute(
            &tree,
            &tg,
            |_| Err(DeleteError::Io("locked".into())),
            &fake,
            || Some(100),
            &AtomicBool::new(false),
        );
        let t = tree.read().unwrap();
        assert!(!t.is_live(u), "old Unseen node was replaced");
        let unseen = t
            .node(Tree::ROOT)
            .children
            .iter()
            .copied()
            .find(|&c| t.node(c).kind == NodeKind::Unseen)
            .expect("Unseen re-added");
        assert_eq!(t.node(unseen).size, 85);
        assert_eq!(t.node(Tree::ROOT).size, 100);
    }

    #[test]
    fn real_permanent_delete_removes_file_and_node() {
        let dir = PathBuf::from(r"C:\Users\TomCatT\AppData\Local\Temp\winbaobab-test")
            .join(format!("actions-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("victim.txt"), b"hello").unwrap();
        let mut t = Tree::new(dir.clone());
        let v = t.add_child(Tree::ROOT, "victim.txt", NodeKind::File, 4096, None);
        let tg = target_of(&t, v, Mode::Permanent);
        let tree = RwLock::new(t);
        let out = execute(
            &tree,
            &tg,
            delete_permanently,
            &WinDirSource,
            || None,
            &AtomicBool::new(false),
        );
        let gone = !dir.join("victim.txt").exists();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(out, Outcome::Deleted);
        assert!(gone);
        assert!(!tree.read().unwrap().is_live(v));
    }

    // ---- keys

    fn k(f: impl FnOnce(&mut Keys)) -> Keys {
        let mut k = Keys::default();
        f(&mut k);
        k
    }

    #[test]
    fn key_commands() {
        let c = |k: Keys| classify_keys(&k, true);
        assert_eq!(c(k(|k| k.delete = true)), Some(KeyCmd::Recycle));
        assert_eq!(
            c(k(|k| {
                k.delete = true;
                k.shift = true
            })),
            Some(KeyCmd::Permanent)
        );
        assert_eq!(c(k(|k| k.backspace = true)), Some(KeyCmd::Up));
        assert_eq!(
            c(k(|k| {
                k.left = true;
                k.alt = true
            })),
            Some(KeyCmd::Up)
        );
        assert_eq!(c(k(|k| k.left = true)), None);
        assert_eq!(
            c(k(|k| {
                k.l = true;
                k.ctrl = true
            })),
            Some(KeyCmd::FocusPath)
        );
        assert_eq!(c(k(|k| k.l = true)), None);
    }

    #[test]
    fn keys_are_ignored_while_typing_or_in_dialogs_but_ctrl_l_works() {
        let blocked = |f: fn(&mut Keys)| {
            let mut x = k(f);
            x.typing = true;
            classify_keys(&x, true)
        };
        assert_eq!(blocked(|k| k.delete = true), None);
        assert_eq!(blocked(|k| k.backspace = true), None);
        assert_eq!(
            blocked(|k| {
                k.l = true;
                k.ctrl = true
            }),
            Some(KeyCmd::FocusPath)
        );
    }

    #[test]
    fn nothing_works_while_a_dialog_or_job_is_open() {
        let modal = |f: fn(&mut Keys)| {
            let mut x = k(f);
            x.modal = true;
            classify_keys(&x, true)
        };
        assert_eq!(modal(|k| k.delete = true), None);
        assert_eq!(modal(|k| k.backspace = true), None);
        assert_eq!(
            modal(|k| {
                k.l = true;
                k.ctrl = true
            }),
            None
        );
        // Also when a text field is focused at the same time.
        let mut x = k(|k| {
            k.l = true;
            k.ctrl = true;
            k.typing = true;
        });
        assert_eq!(classify_keys(&x, true), Some(KeyCmd::FocusPath));
        x.modal = true;
        assert_eq!(classify_keys(&x, true), None);
    }

    #[test]
    fn f5_needs_an_idle_screen() {
        assert!(rescan_key_allowed(false, false, false, true));
        assert!(!rescan_key_allowed(false, false, false, false));
        assert!(!rescan_key_allowed(true, false, false, true), "scanning");
        assert!(
            !rescan_key_allowed(false, true, false, true),
            "dialog or job"
        );
        assert!(!rescan_key_allowed(false, false, true, true), "typing");
        assert!(rescan_enabled(false, false));
        assert!(!rescan_enabled(false, true));
        assert!(!rescan_enabled(true, false));
    }

    #[test]
    fn only_ctrl_l_works_off_the_results_screen() {
        assert_eq!(classify_keys(&k(|k| k.delete = true), false), None);
        assert_eq!(classify_keys(&k(|k| k.backspace = true), false), None);
        assert_eq!(
            classify_keys(
                &k(|k| {
                    k.l = true;
                    k.ctrl = true
                }),
                false
            ),
            Some(KeyCmd::FocusPath)
        );
    }

    // ---- menu

    #[test]
    fn menu_info_disables_by_reason() {
        let (t, _, f, _, u) = sample(r"C:\data");
        let i = menu_info(&t, f, false, false);
        assert_eq!((i.delete_block, i.recycle_block), (None, None));
        assert_eq!(
            menu_info(&t, f, true, false).delete_block,
            Some("Wait for the scan to finish.")
        );
        assert_eq!(
            menu_info(&t, f, false, true).delete_block,
            Some("Another delete is in progress.")
        );
        assert!(
            menu_info(&t, Tree::ROOT, false, false)
                .delete_block
                .is_some()
        );
        let un = menu_info(&t, u, false, false);
        assert!(un.delete_block.is_some());
        assert!(!un.has_path);

        let (n, _, nf, ..) = sample(r"\\srv\share\data");
        let i = menu_info(&n, nf, false, false);
        assert_eq!(i.delete_block, None);
        assert_eq!(i.recycle_block, Some("Network drives have no Recycle Bin."));
    }
}
