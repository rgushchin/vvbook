use crate::canvas::Canvas;
use vvbook_pal::Tone;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiIcon {
    Library,
    VvDrop,
    PocketPip,
    Chess,
    Settings,
    Bookmark,
    Lock,
    Check,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChessPieceKind {
    King,
    Queen,
    Rook,
    Bishop,
    Knight,
    Pawn,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PipMood {
    Happy,
    Curious,
    Reading,
    Sleeping,
    Celebrating,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PipHat {
    None = 0,
    FlowerCrown = 1,
    WizardHat = 2,
    SpaceHelmet = 3,
    DetectiveCap = 4,
    RoyalTiara = 5,
}

impl PipHat {
    pub const ALL: [PipHat; 6] = [
        Self::None,
        Self::FlowerCrown,
        Self::WizardHat,
        Self::SpaceHelmet,
        Self::DetectiveCap,
        Self::RoyalTiara,
    ];
}

pub fn draw_battery_capsule(canvas: &mut Canvas<'_>, x: i32, y: i32, percent: u8, charging: bool) {
    // Clean 30x14 orthogonal battery meter + 3x6 terminal nub matched to 20 px mono status bar
    canvas.stroke_rect(x, y, 30, 14, 2, Tone::Ink);
    canvas.fill_rect(x + 30, y + 4, 3, 6, Tone::Ink);
    let fill_w = ((percent.min(100) as i32 * 22) + 50) / 100;
    if fill_w > 0 {
        canvas.fill_rect(x + 4, y + 4, fill_w.max(2), 6, Tone::Ink);
    }
    if charging {
        canvas.draw_line(x + 16, y + 3, x + 13, y + 7, Tone::Paper);
        canvas.hline(x + 13, y + 7, 4, Tone::Paper);
        canvas.draw_line(x + 17, y + 7, x + 14, y + 10, Tone::Paper);
    }
}

pub fn draw_ui_icon(canvas: &mut Canvas<'_>, icon: UiIcon, cx: i32, cy: i32, size: i32, tone: Tone) {
    let r = size / 2;
    match icon {
        UiIcon::Library => {
            // Bauhaus orthogonal open book with central spine bar
            let w = size * 4 / 5;
            let h = size * 3 / 4;
            let x0 = cx - w / 2;
            let y0 = cy - h / 2;
            canvas.stroke_rect(x0, y0, w, h, 2, tone);
            canvas.fill_rect(cx - 1, y0 + 2, 2, h - 4, tone);
            canvas.fill_rect(x0 + 4, y0 + 6, (w / 2 - 7).max(2), 2, tone);
            canvas.fill_rect(x0 + 4, y0 + 11, (w / 2 - 9).max(2), 2, tone);
            canvas.fill_rect(cx + 3, y0 + 6, (w / 2 - 7).max(2), 2, tone);
            canvas.fill_rect(cx + 3, y0 + 11, (w / 2 - 9).max(2), 2, tone);
        }
        UiIcon::VvDrop => {
            // Bauhaus broadcast origin dot + geometric arcs
            canvas.fill_circle(cx, cy + r / 3, 4, tone);
            for rad in [r / 2, r * 4 / 5] {
                for deg in -52i32..=52 {
                    let (sx, sy) = sin_cos_deg_approx(deg);
                    let px = cx + (sx * rad) / 100;
                    let py = cy + r / 3 - (sy * rad) / 100;
                    canvas.fill_rect(px - 1, py - 1, 3, 3, tone);
                }
            }
        }
        UiIcon::PocketPip => {
            // Minimalist geometric fox head emblem
            let w = r * 7 / 5;
            let x0 = cx - w / 2;
            let y0 = cy - r / 2;
            fill_triangle(canvas, x0 + 2, y0 + 8, x0 + 4, y0 - 4, cx - 2, y0 + 4, tone);
            fill_triangle(canvas, x0 + w - 2, y0 + 8, x0 + w - 4, y0 - 4, cx + 2, y0 + 4, tone);
            fill_triangle(canvas, x0, y0 + 6, x0 + w, y0 + 6, cx, cy + r - 1, tone);
            canvas.fill_rect(cx - w / 4 - 1, y0 + 8, 3, 3, Tone::Paper);
            canvas.fill_rect(cx + w / 4 - 1, y0 + 8, 3, 3, Tone::Paper);
        }
        UiIcon::Chess => {
            draw_chess_piece(canvas, ChessPieceKind::Knight, true, false, cx, cy, size);
        }
        UiIcon::Settings => {
            // Bauhaus orthogonal faders: heavy tracks + square stops
            let w = size * 3 / 4;
            let x0 = cx - w / 2;
            canvas.fill_rect(x0, cy - 6, w, 2, tone);
            canvas.fill_rect(cx - 8, cy - 10, 8, 10, tone);
            canvas.fill_rect(cx - 6, cy - 8, 4, 6, Tone::Paper);
            canvas.fill_rect(x0, cy + 5, w, 2, tone);
            canvas.fill_rect(cx + 1, cy + 1, 8, 10, tone);
            canvas.fill_rect(cx + 3, cy + 3, 4, 6, Tone::Paper);
        }
        UiIcon::Bookmark => {
            let w = size / 2;
            let h = size * 3 / 4;
            let x0 = cx - w / 2;
            let y0 = cy - h / 2;
            canvas.fill_rect(x0, y0, w, h, tone);
            fill_triangle(canvas, x0, y0 + h, x0 + w, y0 + h, cx, y0 + h - w / 2, Tone::Paper);
        }
        UiIcon::Lock => {
            let w = size * 3 / 5;
            let h = size / 2;
            canvas.stroke_circle(cx, cy - h / 3, w / 3, 2, tone);
            canvas.fill_rect(cx - w / 2, cy - 2, w, h, tone);
            canvas.fill_rect(cx - 2, cy + 2, 4, (h / 2).max(4), Tone::Paper);
        }
        UiIcon::Check => {
            canvas.draw_line(cx - r / 2, cy, cx - r / 6, cy + r / 3, tone);
            canvas.draw_line(cx - r / 6, cy + r / 3, cx + r / 2, cy - r / 3, tone);
            canvas.draw_line(cx - r / 2, cy + 1, cx - r / 6, cy + r / 3 + 1, tone);
            canvas.draw_line(cx - r / 6, cy + r / 3 + 1, cx + r / 2, cy - r / 3 + 1, tone);
        }
    }
}

/// Draw crisp Staunton Chess piece centered at `(cx, cy)` inside `square_px` box.
/// Includes high-contrast outline + inner detail so pieces remain unmistakable on
/// both Paper and Charcoal/Silver E-Ink squares.
pub fn draw_chess_piece(
    canvas: &mut Canvas<'_>,
    kind: ChessPieceKind,
    is_white: bool,
    on_dark_square: bool,
    cx: i32,
    cy: i32,
    square_px: i32,
) {
    let s = (square_px * 38) / 56; // scale factor relative to 38px nominal
    let sc = |v: i32| (v * s) / 38;

    let fill = if is_white { Tone::Paper } else { Tone::Ink };
    let outline = if is_white {
        Tone::Ink
    } else if on_dark_square {
        Tone::Paper
    } else {
        Tone::Ink
    };
    let detail = if is_white { Tone::Ink } else { Tone::Paper };

    // Base plinth shared by all Staunton pieces
    let base_y = cy + sc(14);
    let base_w = sc(24);
    let draw_base = |c: &mut Canvas<'_>, t: Tone, grow: i32| {
        c.fill_rect(
            cx - base_w / 2 - grow,
            base_y - sc(4) - grow,
            base_w + grow * 2,
            sc(7) + grow * 2,
            t,
        );
    };

    // Draw outline pass (expanded silhouette) then fill pass + inner detail
    for pass in 0..2 {
        let t = if pass == 0 { outline } else { fill };
        let g = if pass == 0 { 2 } else { 0 };
        draw_base(canvas, t, g);

        match kind {
            ChessPieceKind::Pawn => {
                // Tapered stem + collar + sphere head
                fill_trapezoid(canvas, cx, base_y - sc(3), sc(14) + g * 2, sc(7) + g * 2, sc(12) + g, t);
                canvas.fill_rect(cx - sc(7) - g, cy - sc(1), sc(14) + g * 2, sc(3) + g, t);
                canvas.fill_circle(cx, cy - sc(7), sc(7) + g, t);
            }
            ChessPieceKind::Rook => {
                // Solid tower + flared parapet + 3 battlements
                fill_trapezoid(canvas, cx, base_y - sc(3), sc(18) + g * 2, sc(14) + g * 2, sc(16) + g, t);
                canvas.fill_rect(cx - sc(10) - g, cy - sc(11) - g, sc(20) + g * 2, sc(6) + g, t);
                for bx in [-sc(7), 0, sc(7)] {
                    canvas.fill_rect(cx + bx - sc(2) - g, cy - sc(16) - g, sc(5) + g * 2, sc(6) + g, t);
                }
            }
            ChessPieceKind::Knight => {
                // Iconic arched horse neck, muzzle, ears & mane
                fill_trapezoid(canvas, cx, base_y - sc(3), sc(19) + g * 2, sc(13) + g * 2, sc(10) + g, t);
                canvas.fill_circle(cx + sc(1), cy - sc(2), sc(9) + g, t);
                // Head & snout pointing left
                fill_triangle(
                    canvas,
                    cx + sc(6) + g,
                    cy - sc(13) - g,
                    cx - sc(13) - g,
                    cy - sc(3),
                    cx + sc(2) + g,
                    cy + sc(2),
                    t,
                );
                canvas.fill_circle(cx - sc(9), cy - sc(4), sc(4) + g, t);
                // Ear
                fill_triangle(
                    canvas,
                    cx - sc(2),
                    cy - sc(10),
                    cx + sc(3) + g,
                    cy - sc(19) - g,
                    cx + sc(6) + g,
                    cy - sc(9),
                    t,
                );
            }
            ChessPieceKind::Bishop => {
                fill_trapezoid(canvas, cx, base_y - sc(3), sc(16) + g * 2, sc(8) + g * 2, sc(13) + g, t);
                canvas.fill_rect(cx - sc(8) - g, cy - sc(2), sc(16) + g * 2, sc(3) + g, t);
                // Mitre oval + top finial
                canvas.fill_circle(cx, cy - sc(9), sc(7) + g, t);
                fill_triangle(
                    canvas,
                    cx - sc(6) - g,
                    cy - sc(9),
                    cx + sc(6) + g,
                    cy - sc(9),
                    cx,
                    cy - sc(19) - g,
                    t,
                );
                canvas.fill_circle(cx, cy - sc(19), sc(2) + g, t);
            }
            ChessPieceKind::Queen => {
                fill_trapezoid(canvas, cx, base_y - sc(3), sc(19) + g * 2, sc(10) + g * 2, sc(14) + g, t);
                canvas.fill_rect(cx - sc(9) - g, cy - sc(2), sc(18) + g * 2, sc(3) + g, t);
                // Bold 3-point Staunton coronet (clean at E-Ink resolution)
                fill_trapezoid(canvas, cx, cy - sc(2), sc(14) + g * 2, sc(18) + g * 2, sc(10) + g, t);
                for (dx, tip_y) in [
                    (-sc(8), cy - sc(15)),
                    (0, cy - sc(18)),
                    (sc(8), cy - sc(15)),
                ] {
                    fill_triangle(canvas, cx - sc(4), cy - sc(7), cx + sc(4), cy - sc(7), cx + dx, tip_y - g, t);
                    canvas.fill_circle(cx + dx, tip_y - g, sc(3) + g / 2, t);
                }
            }
            ChessPieceKind::King => {
                fill_trapezoid(canvas, cx, base_y - sc(3), sc(20) + g * 2, sc(11) + g * 2, sc(15) + g, t);
                canvas.fill_rect(cx - sc(9) - g, cy - sc(3), sc(18) + g * 2, sc(4) + g, t);
                // Crown lobes
                canvas.fill_circle(cx - sc(5), cy - sc(8), sc(6) + g, t);
                canvas.fill_circle(cx + sc(5), cy - sc(8), sc(6) + g, t);
                canvas.fill_circle(cx, cy - sc(9), sc(6) + g, t);
                // Cross finial
                canvas.fill_rect(cx - sc(2) - g, cy - sc(21) - g, sc(4) + g * 2, sc(10) + g * 2, t);
                canvas.fill_rect(cx - sc(5) - g, cy - sc(18) - g, sc(10) + g * 2, sc(3) + g * 2, t);
            }
        }
    }

    // Crisp Staunton interior accents
    canvas.hline(cx - sc(9), base_y - sc(2), sc(18), detail);
    match kind {
        ChessPieceKind::Knight => {
            canvas.fill_circle(cx - sc(5), cy - sc(8), sc(1).max(1), detail);
            canvas.draw_line(cx + sc(2), cy - sc(12), cx + sc(6), cy + sc(4), detail);
        }
        ChessPieceKind::Bishop => {
            canvas.draw_line(cx - sc(3), cy - sc(12), cx + sc(3), cy - sc(6), detail);
        }
        ChessPieceKind::King => {
            canvas.vline(cx, cy - sc(11), sc(6), detail);
        }
        ChessPieceKind::Queen => {
            canvas.fill_circle(cx, cy - sc(6), sc(2).max(1), detail);
        }
        ChessPieceKind::Rook => {
            canvas.hline(cx - sc(6), cy - sc(5), sc(12), detail);
        }
        ChessPieceKind::Pawn => {}
    }
}

/// Draw Pocket Pip the Fox — charming children's-book 4-tone illustration!
pub fn draw_pip_fox(canvas: &mut Canvas<'_>, cx: i32, cy: i32, scale: i32, mood: PipMood, hat: PipHat) {
    let s = |v: i32| (v * scale) / 100;

    // Fluffy tail curled beside Pip (1-bpp line art)
    canvas.fill_circle(cx + s(46), cy + s(32), s(24), Tone::Paper);
    canvas.stroke_circle(cx + s(46), cy + s(32), s(24), 2, Tone::Ink);
    canvas.fill_circle(cx + s(62), cy + s(24), s(13), Tone::Paper);
    canvas.stroke_circle(cx + s(62), cy + s(24), s(13), 2, Tone::Ink);

    // Body
    canvas.fill_rrect(cx - s(32), cy + s(6), s(64), s(58), s(26), Tone::Paper);
    canvas.stroke_rrect(cx - s(32), cy + s(6), s(64), s(58), s(26), 2, Tone::Ink);
    // Chest bib
    canvas.stroke_rrect(cx - s(18), cy + s(16), s(36), s(40), s(16), 1, Tone::Ink);

    // Little paws
    canvas.fill_circle(cx - s(20), cy + s(62), s(9), Tone::Ink);
    canvas.fill_circle(cx + s(20), cy + s(62), s(9), Tone::Ink);

    // Fox ears (outer Ink, mid Paper, inner Ink accent for 1-bpp contrast)
    fill_triangle(canvas, cx - s(44), cy - s(16), cx - s(48), cy - s(64), cx - s(12), cy - s(36), Tone::Ink);
    fill_triangle(canvas, cx - s(42), cy - s(18), cx - s(45), cy - s(58), cx - s(15), cy - s(36), Tone::Paper);
    fill_triangle(canvas, cx - s(38), cy - s(24), cx - s(41), cy - s(46), cx - s(23), cy - s(34), Tone::Ink);

    fill_triangle(canvas, cx + s(44), cy - s(16), cx + s(48), cy - s(64), cx + s(12), cy - s(36), Tone::Ink);
    fill_triangle(canvas, cx + s(42), cy - s(18), cx + s(45), cy - s(58), cx + s(15), cy - s(36), Tone::Paper);
    fill_triangle(canvas, cx + s(38), cy - s(24), cx + s(41), cy - s(46), cx + s(23), cy - s(34), Tone::Ink);

    // Head oval
    canvas.fill_rrect(cx - s(46), cy - s(42), s(92), s(68), s(32), Tone::Paper);
    canvas.stroke_rrect(cx - s(46), cy - s(42), s(92), s(68), s(32), 2, Tone::Ink);

    // Eyes & expression
    match mood {
        PipMood::Sleeping => {
            canvas.draw_line(cx - s(26), cy - s(12), cx - s(12), cy - s(10), Tone::Ink);
            canvas.draw_line(cx + s(12), cy - s(10), cx + s(26), cy - s(12), Tone::Ink);
            canvas.draw_line(cx - s(26), cy - s(11), cx - s(12), cy - s(9), Tone::Ink);
            canvas.draw_line(cx + s(12), cy - s(9), cx + s(26), cy - s(11), Tone::Ink);
        }
        PipMood::Celebrating => {
            // Happy "^ ^" arcs
            canvas.draw_line(cx - s(26), cy - s(10), cx - s(18), cy - s(17), Tone::Ink);
            canvas.draw_line(cx - s(18), cy - s(17), cx - s(10), cy - s(10), Tone::Ink);
            canvas.draw_line(cx + s(10), cy - s(10), cx + s(18), cy - s(17), Tone::Ink);
            canvas.draw_line(cx + s(18), cy - s(17), cx + s(26), cy - s(10), Tone::Ink);
        }
        _ => {
            canvas.fill_circle(cx - s(19), cy - s(13), s(6), Tone::Ink);
            canvas.fill_circle(cx + s(19), cy - s(13), s(6), Tone::Ink);
            // Catchlight sparkle
            canvas.fill_circle(cx - s(21), cy - s(15), s(2), Tone::Paper);
            canvas.fill_circle(cx + s(17), cy - s(15), s(2), Tone::Paper);
        }
    }

    // Little triangle nose + smile
    fill_triangle(canvas, cx - s(6), cy + s(2), cx + s(6), cy + s(2), cx, cy + s(9), Tone::Ink);
    canvas.draw_line(cx, cy + s(9), cx - s(6), cy + s(14), Tone::Ink);
    canvas.draw_line(cx, cy + s(9), cx + s(6), cy + s(14), Tone::Ink);

    // Optional mini-book when reading
    if mood == PipMood::Reading {
        canvas.fill_rrect(cx - s(30), cy + s(32), s(60), s(26), 4, Tone::Paper);
        canvas.stroke_rrect(cx - s(30), cy + s(32), s(60), s(26), 4, 2, Tone::Ink);
        canvas.vline(cx, cy + s(34), s(22), Tone::Ink);
        canvas.hline(cx - s(24), cy + s(40), s(18), Tone::Ink);
        canvas.hline(cx - s(24), cy + s(46), s(16), Tone::Ink);
        canvas.hline(cx + s(6), cy + s(40), s(18), Tone::Ink);
        canvas.hline(cx + s(6), cy + s(46), s(16), Tone::Ink);
    }

    // Unlockable hats!
    let hat_y = cy - s(42);
    match hat {
        PipHat::None => {}
        PipHat::FlowerCrown => {
            for (fx, fy) in [(-s(24), hat_y), (0, hat_y - s(4)), (s(24), hat_y)] {
                for p in 0..5 {
                    let (dx, dy) = sin_cos_deg_approx(p * 72);
                    canvas.fill_circle(cx + fx + (dx * s(7)) / 100, fy + (dy * s(7)) / 100, s(4), Tone::Paper);
                    canvas.stroke_circle(cx + fx + (dx * s(7)) / 100, fy + (dy * s(7)) / 100, s(4), 1, Tone::Ink);
                }
                canvas.fill_circle(cx + fx, fy, s(3), Tone::Ink);
            }
        }
        PipHat::WizardHat => {
            canvas.fill_rrect(cx - s(42), hat_y - s(4), s(84), s(10), 4, Tone::Ink);
            fill_triangle(canvas, cx - s(28), hat_y - s(2), cx + s(28), hat_y - s(2), cx + s(6), hat_y - s(56), Tone::Ink);
            // Moon & stars on wizard hat
            canvas.fill_circle(cx - s(4), hat_y - s(24), s(5), Tone::Paper);
            canvas.fill_circle(cx - s(1), hat_y - s(25), s(4), Tone::Ink);
            canvas.fill_circle(cx + s(8), hat_y - s(36), s(2), Tone::Paper);
        }
        PipHat::SpaceHelmet => {
            canvas.stroke_circle(cx, cy - s(10), s(58), 2, Tone::Ink);
        }
        PipHat::DetectiveCap => {
            canvas.fill_rrect(cx - s(36), hat_y - s(18), s(72), s(22), 10, Tone::Paper);
            canvas.stroke_rrect(cx - s(36), hat_y - s(18), s(72), s(22), 10, 2, Tone::Ink);
            canvas.fill_rrect(cx - s(44), hat_y - s(2), s(88), s(8), 3, Tone::Ink);
        }
        PipHat::RoyalTiara => {
            fill_triangle(canvas, cx - s(24), hat_y + 1, cx - s(6), hat_y + 1, cx - s(18), hat_y - s(24), Tone::Ink);
            fill_triangle(canvas, cx - s(12), hat_y + 1, cx + s(12), hat_y + 1, cx, hat_y - s(30), Tone::Ink);
            fill_triangle(canvas, cx + s(6), hat_y + 1, cx + s(24), hat_y + 1, cx + s(18), hat_y - s(24), Tone::Ink);
            fill_triangle(canvas, cx - s(20), hat_y - 2, cx - s(10), hat_y - 2, cx - s(18), hat_y - s(18), Tone::Paper);
            fill_triangle(canvas, cx - s(8), hat_y - 2, cx + s(8), hat_y - 2, cx, hat_y - s(22), Tone::Paper);
            fill_triangle(canvas, cx + s(10), hat_y - 2, cx + s(20), hat_y - 2, cx + s(18), hat_y - s(18), Tone::Paper);
            canvas.fill_rect(cx - s(22), hat_y - s(4), s(44), s(5), Tone::Ink);
            canvas.fill_circle(cx, hat_y - s(12), s(3), Tone::Ink);
        }
    }
}

/// Crisp 4-pointed or 5-pointed starberry / sparkle glyph for Pocket Pip & rewards.
pub fn draw_starberry(canvas: &mut Canvas<'_>, cx: i32, cy: i32, r: i32, filled: bool) {
    if filled {
        canvas.fill_circle(cx, cy + 2, r, Tone::Charcoal);
        canvas.stroke_circle(cx, cy + 2, r, 2, Tone::Ink);
        // Cute leaf crown on top
        fill_triangle(canvas, cx - r / 2, cy - r + 2, cx + r / 2, cy - r + 2, cx, cy - r - 5, Tone::Ink);
        // Sparkle highlight
        canvas.fill_circle(cx - r / 3, cy - 1, (r / 4).max(1), Tone::Paper);
    } else {
        canvas.stroke_circle(cx, cy + 2, r, 1, Tone::Charcoal);
    }
}

#[allow(clippy::too_many_arguments)]
pub fn fill_triangle(canvas: &mut Canvas<'_>, x0: i32, y0: i32, x1: i32, y1: i32, x2: i32, y2: i32, tone: Tone) {
    let mut pts = [(x0, y0), (x1, y1), (x2, y2)];
    pts.sort_by_key(|p| p.1);
    let (ax, ay) = pts[0];
    let (bx, by) = pts[1];
    let (cx, cy) = pts[2];
    if ay == cy {
        let min_x = ax.min(bx).min(cx);
        let max_x = ax.max(bx).max(cx);
        canvas.hline(min_x, ay, max_x - min_x + 1, tone);
        return;
    }
    let total_h = cy - ay;
    for y in ay..=cy {
        if !canvas.intersects_y(y, y + 1) {
            continue;
        }
        let second_half = y > by || by == ay;
        let seg_h = if second_half { cy - by } else { by - ay }.max(1);
        let alpha = y - ay;
        let beta = if second_half { y - by } else { y - ay };
        let xa = ax + ((cx - ax) * alpha) / total_h;
        let xb = if second_half {
            bx + ((cx - bx) * beta) / seg_h
        } else {
            ax + ((bx - ax) * beta) / seg_h
        };
        let (lx, rx) = if xa < xb { (xa, xb) } else { (xb, xa) };
        canvas.hline(lx, y, rx - lx + 1, tone);
    }
}

pub fn fill_trapezoid(canvas: &mut Canvas<'_>, cx: i32, bottom_y: i32, bottom_w: i32, top_w: i32, h: i32, tone: Tone) {
    if h <= 0 {
        return;
    }
    for row in 0..h {
        let y = bottom_y - row;
        if !canvas.intersects_y(y, y + 1) {
            continue;
        }
        let w = bottom_w + ((top_w - bottom_w) * row) / h;
        canvas.hline(cx - w / 2, y, w.max(1), tone);
    }
}

/// Fast integer sin/cos * 100 approximation for angles in degrees.
fn sin_cos_deg_approx(deg: i32) -> (i32, i32) {
    let mut a = deg % 360;
    if a < 0 {
        a += 360;
    }
    let sin_q = |x: i32| -> i32 {
        // Bhaskara I approximation on [0..=90]
        let num = 4 * x * (180 - x) * 100;
        let den = 40500 - x * (180 - x);
        num / den
    };
    let s = if a <= 180 { sin_q(a) } else { -sin_q(a - 180) };
    let c_deg = (a + 90) % 360;
    let c = if c_deg <= 180 { sin_q(c_deg) } else { -sin_q(c_deg - 180) };
    (s, c)
}
