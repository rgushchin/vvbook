#[derive(Clone, Copy, Debug)]
pub struct GlyphEntry {
    pub codepoint: u32,
    pub xmin: i8,
    pub ymin: i8,
    pub width: u8,
    pub height: u8,
    pub advance_q4: i16,
    pub offset: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct KernPair {
    pub key: u32,
    pub delta_q4: i8,
}

#[derive(Clone, Copy, Debug)]
pub struct BakedFont {
    pub name: &'static str,
    pub px: u8,
    pub ascent: i16,
    pub descent: i16,
    pub line_gap: i16,
    pub glyphs: &'static [GlyphEntry],
    pub kerns: &'static [KernPair],
    pub atlas: &'static [u8],
}

impl BakedFont {
    /// Fast glyph lookup: O(1) printable ASCII fast-path with binary search fallback.
    #[inline]
    pub fn glyph_index(&self, ch: char) -> Option<usize> {
        let cp = ch as u32;
        let idx = cp.wrapping_sub(0x20) as usize;
        if idx < self.glyphs.len() && self.glyphs[idx].codepoint == cp {
            return Some(idx);
        }
        self.glyphs.binary_search_by_key(&cp, |g| g.codepoint).ok()
    }

    #[inline]
    pub fn glyph(&self, ch: char) -> Option<(usize, &'static GlyphEntry)> {
        if let Some(idx) = self.glyph_index(ch) {
            return Some((idx, &self.glyphs[idx]));
        }
        // Smart typographic fallback to close ASCII if an exotic punctuation mark appears
        let fb = match ch {
            '\u{00A0}' | '\u{2009}' | '\u{202F}' => ' ',
            '‐' | '‑' => '-',
            '–' => '-',
            '’' | '‘' => '\'',
            '“' | '”' | '„' => '"',
            _ => '?',
        };
        self.glyph_index(fb).map(|idx| (idx, &self.glyphs[idx]))
    }

    #[inline]
    pub fn kern_q4(&self, left_idx: usize, right_idx: usize) -> i16 {
        if self.kerns.is_empty() {
            return 0;
        }
        let key = ((left_idx as u32) << 16) | (right_idx as u32);
        match self.kerns.binary_search_by_key(&key, |k| k.key) {
            Ok(i) => self.kerns[i].delta_q4 as i16,
            Err(_) => 0,
        }
    }

    pub fn line_height(&self) -> i32 {
        (self.ascent as i32 - self.descent as i32 + self.line_gap as i32).max(self.px as i32 + 2)
    }

    pub fn measure_text_q4(&self, text: &str, tracking_q4: i32) -> i32 {
        let mut x_q4: i32 = 0;
        let mut prev: Option<usize> = None;
        for ch in text.chars() {
            if let Some((idx, g)) = self.glyph(ch) {
                if let Some(p) = prev {
                    x_q4 += self.kern_q4(p, idx) as i32;
                    x_q4 += tracking_q4;
                }
                x_q4 += g.advance_q4 as i32;
                prev = Some(idx);
            }
        }
        x_q4
    }

    pub fn measure_text(&self, text: &str) -> i32 {
        (self.measure_text_q4(text, 0) + 8) >> 4
    }

    pub fn measure_text_tracked(&self, text: &str, tracking_q4: i32) -> i32 {
        (self.measure_text_q4(text, tracking_q4) + 8) >> 4
    }
}
