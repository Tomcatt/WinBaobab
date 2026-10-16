//! Locations screen: drives, folder picker, path box and recent scans.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError, channel};

use eframe::egui::{self, Color32, Pos2, Rect, Sense, Stroke, Vec2};

use super::{WinBaobabApp, now_unix};
use crate::drives::{DriveInfo, DriveKind, list_drives};
use crate::format::format_size;

pub(crate) struct LocationsState {
    /// `None` until the first background enumeration finishes.
    drives: Option<Vec<DriveInfo>>,
    drives_rx: Option<Receiver<Vec<DriveInfo>>>,
    drives_error: Option<String>,
    path_text: String,
    error: Option<String>,
    /// Pending off-thread existence check of a typed path.
    check_rx: Option<Receiver<(PathBuf, bool)>>,
    /// Ctrl+L: put the keyboard cursor in the path box on the next frame.
    pub(crate) focus_path_box: bool,
}

impl LocationsState {
    pub(crate) fn new(ctx: &egui::Context) -> Self {
        let mut s = LocationsState {
            drives: None,
            drives_rx: None,
            drives_error: None,
            path_text: String::new(),
            error: None,
            check_rx: None,
            focus_path_box: false,
        };
        s.refresh_drives(ctx);
        s
    }

    /// Re-runs `list_drives` on a worker thread; a dead network drive can't
    /// block the UI. Keeps showing the old list until the new one arrives.
    fn refresh_drives(&mut self, ctx: &egui::Context) {
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        self.drives_error = None;
        std::thread::spawn(move || {
            let _ = tx.send(list_drives());
            ctx.request_repaint();
        });
        self.drives_rx = Some(rx);
    }
}

/// Cleans typed/pasted/CLI path text: trims whitespace, drops one pair of
/// surrounding double quotes (inner text is kept as is), drops a stray trailing
/// quote (Windows turns `"C:\"` into `C:"`), and makes a bare `C:` into `C:\`.
pub fn clean_path_input(s: &str) -> PathBuf {
    let mut t = s.trim();
    if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
        t = &t[1..t.len() - 1];
    } else if let Some(r) = t.strip_suffix('"') {
        t = r;
    }
    let b = t.as_bytes();
    if b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        return PathBuf::from(format!("{t}\\"));
    }
    PathBuf::from(t)
}

/// "just now", "5 minutes ago", "3 hours ago", "2 days ago", else `YYYY-MM-DD`.
pub fn relative_time(now_unix: u64, then_unix: u64) -> String {
    let secs = now_unix.saturating_sub(then_unix);
    let plural = |n: u64, unit: &str| {
        if n == 1 {
            format!("1 {unit} ago")
        } else {
            format!("{n} {unit}s ago")
        }
    };
    match secs {
        0..=59 => "just now".to_string(),
        60..=3599 => plural(secs / 60, "minute"),
        3600..=86399 => plural(secs / 3600, "hour"),
        86400..=604799 => plural(secs / 86400, "day"),
        _ => {
            let (y, m, d) = civil_from_days((then_unix / 86400) as i64);
            format!("{y:04}-{m:02}-{d:02}")
        }
    }
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

fn drive_title(d: &DriveInfo) -> String {
    let letter = d.root.to_string_lossy().trim_end_matches('\\').to_string();
    let name = if !d.label.is_empty() {
        d.label.clone()
    } else if let Some(r) = &d.remote {
        r.clone()
    } else {
        match d.kind {
            DriveKind::Network => "Network drive",
            DriveKind::Removable => "Removable drive",
            _ => "Local disk",
        }
        .to_string()
    };
    format!("{name} ({letter})")
}

/// Simple painter-drawn glyph per drive kind (no image assets).
fn draw_icon(ui: &mut egui::Ui, kind: DriveKind) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(32.0), Sense::hover());
    let p = ui.painter();
    let col = ui.visuals().text_color();
    let st = Stroke::new(1.5, col);
    let c = rect.center();
    match kind {
        DriveKind::Network => {
            // Three linked nodes.
            let pts = [
                Pos2::new(c.x, c.y - 9.0),
                Pos2::new(c.x - 9.0, c.y + 8.0),
                Pos2::new(c.x + 9.0, c.y + 8.0),
            ];
            for (a, b) in [(0, 1), (1, 2), (2, 0)] {
                p.line_segment([pts[a], pts[b]], st);
            }
            for pt in pts {
                p.circle_filled(pt, 4.0, col);
            }
        }
        DriveKind::Removable => {
            // USB-stick-like: body plus plug.
            p.rect_stroke(
                Rect::from_center_size(Pos2::new(c.x, c.y + 4.0), Vec2::new(14.0, 18.0)),
                2.0,
                st,
                egui::StrokeKind::Inside,
            );
            p.rect_filled(
                Rect::from_center_size(Pos2::new(c.x, c.y - 9.0), Vec2::new(8.0, 6.0)),
                1.0,
                col,
            );
        }
        _ => {
            // Disk drive: slab with an activity light.
            let slab = Rect::from_center_size(c, Vec2::new(28.0, 16.0));
            p.rect_stroke(slab, 3.0, st, egui::StrokeKind::Inside);
            p.circle_filled(Pos2::new(slab.right() - 5.0, slab.center().y), 2.0, col);
        }
    }
}

