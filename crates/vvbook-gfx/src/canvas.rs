use crate::font::BakedFont;
use vvbook_pal::{DisplaySink, PORTRAIT_H, PORTRAIT_W, ROW_2BPP_BYTES, RefreshMode, STRIP_ROWS, Tone};

/// Xteink X3 physical bottom-chin button horizontal slots (`528 px` portrait panel).
/// Aligned directly above the 4 front bezel buttons (`Back`, `Confirm`, `Left`, `Right`).
pub const X3_CHIN_X: [i32; 4] = [38, 154, 268, 384];
pub const X3_CHIN_W: i32 = 106;
pub const X3_CHIN_Y: i32 = 740;
pub const X3_CHIN_H: i32 = 44;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

/// Strip-capable 2-bpp canvas (`528 × strip_rows`, anchored at `y_offset`).
/// Any UI or game scene renders against `&mut Canvas` identically whether backed
/// by a full 104 KB host frame or an 8.4 KB 64-row ESP32-C3 strip!
pub struct Canvas<'a> {
    buf: &'a mut [u8],
    y_offset: usize,
    rows: usize,
}

impl<'a> Canvas<'a> {
    pub fn new(buf: &'a mut [u8], y_offset: usize, rows: usize) -> Self {
        debug_assert!(buf.len() >= rows * ROW_2BPP_BYTES);
        Self {
            buf,
            y_offset,
            rows,
        }
    }

    #[inline]
    pub fn y_offset(&self) -> usize {
        self.y_offset
    }

    #[inline]
    pub fn strip_rows(&self) -> usize {
        self.rows
    }

    #[inline]
    pub fn intersects_y(&self, y0: i32, y1_excl: i32) -> bool {
        let s0 = self.y_offset as i32;
        let s1 = (self.y_offset + self.rows) as i32;
        y1_excl > s0 && y0 < s1
    }

    pub fn clear(&mut self, tone: Tone) {
        let t = tone as u8;
        let packed = (t << 6) | (t << 4) | (t << 2) | t;
        self.buf[..self.rows * ROW_2BPP_BYTES].fill(packed);
    }

    #[inline]
    pub fn get_pixel(&self, x: i32, y: i32) -> Tone {
        if x < 0 || x >= PORTRAIT_W as i32 {
            return Tone::Paper;
        }
        let ly = y - self.y_offset as i32;
        if ly < 0 || ly >= self.rows as i32 {
            return Tone::Paper;
        }
        let ux = x as usize;
        let uy = ly as usize;
        let idx = uy * ROW_2BPP_BYTES + (ux >> 2);
        let shift = 6 - ((ux & 3) << 1);
        Tone::from_bits(self.buf[idx] >> shift)
    }

    #[inline]
    pub fn set_pixel(&mut self, x: i32, y: i32, tone: Tone) {
        if x < 0 || x >= PORTRAIT_W as i32 {
            return;
        }
        let ly = y - self.y_offset as i32;
        if ly < 0 || ly >= self.rows as i32 {
            return;
        }
        let ux = x as usize;
        let uy = ly as usize;
        let idx = uy * ROW_2BPP_BYTES + (ux >> 2);
        let shift = 6 - ((ux & 3) << 1);
        let mask = !(0x03u8 << shift);
        self.buf[idx] = (self.buf[idx] & mask) | ((tone as u8) << shift);
    }

    /// Blend 2-bpp glyph coverage (0..=3) with foreground `ink` against existing background.
    ///
    /// Preserves dark/light polarity across both 1-bpp B/W (`is_dark()`) and 2-bpp Grayscale4
    /// waveforms so `cov == 2` stems on `Tone::Charcoal` or `Tone::Paper` never erode away.
    #[inline]
    pub fn blend_coverage(&mut self, x: i32, y: i32, cov: u8, ink: Tone) {
        if cov == 0 {
            return;
        }
        if cov >= 3 {
            self.set_pixel(x, y, ink);
            return;
        }
        let bg = self.get_pixel(x, y);
        let out = if ink.is_dark() && !bg.is_dark() {
            if cov == 2 {
                Tone::Charcoal
            } else {
                Tone::Silver
            }
        } else if !ink.is_dark() && bg.is_dark() {
            if cov == 2 {
                Tone::Silver
            } else {
                Tone::Charcoal
            }
        } else {
            let b = bg as i32;
            let f = ink as i32;
            let lin = (b * (3 - cov as i32) + f * (cov as i32) + 1) / 3;
            Tone::from_bits(lin.clamp(0, 3) as u8)
        };
        self.set_pixel(x, y, out);
    }

