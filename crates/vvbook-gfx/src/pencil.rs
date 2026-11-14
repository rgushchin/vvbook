use crate::canvas::Canvas;
use vvbook_pal::Tone;

#[inline]
fn div_round(num: i64, den: i64) -> i32 {
    if num >= 0 {
        ((num + den / 2) / den) as i32
    } else {
        ((num - den / 2) / den) as i32
    }
}

/// Evaluate quadratic Bézier point at parameter `t / steps`.
#[inline]
fn eval_quad(p0: (i32, i32), p1: (i32, i32), p2: (i32, i32), t: i32, steps: i32) -> (i32, i32) {
    let i = t as i64;
    let n = steps.max(1) as i64;
    let u = n - i;
    let den = n * n;
    let x = div_round(u * u * p0.0 as i64 + 2 * u * i * p1.0 as i64 + i * i * p2.0 as i64, den);
    let y = div_round(u * u * p0.1 as i64 + 2 * u * i * p1.1 as i64 + i * i * p2.1 as i64, den);
    (x, y)
}

/// Draw a smooth integer quadratic Bézier curve with constant stroke thickness (`>= 1`).
#[allow(clippy::too_many_arguments)]
pub fn quad_bezier(
    c: &mut Canvas,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    x2: i32,
    y2: i32,
    steps: i32,
    thickness: i32,
    tone: Tone,
) {
    let min_y = y0.min(y1).min(y2) - thickness - 1;
    let max_y = y0.max(y1).max(y2) + thickness + 2;
    if !c.intersects_y(min_y, max_y) {
        return;
    }
    let n = steps.max(2);
    let (mut px, mut py) = (x0, y0);
    for t in 1..=n {
        let (nx, ny) = eval_quad((x0, y0), (x1, y1), (x2, y2), t, n);
        c.draw_line(px, py, nx, ny, tone);
        if thickness >= 2 {
            let r = thickness / 2;
            c.fill_circle(nx, ny, r, tone);
            if (nx - px).abs() + (ny - py).abs() > 2 {
                c.fill_circle((px + nx) / 2, (py + ny) / 2, r, tone);
            }
        }
        px = nx;
        py = ny;
    }
}

/// Draw a tapered quadratic Bézier curve swelling from `0` at endpoints to `max_r`.
#[allow(clippy::too_many_arguments)]
pub fn tapered_quad_bezier(
    c: &mut Canvas,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    x2: i32,
    y2: i32,
    steps: i32,
    max_r: i32,
    tone: Tone,
) {
    let pad = max_r.max(1) + 2;
    let min_y = y0.min(y1).min(y2) - pad;
    let max_y = y0.max(y1).max(y2) + pad + 1;
    if !c.intersects_y(min_y, max_y) {
        return;
    }
    let n = steps.max(4) as i64;
    let den = n * n;
    let (mut px, mut py) = (x0, y0);
    for t in 1..=n as i32 {
        let ti = t as i64;
        let (nx, ny) = eval_quad((x0, y0), (x1, y1), (x2, y2), t, n as i32);
        let r = div_round(4 * ti * (n - ti) * max_r as i64, den);
        c.draw_line(px, py, nx, ny, tone);
        if r > 0 {
            c.fill_circle(nx, ny, r, tone);
            if (nx - px).abs() + (ny - py).abs() > 2 {
                c.fill_circle((px + nx) / 2, (py + ny) / 2, r, tone);
            }
        }
        px = nx;
        py = ny;
    }
}