/// Usage bar with a visible track in light and dark themes; tiny fills are
/// drawn with a radius clamped to half their width (or skipped under 1 px).
fn usage_bar(ui: &mut egui::Ui, frac: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(260.0, 10.0), Sense::hover());
    let track = if ui.visuals().dark_mode {
        Color32::from_white_alpha(40)
    } else {
        Color32::from_black_alpha(40)
    };
    ui.painter().rect_filled(rect, 3.0, track);
    let w = rect.width() * frac.clamp(0.0, 1.0);
    if w >= 1.0 {
        let fill = Rect::from_min_size(rect.min, Vec2::new(w, rect.height()));
        ui.painter()
            .rect_filled(fill, (w / 2.0).min(2.0), ui.visuals().hyperlink_color);
    }
}

fn drive_row(ui: &mut egui::Ui, d: &DriveInfo) -> bool {
    let inner = ui.horizontal(|ui| {
        draw_icon(ui, d.kind);
        ui.vertical(|ui| {
            ui.label(egui::RichText::new(drive_title(d)).strong());
            let frac = if d.total > 0 {
                d.total.saturating_sub(d.free) as f32 / d.total as f32
            } else {
                0.0
            };
            usage_bar(ui, frac);
            if d.total > 0 {
                ui.label(format!(
                    "{} free of {}",
                    format_size(d.free),
                    format_size(d.total)
                ));
            } else {
                ui.weak("Not available");
            }
        });
    });
    let resp = ui.interact(
        inner.response.rect,
        ui.make_persistent_id(("drive", &d.root)),
        Sense::click(),
    );
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
}

/// Spawns the off-thread "is this an existing folder" check.
fn spawn_check(ctx: &egui::Context, text: String) -> Receiver<(PathBuf, bool)> {
    let (tx, rx) = channel();
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let is_dir = |p: &PathBuf| std::fs::metadata(p).map(|m| m.is_dir()).unwrap_or(false);
        let mut p = clean_path_input(&text);
        let mut ok = is_dir(&p);
        if !ok {
            // Fall back to the whitespace-trimmed path (e.g. pasted `"C:\Foo "`).
            let trimmed = PathBuf::from(p.to_string_lossy().trim());
            if trimmed != p && is_dir(&trimmed) {
                p = trimmed;
                ok = true;
            }
        }
        let _ = tx.send((p, ok));
        ctx.request_repaint();
    });
    rx
}