    pub fn hline(&mut self, x: i32, y: i32, w: i32, tone: Tone) {
        if w <= 0 {
            return;
        }
        let ly = y - self.y_offset as i32;
        if ly < 0 || ly >= self.rows as i32 {
            return;
        }
        let mut px = x.max(0);
        let x1 = (x + w).min(PORTRAIT_W as i32);
        if px >= x1 {
            return;
        }
        if x1 - px < 8 {
            while px < x1 {
                self.set_pixel(px, y, tone);
                px += 1;
            }
            return;
        }
        while (px & 3) != 0 {
            self.set_pixel(px, y, tone);
            px += 1;
        }
        let t = tone as u8 & 0x03;
        let packed = (t << 6) | (t << 4) | (t << 2) | t;
        let row_base = (ly as usize) * ROW_2BPP_BYTES;
        let b0 = row_base + ((px as usize) >> 2);
        let b1 = row_base + ((x1 as usize) >> 2);
        if b0 < b1 {
            self.buf[b0..b1].fill(packed);
        }
        for tail_x in (x1 & !3)..x1 {
            self.set_pixel(tail_x, y, tone);
        }
    }

    pub fn vline(&mut self, x: i32, y: i32, h: i32, tone: Tone) {
        if h <= 0 {
            return;
        }
        let y0 = y.max(self.y_offset as i32);
        let y1 = (y + h).min((self.y_offset + self.rows) as i32);
        for py in y0..y1 {
            self.set_pixel(x, py, tone);
        }
    }

    pub fn hline_dotted(&mut self, x: i32, y: i32, w: i32, step: i32, tone: Tone) {
        if w <= 0 {
            return;
        }
        let ly = y - self.y_offset as i32;
        if ly < 0 || ly >= self.rows as i32 {
            return;
        }
        let step = step.max(1);
        let x0 = x.max(0);
        let x1 = (x + w).min(PORTRAIT_W as i32);
        for px in x0..x1 {
            if (px - x).rem_euclid(step) == 0 {
                self.set_pixel(px, y, tone);
            }
        }
    }

    pub fn vline_dotted(&mut self, x: i32, y: i32, h: i32, step: i32, tone: Tone) {
        if h <= 0 || x < 0 || x >= PORTRAIT_W as i32 {
            return;
        }
        let step = step.max(1);
        let y0 = y.max(self.y_offset as i32);
        let y1 = (y + h).min((self.y_offset + self.rows) as i32);
        for py in y0..y1 {
            if (py - y).rem_euclid(step) == 0 {
                self.set_pixel(x, py, tone);
            }
        }
    }

    /// 25% dot-matrix texture (global coordinates for strip parity & 1-bpp visibility).
    pub fn fill_stipple_25(&mut self, x: i32, y: i32, w: i32, h: i32, fg: Tone, bg: Tone) {
        if w <= 0 || h <= 0 || !self.intersects_y(y, y + h) {
            return;
        }
        let x0 = x.max(0);
        let x1 = (x + w).min(PORTRAIT_W as i32);
        if x0 >= x1 {
            return;
        }
        let y0 = y.max(self.y_offset as i32);
        let y1 = (y + h).min((self.y_offset + self.rows) as i32);
        let span = x1 - x0;
        for py in y0..y1 {
            self.hline(x0, py, span, bg);
            if (py & 1) == 0 {
                let mut px = x0 + (x0 & 1);
                while px < x1 {
                    self.set_pixel(px, py, fg);
                    px += 2;
                }
            }
        }
    }

    /// 50% checkerboard texture (global coordinates for strip parity & 1-bpp visibility).
    pub fn fill_checker_50(&mut self, x: i32, y: i32, w: i32, h: i32, fg: Tone, bg: Tone) {
        if w <= 0 || h <= 0 || !self.intersects_y(y, y + h) {
            return;
        }
        let x0 = x.max(0);
        let x1 = (x + w).min(PORTRAIT_W as i32);
        if x0 >= x1 {
            return;
        }
        let y0 = y.max(self.y_offset as i32);
        let y1 = (y + h).min((self.y_offset + self.rows) as i32);
        let span = x1 - x0;
        for py in y0..y1 {
            self.hline(x0, py, span, bg);
            let start_odd = (py & 1) != (x0 & 1);
            let mut px = x0 + if start_odd { 1 } else { 0 };
            while px < x1 {
                self.set_pixel(px, py, fg);
                px += 2;
            }
        }
    }

