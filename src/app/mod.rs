//! App shell: owns the screen state and dispatches to the screens.

pub mod actions;
pub mod charts;
pub mod locations;
pub mod results;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use eframe::egui;

use crate::drives::unseen_used;
use crate::model::{NodeId, Tree};
use crate::scan::{ScanHandle, WinDirSource, start_scan};
use crate::settings::{Chart, Settings};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    Locations,
    Results,
}

pub struct WinBaobabApp {
    pub screen: Screen,
    pub settings: Settings,
    pub scan: Option<ScanHandle>,
    pub focus: NodeId,
    pub chart: Chart,
    pub(crate) locations: locations::LocationsState,
    pub(crate) results: results::ResultsState,
    pub(crate) actions: actions::ActionsState,
    /// Path being scanned (shown by the Results screen).
    pub scan_path: PathBuf,
    /// Non-modal error from saving settings, if any.
    pub save_error: Option<String>,
    /// Scan being started on a worker (generation, handle).
    starting: Option<Receiver<(u64, ScanHandle)>>,
    scan_gen: u64,
    save_rx: Option<Receiver<Option<String>>>,
    repaint_ctx: Option<egui::Context>,
}

/// Serializes settings writes (they share one temp file name per process).
static SAVE_LOCK: Mutex<()> = Mutex::new(());
/// Sequence handed to each save request / highest sequence already written.
static SAVE_SEQ: AtomicU64 = AtomicU64::new(0);
static SAVE_WRITTEN: AtomicU64 = AtomicU64::new(0);

pub(crate) fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl WinBaobabApp {
    pub fn new(cc: &eframe::CreationContext, initial_path: Option<PathBuf>) -> Self {
        cc.egui_ctx
            .options_mut(|o| o.theme_preference = egui::ThemePreference::System);
        let settings = Settings::load_from(&Settings::default_path());
        let chart = settings.default_chart;
        let mut app = WinBaobabApp {
            screen: Screen::Locations,
            settings,
            scan: None,
            focus: Tree::ROOT,
            chart,
            locations: locations::LocationsState::new(&cc.egui_ctx),
            results: results::ResultsState::default(),
            actions: actions::ActionsState::default(),
            scan_path: PathBuf::new(),
            save_error: None,
            starting: None,
            scan_gen: 0,
            save_rx: None,
            repaint_ctx: Some(cc.egui_ctx.clone()),
        };
        if let Some(p) = initial_path {
            app.begin_scan(p);
        }
        app
    }

    /// True while a requested scan is still being started on its worker.
    pub fn is_starting(&self) -> bool {
        self.starting.is_some()
    }

    /// Starts scanning `path`, records it as a recent scan and shows Results.
    /// All I/O (disk query, settings save) happens off the UI thread.
    pub fn begin_scan(&mut self, path: PathBuf) {
        self.begin_scan_with_used(path, None);
    }

    /// Like `begin_scan`, with an already-known used-bytes figure (drive rows).
    pub fn begin_scan_with_used(&mut self, path: PathBuf, known_used: Option<u64>) {
        // Normalise the user-supplied root once; every child path is then
        // built by appending raw scanned names, never re-normalised (that
        // would turn `foo.` into its sibling `foo`).
        let path = crate::winfs::normalize_root(&path);
        if let Some(old) = self.scan.take() {
            old.cancel();
        }
        // Replacing the receiver supersedes any older still-starting scan:
        // its worker fails to send and cancels the stale handle itself.
        self.scan_gen += 1;
        let generation = self.scan_gen;
        let (tx, rx) = channel();
        self.starting = Some(rx);
        let p = path.clone();
        let ctx = self.repaint_ctx.clone();
        std::thread::spawn(move || {
            // Only the root of a local fixed volume gets an Unseen block.
            let used = unseen_used(&p, known_used);
            let h = start_scan(p, Arc::new(WinDirSource), used);
            if let Err(e) = tx.send((generation, h)) {
                e.0.1.cancel();
            }
            if let Some(c) = ctx {
                c.request_repaint();
            }
        });
        self.focus = Tree::ROOT;
        self.results = results::ResultsState::default();
        self.actions.on_new_scan();
        self.chart = self.settings.default_chart;
        self.settings.add_recent(&path, now_unix());
        self.save_settings();
        self.scan_path = path;
        self.screen = Screen::Results;
    }

    /// Saves the current settings on a worker thread; the outcome lands in
    /// `save_error` via `poll_workers`.
    pub(crate) fn save_settings(&mut self) {
        let settings = self.settings.clone();
        let seq = SAVE_SEQ.fetch_add(1, Ordering::SeqCst) + 1;
        let (stx, srx) = channel();
        self.save_rx = Some(srx);
        let ctx = self.repaint_ctx.clone();
        std::thread::spawn(move || {
            let _guard = SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            // A newer snapshot was already written; don't overwrite it with this one.
            let err = if SAVE_WRITTEN.load(Ordering::SeqCst) > seq {
                None
            } else {
                SAVE_WRITTEN.store(seq, Ordering::SeqCst);
                settings
                    .save_to(&Settings::default_path())
                    .err()
                    .map(|e| format!("Couldn't save settings: {e}"))
            };
            let _ = stx.send(err);
            if let Some(c) = ctx {
                c.request_repaint();
            }
        });
    }

    fn poll_workers(&mut self) {
        if let Some(rx) = &self.starting {
            match rx.try_recv() {
                Ok((g, h)) if g == self.scan_gen => {
                    self.scan = Some(h);
                    self.starting = None;
                }
                Ok((_, stale)) => stale.cancel(),
                Err(TryRecvError::Disconnected) => self.starting = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        if let Some(rx) = &self.save_rx {
            match rx.try_recv() {
                Ok(err) => {
                    self.save_error = err;
                    self.save_rx = None;
                }
                Err(TryRecvError::Disconnected) => self.save_rx = None,
                Err(TryRecvError::Empty) => {}
            }
        }
    }
    /// Cancels any running scan and returns to the Locations screen.
    pub fn back_to_locations(&mut self) {
        self.starting = None; // a late-arriving handle is cancelled by its worker
        if let Some(h) = self.scan.take() {
            h.cancel();
        }
        // Nothing stays selected behind the Locations screen.
        self.actions.selected = None;
        self.screen = Screen::Locations;
    }
}

impl eframe::App for WinBaobabApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_workers();
        actions::handle_keys(self, &ui.ctx().clone());
        match self.screen {
            Screen::Locations => locations::locations_ui(self, ui),
            Screen::Results => results::results_ui(self, ui),
        }
    }
}