pub fn locations_ui(app: &mut WinBaobabApp, ui: &mut egui::Ui) {
    let ctx = ui.ctx().clone();
    let mut to_scan: Option<PathBuf> = None;
    let mut known_used: Option<u64> = None;

    // Collect async results.
    let st = &mut app.locations;
    if let Some(rx) = &st.drives_rx {
        match rx.try_recv() {
            Ok(v) => {
                st.drives = Some(v);
                st.drives_rx = None;
            }
            Err(TryRecvError::Disconnected) => {
                st.drives_rx = None;
                st.drives_error = Some("Couldn't list drives.".to_string());
            }
            Err(TryRecvError::Empty) => {}
        }
    }
    if let Some(rx) = &st.check_rx {
        match rx.try_recv() {
            Ok((path, ok)) => {
                st.check_rx = None;
                if ok {
                    to_scan = Some(path);
                } else {
                    st.error = Some(format!(
                        "{} doesn't exist or isn't a folder.",
                        path.display()
                    ));
                }
            }
            Err(TryRecvError::Disconnected) => {
                st.check_rx = None;
                st.error = Some("Couldn't check that path.".to_string());
            }
            Err(TryRecvError::Empty) => {}
        }
    }

    // Same default panel frame (8 px inner margin) as the results screen.
    egui::CentralPanel::default().show(ui, |ui| {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.heading("Locations");
            ui.add_space(8.0);

            ui.horizontal(|ui| {
                if ui.button("Scan Folder…").clicked()
                    && let Some(p) = rfd::FileDialog::new().pick_folder()
                {
                    to_scan = Some(p);
                }
                ui.separator();
                let edit = ui.add(
                    egui::TextEdit::singleline(&mut app.locations.path_text)
                        .hint_text(r"Paste a path, e.g. D:\Data or \\server\share")
                        .desired_width(380.0),
                );
                if std::mem::take(&mut app.locations.focus_path_box) {
                    edit.request_focus();
                }
                let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if edit.changed() {
                    app.locations.error = None;
                }
                let busy = app.locations.check_rx.is_some();
                let clicked = ui.add_enabled(!busy, egui::Button::new("Scan")).clicked();
                if (clicked || enter) && !busy {
                    let text = app.locations.path_text.clone();
                    if text.trim().is_empty() {
                        app.locations.error = Some("Enter a folder path to scan.".to_string());
                    } else {
                        app.locations.error = None;
                        app.locations.check_rx = Some(spawn_check(&ctx, text));
                    }
                }
                if app.locations.check_rx.is_some() {
                    ui.spinner();
                }
            });
            if let Some(e) = &app.locations.error {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            if let Some(e) = &app.save_error {
                ui.colored_label(ui.visuals().warn_fg_color, e);
            }

            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.heading("Drives");
                let refreshing = app.locations.drives_rx.is_some();
                if ui
                    .add_enabled(!refreshing, egui::Button::new("Refresh"))
                    .clicked()
                {
                    app.locations.refresh_drives(&ctx);
                }
                if refreshing {
                    ui.spinner();
                }
            });
            if let Some(e) = &app.locations.drives_error {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            match &app.locations.drives {
                None => {
                    ui.label("Loading drives…");
                }
                Some(drives) => {
                    for d in drives {
                        if drive_row(ui, d) {
                            to_scan = Some(d.root.clone());
                            known_used = (d.total > 0).then(|| d.total.saturating_sub(d.free));
                        }
                        ui.add_space(6.0);
                    }
                }
            }

            ui.add_space(12.0);
            ui.heading("Recent scans");
            if app.settings.recent.is_empty() {
                ui.weak("Nothing scanned yet.");
            }
            let now = now_unix();
            for r in &app.settings.recent {
                ui.horizontal(|ui| {
                    if ui
                        .link(r.path.display().to_string())
                        .on_hover_text("Scan again")
                        .clicked()
                    {
                        to_scan = Some(r.path.clone());
                        known_used = None;
                    }
                    ui.weak(relative_time(now, r.when_unix));
                });
            }
        });
    });

    if let Some(p) = to_scan {
        app.begin_scan_with_used(p, known_used);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;

    #[test]
    fn relative_buckets() {
        assert_eq!(relative_time(NOW, NOW), "just now");
        assert_eq!(relative_time(NOW, NOW - 59), "just now");
        assert_eq!(relative_time(NOW, NOW - 60), "1 minute ago");
        assert_eq!(relative_time(NOW, NOW - 300), "5 minutes ago");
        assert_eq!(relative_time(NOW, NOW - 3600), "1 hour ago");
        assert_eq!(relative_time(NOW, NOW - 3 * 3600), "3 hours ago");
        assert_eq!(relative_time(NOW, NOW - 86400), "1 day ago");
        assert_eq!(relative_time(NOW, NOW - 2 * 86400 - 5), "2 days ago");
    }

    #[test]
    fn relative_future_is_just_now() {
        assert_eq!(relative_time(NOW, NOW + 500), "just now");
    }

    #[test]
    fn relative_old_is_date() {
        // 2027-01-15 00:00:00 UTC
        assert_eq!(relative_time(1_900_000_000, 1_799_971_200), "2027-01-15");
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
    }

    #[test]
    fn clean_input() {
        let c = |s: &str| clean_path_input(s);
        assert_eq!(c(r"  C:\Foo  "), PathBuf::from(r"C:\Foo"));
        assert_eq!(c(r#""C:\Foo Bar""#), PathBuf::from(r"C:\Foo Bar"));
        assert_eq!(c(r#"  " C:\Foo ""#), PathBuf::from(r" C:\Foo "));
        assert_eq!(c(r#"C:""#), PathBuf::from(r"C:\"));
        assert_eq!(c("C:"), PathBuf::from(r"C:\"));
        assert_eq!(c(r"\\srv\share"), PathBuf::from(r"\\srv\share"));
        assert_eq!(c(r#"C:\Foo""#), PathBuf::from(r"C:\Foo"));
    }

    #[test]
    fn titles() {
        let mut d = DriveInfo {
            root: PathBuf::from(r"C:\"),
            label: "OS".into(),
            kind: DriveKind::Fixed,
            total: 1,
            free: 1,
            remote: None,
        };
        assert_eq!(drive_title(&d), "OS (C:)");
        d.label.clear();
        d.kind = DriveKind::Network;
        d.remote = Some(r"\\nas\share".into());
        assert_eq!(drive_title(&d), r"\\nas\share (C:)");
    }
}