    pub fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, tone: Tone) {
        if w <= 0 || h <= 0 || !self.intersects_y(y, y + h) {
            return;
        }
        let x0 = x.max(0);
        let x1 = (x + w).min(PORTRAIT_W as i32);
        if x0 >= x1 {
            return;
        }
        let y0 = y.max(self.y_offset as i32);
        let y1 = (y + h).min((self.y_offset + self.rows) as i32);
        let span = x1 - x0;
        for py in y0..y1 {
            self.hline(x0, py, span, tone);
        }
    }

    pub fn stroke_rect(&mut self, x: i32, y: i32, w: i32, h: i32, thickness: i32, tone: Tone) {
        if w <= 0 || h <= 0 || thickness <= 0 {
            return;
        }
        self.fill_rect(x, y, w, thickness, tone);
        self.fill_rect(x, y + h - thickness, w, thickness, tone);
        self.fill_rect(x, y, thickness, h, tone);
        self.fill_rect(x + w - thickness, y, thickness, h, tone);
    }

    pub fn fill_rrect(&mut self, x: i32, y: i32, w: i32, h: i32, radius: i32, tone: Tone) {
        if w <= 0 || h <= 0 || !self.intersects_y(y, y + h) {
            return;
        }
        let r = radius.min(w / 2).min(h / 2).max(0);
        if r == 0 {
            self.fill_rect(x, y, w, h, tone);
            return;
        }
        let y0 = y.max(self.y_offset as i32);
        let y1 = (y + h).min((self.y_offset + self.rows) as i32);
        let r_sq = r * r;
        for py in y0..y1 {
            let dy_top = y + r - py;
            let dy_bot = py - (y + h - 1 - r);
            let dy = if dy_top > 0 {
                dy_top
            } else if dy_bot > 0 {
                dy_bot
            } else {
                0
            };
            let inset = if dy == 0 {
                0
            } else {
                // Integer sqrt approximation for exact crisp quarter-circle
                let rem = (r_sq - dy * dy).max(0);
                r - isqrt(rem)
            };
            self.hline(x + inset, py, w - 2 * inset, tone);
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn stroke_rrect(&mut self, x: i32, y: i32, w: i32, h: i32, radius: i32, thickness: i32, tone: Tone) {
        if w <= 0 || h <= 0 || thickness <= 0 || !self.intersects_y(y, y + h) {
            return;
        }
        let r = radius.min(w / 2).min(h / 2).max(0);
        let inner_r = (r - thickness).max(0);
        let y0 = y.max(self.y_offset as i32);
        let y1 = (y + h).min((self.y_offset + self.rows) as i32);
        let r_sq = r * r;
        let ir_sq = inner_r * inner_r;
        for py in y0..y1 {
            let dy_top = y + r - py;
            let dy_bot = py - (y + h - 1 - r);
            let dy = dy_top.max(dy_bot).max(0);
            let outer_inset = if dy == 0 {
                0
            } else {
                r - isqrt((r_sq - dy * dy).max(0))
            };
            let in_top = py < y + thickness;
            let in_bot = py >= y + h - thickness;
            if in_top || in_bot {
                self.hline(x + outer_inset, py, w - 2 * outer_inset, tone);
            } else {
                let idy_top = (y + thickness + inner_r) - py;
                let idy_bot = py - (y + h - thickness - 1 - inner_r);
                let idy = idy_top.max(idy_bot).max(0);
                let inner_inset = if idy == 0 {
                    thickness
                } else {
                    thickness + (inner_r - isqrt((ir_sq - idy * idy).max(0)))
                };
                let left_w = (inner_inset - outer_inset).max(1);
                self.hline(x + outer_inset, py, left_w, tone);
                self.hline(x + w - outer_inset - left_w, py, left_w, tone);
            }
        }
    }

    /// Subtle Cupertino-style dithered drop shadow under floating cards and sheets.
    pub fn draw_drop_shadow(&mut self, x: i32, y: i32, w: i32, h: i32, radius: i32) {
        if !self.intersects_y(y + 1, y + h + 6) {
            return;
        }
        let r = radius.min(w / 2).min(h / 2).max(0);
        let r_sq = r * r;
        let sy = y + 3;
        let sx = x + 2;
        let y0 = sy.max(self.y_offset as i32);
        let y1 = (sy + h).min((self.y_offset + self.rows) as i32);
        for py in y0..y1 {
            let dy_top = sy + r - py;
            let dy_bot = py - (sy + h - 1 - r);
            let dy = dy_top.max(dy_bot).max(0);
            let inset = if dy == 0 {
                0
            } else {
                r - isqrt((r_sq - dy * dy).max(0))
            };
            for px in (sx + inset).max(0)..(sx + w - inset).min(PORTRAIT_W as i32) {
                // Skip interior of card itself to save work
                if px >= x + 2 && px < x + w - 2 && py >= y + 2 && py < y + h - 2 {
                    continue;
                }
                if ((px + py) & 1) == 0 {
                    self.set_pixel(px, py, Tone::Silver);
                }
            }
        }
    }

    /// Crisp 50% checkerboard dither scrim for dimming background under modal HUDs.
    pub fn dim_scrim(&mut self) {
        const fn build_lut(even_px: bool) -> [u8; 256] {
            let mut lut = [0u8; 256];
            let mut b = 0usize;
            while b < 256 {
                let p0 = ((b >> 6) & 0x03) as u8;
                let p1 = ((b >> 4) & 0x03) as u8;
                let p2 = ((b >> 2) & 0x03) as u8;
                let p3 = (b & 0x03) as u8;
                let n0 = if even_px { p0.saturating_sub(1) } else { p0 };
                let n1 = if !even_px { p1.saturating_sub(1) } else { p1 };
                let n2 = if even_px { p2.saturating_sub(1) } else { p2 };
                let n3 = if !even_px { p3.saturating_sub(1) } else { p3 };
                lut[b] = (n0 << 6) | (n1 << 4) | (n2 << 2) | n3;
                b += 1;
            }
            lut
        }
        const DIM_EVEN_LUT: [u8; 256] = build_lut(true);
        const DIM_ODD_LUT: [u8; 256] = build_lut(false);

        for ly in 0..self.rows {
            let py = self.y_offset + ly;
            let lut = if (py & 1) == 0 { &DIM_EVEN_LUT } else { &DIM_ODD_LUT };
            let row = &mut self.buf[ly * ROW_2BPP_BYTES..(ly + 1) * ROW_2BPP_BYTES];
            for b in row {
                *b = lut[*b as usize];
            }
        }
    }

    pub fn fill_circle(&mut self, cx: i32, cy: i32, r: i32, tone: Tone) {
        if r <= 0 || !self.intersects_y(cy - r, cy + r + 1) {
            return;
        }
        let r_sq = r * r;
        let y0 = (cy - r).max(self.y_offset as i32);
        let y1 = (cy + r + 1).min((self.y_offset + self.rows) as i32);
        for py in y0..y1 {
            let dy = py - cy;
            let dx = isqrt((r_sq - dy * dy).max(0));
            self.hline(cx - dx, py, dx * 2 + 1, tone);
        }
    }

    pub fn stroke_circle(&mut self, cx: i32, cy: i32, r: i32, thickness: i32, tone: Tone) {
        if r <= 0 || !self.intersects_y(cy - r, cy + r + 1) {
            return;
        }
        let outer_sq = r * r;
        let ir = (r - thickness).max(0);
        let inner_sq = ir * ir;
        let y0 = (cy - r).max(self.y_offset as i32);
        let y1 = (cy + r + 1).min((self.y_offset + self.rows) as i32);
        for py in y0..y1 {
            let dy = py - cy;
            let dy_sq = dy * dy;
            let odx = isqrt((outer_sq - dy_sq).max(0));
            if dy_sq >= inner_sq {
                self.hline(cx - odx, py, odx * 2 + 1, tone);
            } else {
                let idx = isqrt((inner_sq - dy_sq).max(0));
                let w = (odx - idx).max(1);
                self.hline(cx - odx, py, w, tone);
                self.hline(cx + idx + 1, py, w, tone);
            }
        }
    }

    pub fn draw_line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, tone: Tone) {
        let mut x = x0;
        let mut y = y0;
        let dx = (x1 - x0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let dy = -(y1 - y0).abs();
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;
        loop {
            self.set_pixel(x, y, tone);
            if x == x1 && y == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x += sx;
            }
            if e2 <= dx {
                err += dx;
                y += sy;
            }
        }
    }

    /// Draw proportional text anchored at baseline `(x, baseline_y)`.
    pub fn draw_text(&mut self, font: &BakedFont, x: i32, baseline_y: i32, text: &str, tone: Tone) -> i32 {
        self.draw_text_tracked(font, x, baseline_y, text, tone, 0)
    }

    pub fn draw_text_tracked(
        &mut self,
        font: &BakedFont,
        x: i32,
        baseline_y: i32,
        text: &str,
        tone: Tone,
        tracking_q4: i32,
    ) -> i32 {
        // Early strip rejection
        let top = baseline_y - font.ascent as i32 - 4;
        let bot = baseline_y - font.descent as i32 + 4;
        if !self.intersects_y(top, bot) {
            return x + font.measure_text_tracked(text, tracking_q4);
        }

        let mut cur_x_q4 = x << 4;
        let mut prev: Option<usize> = None;
        for ch in text.chars() {
            if let Some((idx, g)) = font.glyph(ch) {
                if let Some(p) = prev {
                    cur_x_q4 += font.kern_q4(p, idx) as i32;
                    cur_x_q4 += tracking_q4;
                }
                let gx = ((cur_x_q4 + 8) >> 4) + g.xmin as i32;
                let gy = baseline_y - g.ymin as i32 - g.height as i32;
                let gw = g.width as usize;
                let gh = g.height as usize;
                if gw > 0 && gh > 0 && self.intersects_y(gy, gy + gh as i32) {
                    let base = g.offset as usize;
                    let mut p_idx = 0usize;
                    for row in 0..gh {
                        let py = gy + row as i32;
                        let in_strip = py >= self.y_offset as i32 && py < (self.y_offset + self.rows) as i32;
                        if in_strip {
                            for col in 0..gw {
                                let byte = font.atlas[base + (p_idx >> 2)];
                                let shift = 6 - ((p_idx & 3) << 1);
                                let cov = (byte >> shift) & 0x03;
                                if cov != 0 {
                                    self.blend_coverage(gx + col as i32, py, cov, tone);
                                }
                                p_idx += 1;
                            }
                        } else {
                            p_idx += gw;
                        }
                    }
                }
                cur_x_q4 += g.advance_q4 as i32;
                prev = Some(idx);
            }
        }
        (cur_x_q4 + 8) >> 4
    }

    pub fn draw_text_aligned(
        &mut self,
        font: &BakedFont,
        x: i32,
        baseline_y: i32,
        text: &str,
        tone: Tone,
        align: Align,
    ) {
        let top = baseline_y - font.ascent as i32 - 4;
        let bot = baseline_y - font.descent as i32 + 4;
        if !self.intersects_y(top, bot) {
            return;
        }
        let draw_x = match align {
            Align::Left => x,
            Align::Center => x - font.measure_text(text) / 2,
            Align::Right => x - font.measure_text(text),
        };
        self.draw_text(font, draw_x, baseline_y, text, tone);
    }

    pub fn draw_text_truncated(
        &mut self,
        font: &BakedFont,
        x: i32,
        baseline_y: i32,
        max_w: i32,
        text: &str,
        tone: Tone,
    ) {
        let top = baseline_y - font.ascent as i32 - 4;
        let bot = baseline_y - font.descent as i32 + 4;
        if !self.intersects_y(top, bot) {
            return;
        }
        if font.measure_text(text) <= max_w {
            self.draw_text(font, x, baseline_y, text, tone);
            return;
        }
        let ell_w = font.measure_text("…");
        let budget = (max_w - ell_w).max(0);
        let mut end = 0usize;
        let mut cur_w_q4 = 0i32;
        let mut prev: Option<usize> = None;
        for (byte_idx, ch) in text.char_indices() {
            if let Some((idx, g)) = font.glyph(ch) {
                let k = prev.map_or(0, |p| font.kern_q4(p, idx) as i32);
                let next_q4 = cur_w_q4 + k + g.advance_q4 as i32;
                if ((next_q4 + 8) >> 4) > budget {
                    break;
                }
                cur_w_q4 = next_q4;
                end = byte_idx + ch.len_utf8();
                prev = Some(idx);
            }
        }
        let next_x = self.draw_text(font, x, baseline_y, &text[..end], tone);
        self.draw_text(font, next_x, baseline_y, "…", tone);
    }

    fn compute_wrapped_lines<'b>(
        font: &BakedFont,
        max_w: i32,
        max_lines: usize,
        text: &'b str,
    ) -> alloc::vec::Vec<(&'b str, bool)> {
        let mut out = alloc::vec::Vec::new();
        if max_lines == 0 {
            return out;
        }
        let mut rem = text.trim();
        if rem.is_empty() {
            out.push(("", false));
            return out;
        }
        for line_idx in 0..max_lines {
            if rem.is_empty() {
                break;
            }
            if line_idx + 1 == max_lines {
                let trunc = font.measure_text(rem) > max_w;
                out.push((rem, trunc));
                break;
            }
            if font.measure_text(rem) <= max_w {
                out.push((rem, false));
                break;
            }
            let mut split_at = 0usize;
            let mut last_space_split = None;
            for (byte_idx, ch) in rem.char_indices() {
                let cand_end = byte_idx + ch.len_utf8();
                if ch.is_whitespace() && byte_idx > 0 && font.measure_text(&rem[..byte_idx]) <= max_w {
                    last_space_split = Some((byte_idx, cand_end));
                }
                if font.measure_text(&rem[..cand_end]) <= max_w {
                    split_at = cand_end;
                } else {
                    break;
                }
            }
            if let Some((line_end, next_start)) = last_space_split {
                out.push((&rem[..line_end], false));
                rem = rem[next_start..].trim_start();
            } else if split_at > 0 && split_at < rem.len() {
                out.push((&rem[..split_at], false));
                rem = rem[split_at..].trim_start();
            } else {
                out.push((rem, font.measure_text(rem) > max_w));
                break;
            }
        }
        out
    }

    /// Count wrapped lines (`1..=max_lines`) deterministically independent of strip bounds.
    pub fn count_wrapped_lines(font: &BakedFont, max_w: i32, max_lines: usize, text: &str) -> usize {
        Self::compute_wrapped_lines(font, max_w, max_lines, text).len()
    }

    /// Word-wrap `text` across up to `max_lines` lines within `max_w` (truncating overflow on the
    /// final line with `…`) and return the true wrapped line count consistently across all strips.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_text_lines(
        &mut self,
        font: &BakedFont,
        x: i32,
        baseline_y: i32,
        max_w: i32,
        line_step: i32,
        max_lines: usize,
        text: &str,
        tone: Tone,
    ) -> usize {
        let lines = Self::compute_wrapped_lines(font, max_w, max_lines, text);
        for (idx, &(seg, trunc)) in lines.iter().enumerate() {
            let ly = baseline_y + (idx as i32) * line_step;
            if trunc {
                self.draw_text_truncated(font, x, ly, max_w, seg, tone);
            } else {
                self.draw_text(font, x, ly, seg, tone);
            }
        }
        lines.len()
    }

    /// Word-wrap `text` across up to 2 lines within `max_w` (truncating line 2 with `…` if needed)
    /// and return the number of lines drawn (`1` or `2`).
    #[allow(clippy::too_many_arguments)]
    pub fn draw_text_two_lines(
        &mut self,
        font: &BakedFont,
        x: i32,
        baseline_y: i32,
        max_w: i32,
        line_step: i32,
        text: &str,
        tone: Tone,
    ) -> i32 {
        self.draw_text_lines(font, x, baseline_y, max_w, line_step, 2, text, tone) as i32
    }

    /// Draw the 4-slot physical Chin Keycap Bar aligned directly above X3 hardware chin buttons
    /// (`Back`, `Confirm`, `Left`, `Right` at `X3_CHIN_X = [38, 154, 268, 384]`, `w = 106`).
    pub fn draw_chin_bar(
        &mut self,
        font: &BakedFont,
        labels: [&str; 4],
        primary_slot: Option<usize>,
    ) {
        if !self.intersects_y(738, PORTRAIT_H as i32) {
            return;
        }
        self.fill_rect(0, 738, PORTRAIT_W as i32, PORTRAIT_H as i32 - 738, Tone::Paper);
        self.hline_dotted(36, 742, PORTRAIT_W as i32 - 72, 2, Tone::Ink);

        for (i, &kx) in X3_CHIN_X.iter().enumerate() {
            let lbl = labels[i];
            if lbl.is_empty() || lbl == "—" {
                continue;
            }
            let cx = kx + X3_CHIN_W / 2;
            let by = 773;
            if primary_slot == Some(i) {
                self.fill_rect(kx + 8, 740, X3_CHIN_W - 16, 4, Tone::Ink);
            }
            self.draw_chin_label(font, kx, cx, by, lbl, Tone::Ink);
        }
    }

    fn draw_chin_label(
        &mut self,
        font: &BakedFont,
        kx: i32,
        cx: i32,
        baseline_y: i32,
        lbl: &str,
        tone: Tone,
    ) {
        if font.measure_text(lbl) <= X3_CHIN_W - 8 {
            self.draw_text_aligned(font, cx, baseline_y, lbl, tone, Align::Center);
        } else {
            self.draw_text_truncated(font, kx + 5, baseline_y, X3_CHIN_W - 10, lbl, tone);
        }
    }

    /// Draw opposing upper-left (`Button::Up`) and upper-right (`Button::Down`) edge shoulder tabs
    /// matching Xteink X3 physical side-edge button placement (`y = 168..224`).
    pub fn draw_shoulder_pills(&mut self, font: &BakedFont, left_label: &str, right_label: &str) {
        if !self.intersects_y(168, 224) {
            return;
        }
        self.fill_rect(4, 168, 28, 56, Tone::Ink);
        self.draw_text_aligned(font, 18, 203, left_label, Tone::Paper, Align::Center);

        let rx = PORTRAIT_W as i32 - 32;
        self.fill_rect(rx, 168, 28, 56, Tone::Ink);
        self.draw_text_aligned(font, rx + 14, 203, right_label, Tone::Paper, Align::Center);
    }

    /// Blit a packed 2-bpp image (`2 bits per pixel`, MSB-first, 4 pixels/byte).
    pub fn blit_2bpp(&mut self, dx: i32, dy: i32, w: usize, h: usize, data: &[u8]) {
        if !self.intersects_y(dy, dy + h as i32) {
            return;
        }
        let mut p_idx = 0usize;
        for row in 0..h {
            let py = dy + row as i32;
            let in_strip = py >= self.y_offset as i32 && py < (self.y_offset + self.rows) as i32;
            if in_strip {
                for col in 0..w {
                    let byte_idx = p_idx >> 2;
                    if byte_idx < data.len() {
                        let shift = 6 - ((p_idx & 3) << 1);
                        let tone = Tone::from_bits(data[byte_idx] >> shift);
                        self.set_pixel(dx + col as i32, py, tone);
                    }
                    p_idx += 1;
                }
            } else {
                p_idx += w;
            }
        }
    }

    /// Convert current strip pixels to packed 2-bpp bytes (for DisplaySink::write_strip_2bpp).
    pub fn raw_2bpp_bytes(&self) -> &[u8] {
        &self.buf[..self.rows * ROW_2BPP_BYTES]
    }
}

