//! Draws the WinBaobab icon (original concentric-ring artwork, Tango palette).
//! Run: ./scripts/cargo.ps1 run --example make_icon
use std::f32::consts::TAU;
use std::fs::File;

use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Transform};

/// Tango palette: (r, g, b).
const BLUE: (u8, u8, u8) = (0x34, 0x65, 0xa4);
const ORANGE: (u8, u8, u8) = (0xf5, 0x79, 0x00);
const GREEN: (u8, u8, u8) = (0x73, 0xd2, 0x16);
const YELLOW: (u8, u8, u8) = (0xed, 0xd4, 0x00);
const PURPLE: (u8, u8, u8) = (0x75, 0x50, 0x7b);
const CHOCOLATE: (u8, u8, u8) = (0x8f, 0x59, 0x02);
const DARK: (u8, u8, u8) = (0x2e, 0x34, 0x36);

fn paint((r, g, b): (u8, u8, u8)) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color(Color::from_rgba8(r, g, b, 255));
    p.anti_alias = true;
    p
}

/// Fills an annular sector (angles in turns, 0 = top, clockwise).
fn sector(pm: &mut Pixmap, c: f32, r0: f32, r1: f32, a0: f32, a1: f32, col: (u8, u8, u8)) {
    let steps = (((a1 - a0) * 96.0).ceil() as usize).max(2);
    let pt = |r: f32, a: f32| {
        let t = a * TAU;
        (c + r * t.sin(), c - r * t.cos())
    };
    let mut pb = PathBuilder::new();
    let (x, y) = pt(r1, a0);
    pb.move_to(x, y);
    for i in 1..=steps {
        let (x, y) = pt(r1, a0 + (a1 - a0) * i as f32 / steps as f32);
        pb.line_to(x, y);
    }
    if r0 <= 0.0 {
        pb.line_to(c, c);
    } else {
        for i in (0..=steps).rev() {
            let (x, y) = pt(r0, a0 + (a1 - a0) * i as f32 / steps as f32);
            pb.line_to(x, y);
        }
    }
    pb.close();
    if let Some(path) = pb.finish() {
        pm.fill_path(&path, &paint(col), FillRule::Winding, Transform::identity(), None);
    }
}

/// (start, end, colour) of one ring segment, in turns.
type Segment = (f32, f32, (u8, u8, u8));

fn draw(size: u32) -> Pixmap {
    let mut pm = Pixmap::new(size, size).unwrap();
    let s = size as f32;
    let c = s / 2.0;
    let outer = s * 0.48;
    sector(&mut pm, c, 0.0, outer, 0.0, 1.0, DARK);
    let gap = (s / 64.0).max(0.5) / s; // seam between segments, in turns-ish
    let rings = if size < 32 { 2 } else { 3 };
    // (inner, outer) radius fractions of `outer`, leaving a hub in the middle.
    let bands: &[(f32, f32)] = if rings == 2 {
        &[(0.30, 0.62), (0.66, 0.97)]
    } else {
        &[(0.26, 0.50), (0.53, 0.75), (0.78, 0.97)]
    };
    // Segments per ring: (start, end, colour) in turns.
    let layout: [&[Segment]; 3] = [
        &[(0.0, 0.45, BLUE), (0.45, 0.75, ORANGE), (0.75, 1.0, GREEN)],
        &[(0.0, 0.30, BLUE), (0.30, 0.45, YELLOW), (0.45, 0.62, ORANGE), (0.62, 0.75, CHOCOLATE), (0.75, 0.92, GREEN), (0.92, 1.0, PURPLE)],
        &[(0.0, 0.18, BLUE), (0.18, 0.40, PURPLE), (0.45, 0.58, ORANGE), (0.58, 0.70, YELLOW), (0.75, 0.86, GREEN), (0.86, 0.97, BLUE)],
    ];
    for (i, &(r0, r1)) in bands.iter().enumerate() {
        for &(a0, a1, col) in layout[i] {
            sector(&mut pm, c, r0 * outer, r1 * outer, a0 + gap, a1 - gap, col);
        }
    }
    sector(&mut pm, c, 0.0, outer * 0.20, 0.0, 1.0, YELLOW);
    pm
}

/// tiny-skia stores premultiplied RGBA; ICO wants straight alpha.
fn straight_rgba(pm: &Pixmap) -> Vec<u8> {
    pm.pixels()
        .iter()
        .flat_map(|p| {
            let c = p.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        })
        .collect()
}

fn main() {
    let mut dir = ico::IconDir::new(ico::ResourceType::Icon);
    for size in [16u32, 24, 32, 48, 64, 256] {
        let pm = draw(size);
        let img = ico::IconImage::from_rgba_data(size, size, straight_rgba(&pm));
        dir.add_entry(ico::IconDirEntry::encode(&img).expect("encode ico entry"));
        if size == 256 {
            pm.save_png("assets/winbaobab.png").expect("write png");
        }
    }
    dir.write(File::create("assets/winbaobab.ico").expect("create ico"))
        .expect("write ico");
    println!("wrote assets/winbaobab.ico and assets/winbaobab.png");
}