/// Helper to stream any scene closure across 64-row strips into any `DisplaySink`.
/// Uses heap-allocated `STRIP_ROWS * ROW_2BPP_BYTES` = 8,448 bytes of scratch RAM (`0` stack bytes).
pub fn render_to_sink<S: DisplaySink, F: FnMut(&mut Canvas<'_>)>(
    sink: &mut S,
    mode: RefreshMode,
    mut draw_fn: F,
) {
    sink.begin_frame(mode);
    let mut strip_buf = alloc::vec![0xFFu8; STRIP_ROWS * ROW_2BPP_BYTES];
    let mut y = 0usize;
    while y < PORTRAIT_H {
        let rows = (PORTRAIT_H - y).min(STRIP_ROWS);
        let mut canvas = Canvas::new(&mut strip_buf, y, rows);
        canvas.clear(Tone::Paper);
        draw_fn(&mut canvas);
        sink.write_strip_2bpp(y, rows, canvas.raw_2bpp_bytes());
        y += rows;
    }
    sink.end_frame(mode);
}

#[inline]
pub fn isqrt(n: i32) -> i32 {
    if n <= 0 {
        return 0;
    }
    let mut x = n;
    let mut y = (x + 1) >> 1;
    while y < x {
        x = y;
        y = (x + n / x) >> 1;
    }
    x
}
