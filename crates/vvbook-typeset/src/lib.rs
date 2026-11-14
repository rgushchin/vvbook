#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod hyphen;

use alloc::borrow::Cow;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use vvbook_format::{Block, PageAnchor, SpanStyle, TextRun, VvbBook, VvbIndex};
use vvbook_gfx::baked_fonts;
use vvbook_gfx::canvas::{Align, Canvas};
use vvbook_gfx::font::BakedFont;
use vvbook_pal::{PORTRAIT_H, PORTRAIT_W, SAFE_INSETS, StorageBackend, Tone};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FontFamily {
    Serif = 0,  // Palatino (P052)
    School = 1, // Century Schoolbook (C059)
    Sans = 2,   // OpenSans
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FontSizePreset {
    Compact = 0, // 23 px
    Classic = 1, // 27 px
    Large = 2,   // 32 px
    Extra = 3,   // 38 px
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarginPreset {
    Narrow = 0,    // 20 px
    Editorial = 1, // 28 px
    Wide = 2,      // 40 px
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReaderTypography {
    pub font_family: FontFamily,
    pub font_size: FontSizePreset,
    pub margin_preset: MarginPreset,
    /// Line height multiplier in 1/16ths (e.g., 22 => 1.375x).
    pub line_spacing_q4: u8,
    pub justify: bool,
    pub hyphenate: bool,
    pub drop_cap: bool,
}

impl Default for ReaderTypography {
    fn default() -> Self {
        Self {
            font_family: FontFamily::School,
            font_size: FontSizePreset::Classic,
            margin_preset: MarginPreset::Editorial,
            line_spacing_q4: 22,
            justify: true,
            hyphenate: true,
            drop_cap: true,
        }
    }
}

pub struct FontSet {
    pub regular: &'static BakedFont,
    pub italic: &'static BakedFont,
    pub bold: &'static BakedFont,
}

impl ReaderTypography {
    pub fn fonts(&self) -> FontSet {
        match (self.font_family, self.font_size) {
            (FontFamily::School, FontSizePreset::Compact) => FontSet {
                regular: &baked_fonts::FONT_SCHOOL_23,
                italic: &baked_fonts::FONT_SCHOOL_23_ITALIC,
                bold: &baked_fonts::FONT_SCHOOL_23_BOLD,
            },
            (FontFamily::School, FontSizePreset::Classic) => FontSet {
                regular: &baked_fonts::FONT_SCHOOL_27,
                italic: &baked_fonts::FONT_SCHOOL_27_ITALIC,
                bold: &baked_fonts::FONT_SCHOOL_27_BOLD,
            },
            (FontFamily::School, FontSizePreset::Large) => FontSet {
                regular: &baked_fonts::FONT_SCHOOL_32,
                italic: &baked_fonts::FONT_SCHOOL_32_ITALIC,
                bold: &baked_fonts::FONT_SCHOOL_32_BOLD,
            },
            (FontFamily::School, FontSizePreset::Extra) => FontSet {
                regular: &baked_fonts::FONT_SCHOOL_38,
                italic: &baked_fonts::FONT_SCHOOL_38_ITALIC,
                bold: &baked_fonts::FONT_SCHOOL_38_BOLD,
            },
            (FontFamily::Serif, FontSizePreset::Compact) => FontSet {
                regular: &baked_fonts::FONT_SERIF_23,
                italic: &baked_fonts::FONT_SERIF_23_ITALIC,
                bold: &baked_fonts::FONT_SERIF_23_BOLD,
            },
            (FontFamily::Serif, FontSizePreset::Classic) => FontSet {
                regular: &baked_fonts::FONT_SERIF_27,
                italic: &baked_fonts::FONT_SERIF_27_ITALIC,
                bold: &baked_fonts::FONT_SERIF_27_BOLD,
            },
            (FontFamily::Serif, FontSizePreset::Large) => FontSet {
                regular: &baked_fonts::FONT_SERIF_32,
                italic: &baked_fonts::FONT_SERIF_32_ITALIC,
                bold: &baked_fonts::FONT_SERIF_32_BOLD,
            },
            (FontFamily::Serif, FontSizePreset::Extra) => FontSet {
                regular: &baked_fonts::FONT_SERIF_38,
                italic: &baked_fonts::FONT_SERIF_38_ITALIC,
                bold: &baked_fonts::FONT_SERIF_38_BOLD,
            },
            (FontFamily::Sans, FontSizePreset::Compact) => FontSet {
                regular: &baked_fonts::FONT_UI_22_SEMI,
                italic: &baked_fonts::FONT_UI_22_SEMI,
                bold: &baked_fonts::FONT_UI_25_BOLD,
            },
            (FontFamily::Sans, FontSizePreset::Classic) => FontSet {
                regular: &baked_fonts::FONT_UI_25_BOLD,
                italic: &baked_fonts::FONT_UI_22_SEMI,
                bold: &baked_fonts::FONT_UI_32_BOLD,
            },
            (FontFamily::Sans, FontSizePreset::Large | FontSizePreset::Extra) => FontSet {
                regular: &baked_fonts::FONT_UI_32_BOLD,
                italic: &baked_fonts::FONT_UI_25_BOLD,
                bold: &baked_fonts::FONT_UI_32_BOLD,
            },
        }
    }

    pub fn font_for_style(&self, style: SpanStyle) -> &'static BakedFont {
        let fs = self.fonts();
        match style {
            SpanStyle::Regular => fs.regular,
            SpanStyle::Italic => fs.italic,
            SpanStyle::Bold | SpanStyle::BoldItalic => fs.bold,
            SpanStyle::Code => &baked_fonts::FONT_MONO_20_SEMI,
        }
    }

    pub fn margins(&self) -> (i32, i32, i32, i32) {
        let side = match self.margin_preset {
            MarginPreset::Narrow => 20,
            MarginPreset::Editorial => 28,
            MarginPreset::Wide => 40,
        };
        let top = (SAFE_INSETS.top as i32 + 51).max(60);
        let bottom = (SAFE_INSETS.bottom as i32 + 59).max(62);
        let left = side.max(SAFE_INSETS.left as i32 + 12);
        let right = side.max(SAFE_INSETS.right as i32 + 12);
        (top, right, bottom, left)
    }

    pub fn line_advance(&self) -> i32 {
        let reg = self.fonts().regular;
        ((reg.px as i32 * self.line_spacing_q4 as i32) + 8) >> 4
    }

    pub fn layout_hash(&self) -> u32 {
        let mut h: u32 = 0x811C_9DC5;
        let bytes = [
            self.font_family as u8,
            self.font_size as u8,
            self.margin_preset as u8,
            self.line_spacing_q4,
            self.justify as u8,
            self.hyphenate as u8,
            self.drop_cap as u8,
        ];
        for b in bytes {
            h ^= b as u32;
            h = h.wrapping_mul(0x0100_0193);
        }
        h
    }
}

/// One atom along a paragraph: either a standalone word, a glued cluster (`в лесу`),
/// or a prefix ending in a soft hyphen candidate (`мо-`).
#[derive(Clone, Debug)]
struct Atom<'a> {
    text: &'a str,
    width: i16,
    space_after: i16,
    word_idx: u16,
    hyphen_width: i16,
    left_hang: i8,
    right_hang: i8,
    style: SpanStyle,
    is_hyphen_break: bool,
    /// When true, cannot break after this atom unless forced (1-letter preposition glue).
    glue_next: bool,
}

#[derive(Clone, Debug)]
pub struct SpanDraw {
    pub x: i32,
    pub style: SpanStyle,
    pub text: String,
}

#[derive(Clone, Debug)]
pub enum PageItem {
    ChapterBanner {
        y_baseline: i32,
        title: String,
    },
    HeadingLine {
        y_baseline: i32,
        level: u8,
        text: String,
    },
    DropCap {
        x: i32,
        y_baseline: i32,
        glyph: char,
    },
    TextLine {
        y_baseline: i32,
        spans: Vec<SpanDraw>,
        is_quote: bool,
        quote_x: i32,
    },
    CodeLine {
        y_top: i32,
        y_baseline: i32,
        x: i32,
        w: i32,
        text: String,
    },
    SceneBreak {
        y_baseline: i32,
    },
}

#[derive(Clone, Debug)]
pub struct PageLayout {
    pub anchor: PageAnchor,
    pub next_anchor: Option<PageAnchor>,
    pub items: Vec<PageItem>,
    pub chapter_hint: String,
}

pub struct PageChrome<'a> {
    pub book_title: &'a str,
    pub chapter_title: &'a str,
    pub page_num: usize,
    pub total_pages: usize,
    pub bookmarked: bool,
    pub chin_hints: [&'a str; 4],
}

#[inline]
fn hang_left(text: &str) -> i8 {
    match text.chars().next() {
        Some('«' | '“' | '„' | '—') => 3,
        _ => 0,
    }
}

#[inline]
fn hang_right(text: &str) -> i8 {
    match text.chars().last() {
        Some(',' | '.' | '-' | '‐' | ';' | ':' | '»' | '”' | '’' | '!' | '?') => 4,
        _ => 0,
    }
}

#[inline]
fn clamp_i16(v: i32) -> i16 {
    v.clamp(0, i16::MAX as i32) as i16
}

/// Flatten `runs` into borrowed atomic word tokens starting from `start_word_offset`.
/// Windowed to at most `MAX_PAGE_WORDS` logical words so huge paragraphs never OOM DP tables;
/// returns `(atoms, total_words_consumed, next_word_offset_if_windowed)`.
const MAX_PAGE_WORDS: u16 = 260;

fn tokenize_runs<'a>(
    runs: &'a [TextRun],
    typo: &ReaderTypography,
    start_word_offset: u16,
) -> (Vec<Atom<'a>>, u16, Option<u16>) {
    let mut atoms: Vec<Atom<'a>> = Vec::new();
    let mut word_counter: u16 = 0;
    let space_w = clamp_i16((typo.fonts().regular.measure_text(" ")).max(4));
    let hyphen_w_reg = clamp_i16(typo.fonts().regular.measure_text("-"));
    let mut explicit_buf = [0usize; 16];
    let mut hyp_buf = [0usize; 16];

    for run in runs {
        let style = run.style;
        let font = typo.font_for_style(style);
        for word in run.text.split_whitespace() {
            let word_idx = word_counter;
            if word_idx >= start_word_offset && (word_idx - start_word_offset) >= MAX_PAGE_WORDS {
                if let Some(last) = atoms.last_mut() {
                    last.space_after = 0;
                    last.glue_next = false;
                }
                return (atoms, word_counter, Some(word_idx));
            }
            word_counter = word_counter.saturating_add(1);
            if word_idx < start_word_offset {
                continue;
            }
            let glue = hyphen::must_glue_to_next(word);

            if typo.hyphenate && !glue {
                let mut exp_len = 0usize;
                for (i, ch) in word.char_indices() {
                    if (ch == '-' || ch == '‐') && i >= 2 && i + 1 < word.len() && exp_len < explicit_buf.len() {
                        explicit_buf[exp_len] = i + ch.len_utf8();
                        exp_len += 1;
                    }
                }
                if exp_len > 0 {
                    let mut prev = 0usize;
                    for &split in &explicit_buf[..exp_len] {
                        let seg = &word[prev..split];
                        atoms.push(Atom {
                            style,
                            text: seg,
                            width: clamp_i16(font.measure_text(seg)),
                            space_after: 0,
                            word_idx,
                            is_hyphen_break: false,
                            hyphen_width: 0,
                            left_hang: if prev == 0 { hang_left(seg) } else { 0 },
                            right_hang: 4,
                            glue_next: false,
                        });
                        prev = split;
                    }
                    let tail = &word[prev..];
                    atoms.push(Atom {
                        style,
                        text: tail,
                        width: clamp_i16(font.measure_text(tail)),
                        space_after: space_w,
                        word_idx,
                        is_hyphen_break: false,
                        hyphen_width: 0,
                        left_hang: 0,
                        right_hang: hang_right(tail),
                        glue_next: false,
                    });
                    continue;
                }

                let hyp_len = hyphen::hyphen_points_into(word, &mut hyp_buf);
                if hyp_len > 0 {
                    let mut prev = 0usize;
                    for &split in &hyp_buf[..hyp_len] {
                        let seg = &word[prev..split];
                        atoms.push(Atom {
                            style,
                            text: seg,
                            width: clamp_i16(font.measure_text(seg)),
                            space_after: 0,
                            word_idx,
                            is_hyphen_break: true,
                            hyphen_width: hyphen_w_reg,
                            left_hang: if prev == 0 { hang_left(seg) } else { 0 },
                            right_hang: 4,
                            glue_next: false,
                        });
                        prev = split;
                    }
                    let tail = &word[prev..];
                    atoms.push(Atom {
                        style,
                        text: tail,
                        width: clamp_i16(font.measure_text(tail)),
                        space_after: space_w,
                        word_idx,
                        is_hyphen_break: false,
                        hyphen_width: 0,
                        left_hang: 0,
                        right_hang: hang_right(tail),
                        glue_next: false,
                    });
                    continue;
                }
            }

            atoms.push(Atom {
                style,
                text: word,
                width: clamp_i16(font.measure_text(word)),
                space_after: space_w,
                word_idx,
                is_hyphen_break: false,
                hyphen_width: 0,
                left_hang: hang_left(word),
                right_hang: hang_right(word),
                glue_next: glue,
            });
        }
    }

    // Remove trailing space after the final atom
    if let Some(last) = atoms.last_mut() {
        last.space_after = 0;
        last.glue_next = false;
    }

    (atoms, word_counter, None)
}

#[derive(Clone, Debug)]
struct BrokenLine {
    first_atom: usize,
    last_atom_excl: usize,
    ends_with_hyphen: bool,
    word_idx_start: u16,
    x_start: i32,
    line_width: i32,
    is_last_line_of_para: bool,
}

/// Knuth–Plass dynamic programming line-breaker supporting variable first-line geometry
/// (paragraph indent OR sunken 2-line drop-cap cutout!).
fn break_atoms_knuth_plass(
    atoms: &[Atom<'_>],
    total_word_count: u16,
    first_line_indent: i32,
    drop_cap_cutout: Option<(i32, usize)>, // (cutout_px, cutout_lines)
    base_x: i32,
    base_w: i32,
    justify: bool,
) -> Vec<BrokenLine> {
    let n = atoms.len();
    if n == 0 {
        return Vec::new();
    }

    // Prefix sum of atom widths + space_after for O(1) slice width query (approximate un-glued segments)
    // Remember: intra-word segments have `space_after == 0`, so joining atoms[i..j] gives the exact unsplit word width!
    let line_geom = |line_idx: usize| -> (i32, i32) {
        let mut x = base_x;
        let mut w = base_w;
        if let Some((cut_w, cut_lines)) = drop_cap_cutout
            && line_idx < cut_lines {
                x += cut_w;
                w -= cut_w;
                return (x, w.max(80));
            }
        if line_idx == 0 && first_line_indent > 0 {
            x += first_line_indent;
            w -= first_line_indent;
        }
        (x, w.max(80))
    };

    // Since drop-cap or indent only alters the first 1..3 lines, run greedy-informed DP per line budget.
    // For typical paragraphs (<= 300 atoms), a clean DP over `best_cost[i]` finds the global optimum.
    let inf: i64 = 1_000_000_000_000;
    let mut best_cost = vec![inf; n + 1];
    let mut next_break = vec![n; n];
    best_cost[n] = 0;

    // Estimate line index forward roughly using greedy pass so line 0/1 indents align accurately!
    let mut approx_line_of = vec![0usize; n + 1];
    {
        let mut idx = 0usize;
        let mut ln = 0usize;
        while idx < n {
            approx_line_of[idx] = ln;
            let (_, wmax) = line_geom(ln);
            let mut acc = 0i32;
            let mut j = idx;
            while j < n {
                let extra_hyph = if atoms[j].is_hyphen_break {
                    i32::from(atoms[j].hyphen_width)
                } else {
                    0
                };
                if j > idx && acc + i32::from(atoms[j].width) + extra_hyph > wmax {
                    break;
                }
                acc += i32::from(atoms[j].width) + i32::from(atoms[j].space_after);
                j += 1;
                approx_line_of[j] = ln;
            }
            if j == idx {
                j += 1;
            }
            idx = j;
            ln += 1;
        }
    }

    for i in (0..n).rev() {
        let (_, max_w) = line_geom(approx_line_of[i]);
        let left_h = i32::from(atoms[i].left_hang);
        let mut width_no_hang = 0i32;
        let mut gaps = 0i32;

        for j in (i + 1)..=n {
            let a = &atoms[j - 1];
            width_no_hang += i32::from(a.width);
            if j - 1 > i && atoms[j - 2].space_after > 0 {
                gaps += 1;
            }
            if j - 1 > i {
                width_no_hang += i32::from(atoms[j - 2].space_after);
            }

            // Do not break between hyphenated pieces unless j is a valid break point!
            let can_break = j == n || (!a.glue_next && (a.space_after > 0 || a.is_hyphen_break));
            let hyph_w = if j < n && a.is_hyphen_break {
                i32::from(a.hyphen_width)
            } else {
                0
            };
            let eff_w = width_no_hang + hyph_w - left_h - i32::from(a.right_hang);

            if eff_w > max_w + 18 && j > i + 1 {
                break;
            }
            if !can_break {
                continue;
            }

            let is_last = j == n;
            let slack = max_w - eff_w;
            let mut demerits: i64 = if slack < 0 {
                // Slight negative slack allowed for optical margin / emergency squeeze
                let over = (-slack) as i64;
                over * over * 200
            } else if is_last {
                // Ragged last line is natural and incurs near-zero penalty unless single orphan word
                let len_ratio = (eff_w * 100) / max_w.max(1);
                if len_ratio < 16 && gaps == 0 {
                    900
                } else {
                    10
                }
            } else if justify {
                let adj_gaps = gaps.max(1) as i64;
                let stretch_per_gap = (slack as i64 * 10) / adj_gaps;
                stretch_per_gap * stretch_per_gap
            } else {
                // Ragged right prefers smooth line fill without huge gaps
                let s = (slack / 3) as i64;
                s * s
            };

            if j < n && a.is_hyphen_break {
                demerits += 1_800; // Mildly prefer avoiding hyphenation when good breaks exist
            }
            // Penalize double consecutive hyphens
            if j < n && a.is_hyphen_break && next_break[j] < n && atoms[next_break[j] - 1].is_hyphen_break {
                demerits += 5_000;
            }

            let total = demerits + best_cost[j];
            if total < best_cost[i] {
                best_cost[i] = total;
                next_break[i] = j;
            }
        }
    }

    let mut lines = Vec::new();
    let mut cur = 0usize;
    let mut line_idx = 0usize;
    while cur < n {
        let mut end = next_break[cur];
        if end <= cur {
            end = cur + 1;
        }
        // Snap end forward if it landed mid-word without a hyphen break
        if end < n && atoms[end - 1].space_after == 0 && !atoms[end - 1].is_hyphen_break {
            while end < n && atoms[end - 1].space_after == 0 && !atoms[end - 1].is_hyphen_break {
                end += 1;
            }
        }
        let (lx, lw) = line_geom(line_idx);
        let w_start = atoms[cur].word_idx;
        let _ = total_word_count;
        lines.push(BrokenLine {
            first_atom: cur,
            last_atom_excl: end,
            ends_with_hyphen: end < n && atoms[end - 1].is_hyphen_break,
            word_idx_start: w_start,
            x_start: lx,
            line_width: lw,
            is_last_line_of_para: end == n,
        });
        cur = end;
        line_idx += 1;
    }
    lines
}

fn render_broken_line_spans(atoms: &[Atom<'_>], line: &BrokenLine, justify: bool) -> Vec<SpanDraw> {
    let slice = &atoms[line.first_atom..line.last_atom_excl];
    if slice.is_empty() {
        return Vec::new();
    }
    let left_h = i32::from(slice[0].left_hang);
    let right_h = i32::from(slice.last().unwrap().right_hang);

    // Count inter-word gaps and measure unstretched total width
    let mut raw_w = 0i32;
    let mut gap_count = 0i32;
    for (idx, a) in slice.iter().enumerate() {
        raw_w += i32::from(a.width);
        if idx + 1 < slice.len() && a.space_after > 0 {
            raw_w += i32::from(a.space_after);
            gap_count += 1;
        }
    }
    if line.ends_with_hyphen {
        raw_w += i32::from(slice.last().unwrap().hyphen_width);
    }

    let target_w = line.line_width + left_h + right_h;
    let extra = if justify && !line.is_last_line_of_para && gap_count > 0 {
        (target_w - raw_w).clamp(-gap_count * 2, gap_count * 16)
    } else {
        0
    };
    let per_gap_extra = if gap_count > 0 { extra / gap_count } else { 0 };
    let rem_extra = if gap_count > 0 { extra % gap_count } else { 0 };

    // Coalesce consecutive same-style segments that have zero extra stretch into single `SpanDraw` strings
    let mut out: Vec<SpanDraw> = Vec::new();
    let mut cur_x = line.x_start - left_h;
    let mut gap_idx = 0i32;
    let mut pending_text = String::new();
    let mut pending_style = slice[0].style;
    let mut pending_x = cur_x;

    for (idx, a) in slice.iter().enumerate() {
        if a.style != pending_style && !pending_text.is_empty() {
            out.push(SpanDraw {
                x: pending_x,
                style: pending_style,
                text: core::mem::take(&mut pending_text),
            });
            pending_x = cur_x;
            pending_style = a.style;
        }
        if pending_text.is_empty() {
            pending_x = cur_x;
            pending_style = a.style;
        }
        pending_text.push_str(a.text);
        cur_x += i32::from(a.width);

        if idx + 1 == slice.len() {
            if line.ends_with_hyphen {
                pending_text.push('-');
            }
        } else if a.space_after > 0 {
            let bonus = if gap_idx < rem_extra.abs() {
                rem_extra.signum()
            } else {
                0
            };
            let space_after = i32::from(a.space_after);
            let delta = space_after + per_gap_extra + bonus;
            gap_idx += 1;
            // If exact normal space width matches unstretched space, merge cleanly inside pending_text
            if per_gap_extra == 0 && bonus == 0 && slice[idx + 1].style == pending_style {
                pending_text.push(' ');
                cur_x += space_after;
            } else {
                out.push(SpanDraw {
                    x: pending_x,
                    style: pending_style,
                    text: core::mem::take(&mut pending_text),
                });
                cur_x += delta;
                pending_x = cur_x;
                pending_style = slice[idx + 1].style;
            }
        }
    }
    if !pending_text.is_empty() {
        out.push(SpanDraw {
            x: pending_x,
            style: pending_style,
            text: pending_text,
        });
    }
    out
}

/// Lay out a single block starting at `start_word_offset` within `y_cursor..=y_bottom`.
/// When `emit_items == false`, skips allocating `PageItem` / `SpanDraw` while counting exact emissions.
/// Returns `(items_added, emitted_count, new_y_cursor, leftover_word_offset_if_split)`.
#[allow(clippy::too_many_arguments)]
fn layout_block_into_page(
    block: &Block,
    start_word_offset: u16,
    typo: &ReaderTypography,
    left: i32,
    right: i32,
    y_top: i32,
    y_bottom: i32,
    y_cursor: i32,
    at_page_top: bool,
    chapter_hint: &mut String,
    emit_items: bool,
) -> (Vec<PageItem>, usize, i32, Option<u16>) {
    let col_w = PORTRAIT_W as i32 - left - right;
    let line_adv = typo.line_advance();
    let reg_font = typo.fonts().regular;
    let mut items = Vec::new();
    let mut emitted = 0usize;
    let mut y = y_cursor;

    match block {
        Block::ChapterTitle { title } => {
            if emit_items {
                *chapter_hint = title.clone();
            }
            if start_word_offset > 0 {
                return (items, emitted, y, None);
            }
            // Start chapters cleanly with noble breathing room unless already mid-page (handled by caller break)
            let banner_y = if at_page_top { y_top + 68 } else { y + 48 };
            if banner_y + 24 > y_bottom {
                return (items, emitted, y, Some(0));
            }
            emitted += 1;
            if emit_items {
                items.push(PageItem::ChapterBanner {
                    y_baseline: banner_y,
                    title: title.clone(),
                });
            }
            y = banner_y + 42;
            (items, emitted, y, None)
        }
        Block::Heading { level, text } => {
            if at_page_top && y == y_top {
                y += 6;
            } else {
                y += 12;
            }
            let font = if *level <= 2 {
                &baked_fonts::FONT_SCHOOL_27_BOLD
            } else {
                typo.fonts().bold
            };
            let words_as_run = [TextRun {
                style: SpanStyle::Bold,
                text: text.clone(),
            }];
            let (atoms, total_words, cont_word) = tokenize_runs(&words_as_run, typo, start_word_offset);
            let lines = break_atoms_knuth_plass(&atoms, total_words, 0, None, left, col_w, false);
            let h_adv = (font.line_height() + 4).max(line_adv);
            for ln in lines {
                let baseline = y + font.ascent as i32;
                if baseline + 6 > y_bottom {
                    return (items, emitted, y, Some(ln.word_idx_start));
                }
                emitted += 1;
                if emit_items {
                    let line_txt: String = atoms[ln.first_atom..ln.last_atom_excl]
                        .iter()
                        .map(|a| a.text)
                        .collect::<Vec<_>>()
                        .join(" ");
                    items.push(PageItem::HeadingLine {
                        y_baseline: baseline,
                        level: *level,
                        text: line_txt,
                    });
                }
                y += h_adv;
            }
            if cont_word.is_none() {
                y += 8;
            }
            (items, emitted, y, cont_word)
        }
        Block::Paragraph {
            runs,
            first_of_chapter,
            is_dialogue,
        } => {
            let use_drop_cap = typo.drop_cap && *first_of_chapter && !*is_dialogue && start_word_offset == 0;
            let mut drop_glyph: Option<char> = None;
            let mut drop_cutout: Option<(i32, usize)> = None;
            let mut adj_runs_owned: Option<Vec<TextRun>> = None;

            if use_drop_cap
                && let Some((idx, first_run)) = runs.iter().enumerate().find(|(_, r)| !r.text.trim_start().is_empty())
            {
                let trimmed = first_run.text.trim_start();
                let mut ch_it = trimmed.chars();
                if let Some(ch) = ch_it.next()
                    && ch.is_alphabetic()
                {
                    drop_glyph = Some(ch);
                    let mut ch_buf = [0u8; 4];
                    let ch_str = ch.encode_utf8(&mut ch_buf);
                    let dc_w = baked_fonts::FONT_SERIF_64_DROPCAP
                        .measure_text(ch_str)
                        .max(34);
                    drop_cutout = Some((dc_w + 12, 2));
                    let mut cloned = runs.to_vec();
                    cloned[idx].text = ch_it.as_str().trim_start().to_string();
                    adj_runs_owned = Some(cloned);
                }
            }

            let indent = if *first_of_chapter || *is_dialogue || use_drop_cap || start_word_offset > 0 {
                0
            } else {
                reg_font.px as i32
            };

            let runs_slice = adj_runs_owned.as_deref().unwrap_or(runs);
            let (atoms, total_words, cont_word) = tokenize_runs(runs_slice, typo, start_word_offset);
            if atoms.is_empty() && drop_glyph.is_none() {
                return (items, emitted, y, cont_word);
            }

            let lines = break_atoms_knuth_plass(
                &atoms,
                total_words,
                indent,
                drop_cutout,
                left,
                col_w,
                typo.justify,
            );

            // Check at least DropCap fits if present
            if let Some(dg) = drop_glyph {
                let needed_h = line_adv * 2;
                if y + needed_h > y_bottom {
                    return (items, emitted, y, Some(0));
                }
                emitted += 1;
                if emit_items {
                    let dc_baseline = y + reg_font.ascent as i32 + line_adv - 2;
                    items.push(PageItem::DropCap {
                        x: left,
                        y_baseline: dc_baseline,
                        glyph: dg,
                    });
                }
            }

            for ln in lines {
                let baseline = y + reg_font.ascent as i32;
                if baseline + (-reg_font.descent as i32).max(4) > y_bottom {
                    // Prevent breaking immediately after drop cap orphan
                    return (items, emitted, y, Some(ln.word_idx_start));
                }
                emitted += 1;
                if emit_items {
                    let spans = render_broken_line_spans(&atoms, &ln, typo.justify);
                    items.push(PageItem::TextLine {
                        y_baseline: baseline,
                        spans,
                        is_quote: false,
                        quote_x: left,
                    });
                }
                y += line_adv;
            }
            if cont_word.is_none() {
                y += (line_adv / 4).max(4);
            }
            (items, emitted, y, cont_word)
        }
        Block::VerseLine { text } => {
            if start_word_offset > 0 {
                return (items, emitted, y, None);
            }
            let verse_x = left + 28;
            let verse_w = (col_w - 36).max(80);
            let runs = [TextRun {
                style: SpanStyle::Italic,
                text: text.clone(),
            }];
            let (atoms, total_words, _cont) = tokenize_runs(&runs, typo, 0);
            let lines = break_atoms_knuth_plass(&atoms, total_words, 0, None, verse_x, verse_w, false);
            for (li, mut ln) in lines.into_iter().enumerate() {
                if li > 0 {
                    ln.x_start += 18; // Hanging continuation indent for verse wrap
                    ln.line_width -= 18;
                }
                let baseline = y + reg_font.ascent as i32;
                if baseline + 5 > y_bottom {
                    return (items, emitted, y, Some(0));
                }
                emitted += 1;
                if emit_items {
                    let spans = render_broken_line_spans(&atoms, &ln, false);
                    items.push(PageItem::TextLine {
                        y_baseline: baseline,
                        spans,
                        is_quote: false,
                        quote_x: left,
                    });
                }
                y += line_adv;
            }
            (items, emitted, y, None)
        }
        Block::BlockQuote { runs } => {
            let qx = left + 22;
            let qw = (col_w - 28).max(80);
            let (atoms, total_words, cont_word) = tokenize_runs(runs, typo, start_word_offset);
            let lines = break_atoms_knuth_plass(&atoms, total_words, 0, None, qx, qw, typo.justify);
            for ln in lines {
                let baseline = y + reg_font.ascent as i32;
                if baseline + 5 > y_bottom {
                    return (items, emitted, y, Some(ln.word_idx_start));
                }
                emitted += 1;
                if emit_items {
                    let spans = render_broken_line_spans(&atoms, &ln, typo.justify);
                    items.push(PageItem::TextLine {
                        y_baseline: baseline,
                        spans,
                        is_quote: true,
                        quote_x: left + 8,
                    });
                }
                y += line_adv;
            }
            if cont_word.is_none() {
                y += 8;
            }
            (items, emitted, y, cont_word)
        }
        Block::CodeBlock { text } => {
            let raw_lines: Vec<&str> = text.lines().collect();
            let start_ln = start_word_offset as usize;
            let mono = &baked_fonts::FONT_MONO_20_SEMI;
            let m_adv = (mono.line_height() + 4).max(26);
            for (idx, line) in raw_lines.iter().enumerate().skip(start_ln) {
                if y + m_adv > y_bottom {
                    return (items, emitted, y, Some(idx as u16));
                }
                emitted += 1;
                if emit_items {
                    items.push(PageItem::CodeLine {
                        y_top: y,
                        y_baseline: y + mono.ascent as i32 + 2,
                        x: left + 10,
                        w: col_w - 10,
                        text: (*line).to_string(),
                    });
                }
                y += m_adv;
            }
            y += 8;
            (items, emitted, y, None)
        }
        Block::ListItem { bullet, runs } => {
            let lx = left + 22;
            let lw = (col_w - 22).max(80);
            let mut full_runs = runs.clone();
            if start_word_offset == 0 {
                if let Some(first) = full_runs.first_mut() {
                    first.text = format!("{} {}", bullet, first.text);
                } else {
                    full_runs.push(TextRun {
                        style: SpanStyle::Regular,
                        text: bullet.clone(),
                    });
                }
            }
            let (atoms, total_words, cont_word) = tokenize_runs(&full_runs, typo, start_word_offset);
            let lines = break_atoms_knuth_plass(&atoms, total_words, 0, None, lx, lw, false);
            for ln in lines {
                let baseline = y + reg_font.ascent as i32;
                if baseline + 5 > y_bottom {
                    return (items, emitted, y, Some(ln.word_idx_start));
                }
                emitted += 1;
                if emit_items {
                    let spans = render_broken_line_spans(&atoms, &ln, false);
                    items.push(PageItem::TextLine {
                        y_baseline: baseline,
                        spans,
                        is_quote: false,
                        quote_x: left,
                    });
                }
                y += line_adv;
            }
            if cont_word.is_none() {
                y += 4;
            }
            (items, emitted, y, cont_word)
        }
        Block::SceneBreak => {
            if start_word_offset > 0 {
                return (items, emitted, y, None);
            }
            if y + line_adv * 2 > y_bottom {
                return (items, emitted, y, Some(0));
            }
            emitted += 1;
            if emit_items {
                items.push(PageItem::SceneBreak {
                    y_baseline: y + reg_font.ascent as i32 + 8,
                });
            }
            y += line_adv + 14;
            (items, emitted, y, None)
        }
    }
}

fn layout_page_core<'a, F>(
    total_blocks: usize,
    anchor: PageAnchor,
    typo: &ReaderTypography,
    emit_items: bool,
    mut fetch: F,
) -> PageLayout
where
    F: FnMut(usize) -> Option<Cow<'a, Block>>,
{
    let (y_top, right, y_bottom_pad, left) = typo.margins();
    let y_bottom = PORTRAIT_H as i32 - y_bottom_pad;
    let mut y = y_top;
    let mut items = Vec::new();
    let mut total_emitted = 0usize;
    let mut chapter_hint = String::new();
    let mut blk_idx = anchor.block_idx as usize;
    let mut word_off = anchor.word_offset;
    let mut next_anchor = None;

    while blk_idx < total_blocks {
        let Some(block) = fetch(blk_idx) else {
            break;
        };
        let at_top = total_emitted == 0;
        // Force chapter title onto a fresh page if items already exist on this page!
        if matches!(*block, Block::ChapterTitle { .. }) && !at_top {
            next_anchor = Some(PageAnchor {
                block_idx: blk_idx as u32,
                word_offset: 0,
            });
            break;
        }
        let (added, block_emitted, new_y, leftover) = layout_block_into_page(
            &block,
            word_off,
            typo,
            left,
            right,
            y_top,
            y_bottom,
            y,
            at_top,
            &mut chapter_hint,
            emit_items,
        );

        let added_any = block_emitted > 0;
        total_emitted += block_emitted;
        if emit_items {
            items.extend(added);
        }
        y = new_y;

        if let Some(next_word) = leftover {
            // Avoid infinite stall if a single monster line can't fit even on an empty page
            if !added_any && at_top {
                blk_idx += 1;
                word_off = 0;
                continue;
            }
            next_anchor = Some(PageAnchor {
                block_idx: blk_idx as u32,
                word_offset: next_word,
            });
            break;
        }

        blk_idx += 1;
        word_off = 0;
    }

    PageLayout {
        anchor,
        next_anchor,
        items,
        chapter_hint,
    }
}

/// Lay out a single page starting at `anchor` using an arbitrary block provider `fetch(block_idx)`.
pub fn layout_page_with<F>(
    total_blocks: usize,
    anchor: PageAnchor,
    typo: &ReaderTypography,
    mut fetch: F,
) -> PageLayout
where
    F: FnMut(usize) -> Option<Block>,
{
    layout_page_core(total_blocks, anchor, typo, true, |i| {
        fetch(i).map(Cow::Owned)
    })
}

/// Lay out a single page starting at `anchor` by borrowing an in-memory block slice.
pub fn layout_page_slice(
    blocks: &[Block],
    anchor: PageAnchor,
    typo: &ReaderTypography,
) -> PageLayout {
    layout_page_core(blocks.len(), anchor, typo, true, |i| {
        blocks.get(i).map(Cow::Borrowed)
    })
}

pub fn paginate_in_memory(book: &VvbBook, typo: &ReaderTypography) -> Vec<PageAnchor> {
    let mut pages = Vec::new();
    let mut cur = PageAnchor {
        block_idx: 0,
        word_offset: 0,
    };
    let total = book.blocks.len();
    if total == 0 {
        pages.push(cur);
        return pages;
    }
    loop {
        pages.push(cur);
        let page = layout_page_slice(&book.blocks, cur, typo);
        match page.next_anchor {
            Some(na) if na != cur && (na.block_idx as usize) < total => cur = na,
            _ => break,
        }
        if pages.len() > 20_000 {
            break;
        }
    }
    pages
}

pub fn paginate_from_anchor<S: StorageBackend>(
    index: &VvbIndex,
    storage: &mut S,
    vvb_path: &str,
    start: PageAnchor,
    max_pages: usize,
    typo: &ReaderTypography,
) -> (Vec<PageAnchor>, Option<PageAnchor>) {
    let total = index.block_count();
    let mut pages = Vec::new();
    if total == 0 {
        pages.push(PageAnchor {
            block_idx: 0,
            word_offset: 0,
        });
        return (pages, None);
    }
    if max_pages == 0 || (start.block_idx as usize) >= total {
        return (pages, None);
    }

    const BATCH_BLOCKS: usize = 24;
    let mut batch_start = 0usize;
    let mut batch: Vec<Block> = Vec::new();
    let mut cur = start;

    loop {
        pages.push(cur);
        let page = layout_page_core(total, cur, typo, false, |i| {
            if i >= total {
                return None;
            }
            if batch.is_empty() || i < batch_start || i >= batch_start + batch.len() {
                batch = index.read_blocks_range(storage, vvb_path, i, BATCH_BLOCKS).ok()?;
                batch_start = i;
            }
            batch.get(i - batch_start).cloned().map(Cow::Owned)
        });
        match page.next_anchor {
            Some(na) if na != cur && (na.block_idx as usize) < total => {
                if pages.len() >= max_pages {
                    return (pages, Some(na));
                }
                cur = na;
            }
            _ => return (pages, None),
        }
    }
}

/// Paginate an in-memory block slice (e.g., a `DirectBook` section) without cloning blocks.
pub fn paginate_blocks_slice(
    blocks: &[Block],
    typo: &ReaderTypography,
    max_pages: usize,
) -> Vec<PageAnchor> {
    let mut pages = Vec::new();
    let total = blocks.len();
    let mut cur = PageAnchor {
        block_idx: 0,
        word_offset: 0,
    };
    if total == 0 || max_pages == 0 {
        pages.push(cur);
        return pages;
    }
    loop {
        pages.push(cur);
        let page = layout_page_core(total, cur, typo, false, |i| {
            blocks.get(i).map(Cow::Borrowed)
        });
        match page.next_anchor {
            Some(na) if na != cur && (na.block_idx as usize) < total => {
                if pages.len() >= max_pages {
                    break;
                }
                cur = na;
            }
            _ => break,
        }
    }
    pages
}

pub fn build_layout_index<S: StorageBackend>(
    index: &VvbIndex,
    storage: &mut S,
    vvb_path: &str,
    typo: &ReaderTypography,
) -> Vec<PageAnchor> {
    paginate_from_anchor(
        index,
        storage,
        vvb_path,
        PageAnchor {
            block_idx: 0,
            word_offset: 0,
        },
        16_384,
        typo,
    )
    .0
}

pub fn layout_page_from_index<S: StorageBackend>(
    index: &VvbIndex,
    storage: &mut S,
    vvb_path: &str,
    anchor: PageAnchor,
    typo: &ReaderTypography,
) -> PageLayout {
    layout_page_with(index.block_count(), anchor, typo, |i| {
        index.read_block(storage, vvb_path, i).ok()
    })
}

/// Render a laid-out book page onto any `Canvas` strip with disciplined typographic chrome.
pub fn render_page(
    page: &PageLayout,
    typo: &ReaderTypography,
    canvas: &mut Canvas<'_>,
    chrome: &PageChrome<'_>,
) {
    let (_, right, _, left) = typo.margins();
    let col_w = PORTRAIT_W as i32 - left - right;

    // 1. Quiet fine-press running header (no boxes or decorative flourishes)
    if canvas.intersects_y(0, 76) {
        let hdr_y = SAFE_INSETS.top as i32 + 26;
        let header_label = if !chrome.chapter_title.is_empty() {
            chrome.chapter_title
        } else {
            chrome.book_title
        };
        canvas.draw_text_truncated(
            &baked_fonts::FONT_UI_19_SEMI,
            left,
            hdr_y,
            col_w - 110,
            header_label,
            Tone::Ink,
        );
        let pct = ((chrome.page_num + 1) * 100)
            .checked_div(chrome.total_pages)
            .unwrap_or(100);
        let pct_s = format!("{pct}%");
        canvas.draw_text_aligned(
            &baked_fonts::FONT_MONO_20_SEMI,
            PORTRAIT_W as i32 - right - if chrome.bookmarked { 20 } else { 0 },
            hdr_y,
            &pct_s,
            Tone::Ink,
            Align::Right,
        );

        if chrome.bookmarked {
            // Crisp flush top-edge ribbon bookmark
            canvas.fill_rect(PORTRAIT_W as i32 - right - 10, 0, 10, 28, Tone::Ink);
        }
    }

    // 2. Render typeset items
    for item in &page.items {
        let item_y = match item {
            PageItem::ChapterBanner { y_baseline, .. } => *y_baseline,
            PageItem::HeadingLine { y_baseline, .. } => *y_baseline,
            PageItem::DropCap { y_baseline, .. } => *y_baseline,
            PageItem::TextLine { y_baseline, .. } => *y_baseline,
            PageItem::CodeLine { y_baseline, .. } => *y_baseline,
            PageItem::SceneBreak { y_baseline } => *y_baseline,
        };
        if !canvas.intersects_y(item_y - 68, item_y + 24) {
            continue;
        }
        match item {
            PageItem::ChapterBanner { y_baseline, title } => {
                let font = if typo.font_family == FontFamily::Serif {
                    &baked_fonts::FONT_SERIF_27_BOLD
                } else {
                    &baked_fonts::FONT_SCHOOL_27_BOLD
                };
                canvas.draw_text_truncated(font, left, *y_baseline, col_w, title, Tone::Ink);
                canvas.fill_rect(left, *y_baseline + 14, 48, 3, Tone::Ink);
                canvas.hline_dotted(left + 56, *y_baseline + 15, (col_w - 56).max(0), 2, Tone::Ink);
            }
            PageItem::HeadingLine {
                y_baseline,
                level,
                text,
            } => {
                let font = if *level <= 2 {
                    &baked_fonts::FONT_SCHOOL_27_BOLD
                } else {
                    typo.fonts().bold
                };
                canvas.draw_text(font, left, *y_baseline, text, Tone::Ink);
            }
            PageItem::DropCap {
                x,
                y_baseline,
                glyph,
            } => {
                let mut buf = [0u8; 4];
                let s = glyph.encode_utf8(&mut buf);
                canvas.draw_text(&baked_fonts::FONT_SERIF_64_DROPCAP, *x, *y_baseline, s, Tone::Ink);
            }
            PageItem::TextLine {
                y_baseline,
                spans,
                is_quote,
                quote_x,
            } => {
                if *is_quote {
                    let adv = typo.line_advance();
                    canvas.fill_rect(*quote_x, y_baseline - typo.fonts().regular.ascent as i32, 3, adv, Tone::Charcoal);
                }
                for sp in spans {
                    let font = typo.font_for_style(sp.style);
                    canvas.draw_text(font, sp.x, *y_baseline, &sp.text, Tone::Ink);
                }
            }
            PageItem::CodeLine {
                y_top,
                y_baseline,
                x,
                w,
                text,
            } => {
                canvas.fill_rect(*x - 4, *y_top, *w + 8, 25, Tone::Paper);
                canvas.fill_rect(*x - 4, *y_top, 3, 25, Tone::Charcoal);
                canvas.draw_text(&baked_fonts::FONT_MONO_20_SEMI, *x + 6, *y_baseline, text, Tone::Ink);
            }
            PageItem::SceneBreak { y_baseline } => {
                canvas.fill_rect(PORTRAIT_W as i32 / 2 - 18, *y_baseline - 6, 36, 2, Tone::Ink);
            }
        }
    }

    // 3. Quiet fine-press tabular folio & 1-bpp-honest reading trace
    if canvas.intersects_y(748, PORTRAIT_H as i32) {
        let folio = format!("{} / {}", chrome.page_num + 1, chrome.total_pages.max(1));
        canvas.draw_text_aligned(
            &baked_fonts::FONT_MONO_20_SEMI,
            PORTRAIT_W as i32 / 2,
            770,
            &folio,
            Tone::Ink,
            Align::Center,
        );
        canvas.hline_dotted(left, 786, col_w, 3, Tone::Ink);
        if let Some(w) = ((chrome.page_num + 1) * col_w as usize).checked_div(chrome.total_pages) {
            let fill_w = (w as i32).max(4).min(col_w);
            canvas.fill_rect(left, 785, fill_w, 3, Tone::Ink);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vvbook_format::ingest_document;

    #[test]
    fn cyrillic_hyphenation_respects_signs_and_syllables() {
        let pts = hyphen::hyphen_points("подъезд");
        // Split must be after 'ъ', never before 'ъ'
        for p in &pts {
            let left = &"подъезд"[..*p];
            assert!(!left.ends_with("д"), "must not split before ъ: {}", left);
        }
        assert!(pts.iter().any(|&p| &"подъезд"[..p] == "подъ"));

        let pts2 = hyphen::hyphen_points("сильный");
        assert!(pts2.iter().any(|&p| &"сильный"[..p] == "силь"));

        let pts3 = hyphen::hyphen_points("молоко");
        assert!(!pts3.is_empty());
    }

    #[test]
    fn non_breaking_one_letter_prepositions_never_dangle_at_eol() {
        let runs = [TextRun {
            style: SpanStyle::Regular,
            text: "Я пошёл в лес и увидел у дуба прекрасную серебристую лисицу с пушистым хвостом о котором читал в старой книге.".into(),
        }];
        let typo = ReaderTypography::default();
        let (atoms, total_words, _) = tokenize_runs(&runs, &typo, 0);
        // Verify atoms for "в", "и", "у", "с", "о" have `glue_next == true`
        assert!(atoms.iter().any(|a| a.text == "в" && a.glue_next));
        assert!(atoms.iter().any(|a| a.text == "и" && a.glue_next));
        assert!(atoms.iter().any(|a| a.text == "у" && a.glue_next));

        let lines = break_atoms_knuth_plass(&atoms, total_words, 0, None, 36, 260, true);
        assert!(lines.len() >= 2);
        for ln in &lines[..lines.len() - 1] {
            let last_atom = &atoms[ln.last_atom_excl - 1];
            assert!(
                !last_atom.glue_next,
                "1-letter preposition '{}' stranded at EOL!",
                last_atom.text
            );
        }
    }

    #[test]
    fn pagination_handles_russian_poetry_drop_cap_and_renders() {
        let md = "# Глава I\n\nУ лукоморья дуб зелёный; златая цепь на дубе том: и днём и ночью кот учёный всё ходит по цепи кругом.\n\n> Ветер по морю гуляет и кораблик подгоняет.\n\n***\n\nДругой абзац с длинным текстом чтобы проверить перенос строк, буквицу и выравнивание по ширине полосы набора.";
        let book = ingest_document("saltan.md", md.as_bytes()).unwrap();
        let typo = ReaderTypography::default();
        let anchors = paginate_in_memory(&book, &typo);
        assert!(!anchors.is_empty());

        let page0 = layout_page_with(book.blocks.len(), anchors[0], &typo, |i| book.blocks.get(i).cloned());
        assert!(page0.items.iter().any(|it| matches!(it, PageItem::ChapterBanner { .. })));
        assert!(page0.items.iter().any(|it| matches!(it, PageItem::DropCap { glyph: 'У', .. })));

        // Render strip-wise into full frame buffer to prove strip rendering matches
        let mut fb = vec![0xFFu8; PORTRAIT_H * vvbook_pal::ROW_2BPP_BYTES];
        let mut canvas = Canvas::new(&mut fb, 0, PORTRAIT_H);
        canvas.clear(Tone::Paper);
        render_page(
            &page0,
            &typo,
            &mut canvas,
            &PageChrome {
                book_title: "Сказка о царе Салтане",
                chapter_title: "Глава I",
                page_num: 0,
                total_pages: anchors.len(),
                bookmarked: true,
                chin_hints: ["← Home", "Aa", "◄ Page", "Page ►"],
            },
        );
    }

    #[test]
    fn streaming_layout_index_matches_in_memory_pagination_all_presets() {
        use vvbook_format::VvbIndex;
        use vvbook_pal::{DirEntry, StorageBackend, StorageError};

        struct VecStore(Vec<u8>);
        impl StorageBackend for VecStore {
            fn exists(&self, _: &str) -> bool { true }
            fn file_len(&self, _: &str) -> Result<u64, StorageError> { Ok(self.0.len() as u64) }
            fn read_all(&self, _: &str) -> Result<Vec<u8>, StorageError> { Ok(self.0.clone()) }
            fn read_range(&self, _: &str, offset: u64, buf: &mut [u8]) -> Result<usize, StorageError> {
                let off = offset as usize;
                if off >= self.0.len() { return Ok(0); }
                let n = buf.len().min(self.0.len() - off);
                buf[..n].copy_from_slice(&self.0[off..off + n]);
                Ok(n)
            }
            fn write_all(&mut self, _: &str, _: &[u8]) -> Result<(), StorageError> { Ok(()) }
            fn write_atomic(&mut self, _: &str, _: &[u8]) -> Result<(), StorageError> { Ok(()) }
            fn append(&mut self, _: &str, _: &[u8]) -> Result<(), StorageError> { Ok(()) }
            fn remove(&mut self, _: &str) -> Result<(), StorageError> { Ok(()) }
            fn mkdir_all(&mut self, _: &str) -> Result<(), StorageError> { Ok(()) }
            fn list_dir(&self, _: &str) -> Result<Vec<DirEntry>, StorageError> { Ok(Vec::new()) }
        }

        let mut md = String::from("# Часть первая\n\n");
        for i in 1..=18 {
            md.push_str(&alloc::format!(
                "Глава {i}. Однажды зимой в старом петербургском доме зазвучала тихая музыка, и снег за окном искрился серебром.\n\n"
            ));
        }
        let book = ingest_document("story.md", md.as_bytes()).unwrap();
        let mut store = VecStore(book.to_bytes());
        let idx = VvbIndex::open_from_storage(&store, "/story.vvb").unwrap();

        let combos = [
            (FontFamily::Serif, FontSizePreset::Classic, MarginPreset::Editorial),
            (FontFamily::School, FontSizePreset::Large, MarginPreset::Wide),
            (FontFamily::Sans, FontSizePreset::Compact, MarginPreset::Narrow),
        ];
        for (fam, sz, mar) in combos {
            let typo = ReaderTypography {
                font_family: fam,
                font_size: sz,
                margin_preset: mar,
                ..ReaderTypography::default()
            };
            let mem_pages = paginate_in_memory(&book, &typo);
            let stream_pages = build_layout_index(&idx, &mut store, "/story.vvb", &typo);
            assert_eq!(stream_pages, mem_pages);
            let p_mem = layout_page_with(book.blocks.len(), mem_pages[1], &typo, |i| book.blocks.get(i).cloned());
            let p_str = layout_page_from_index(&idx, &mut store, "/story.vvb", stream_pages[1], &typo);
            assert_eq!(p_str.next_anchor, p_mem.next_anchor);
            assert_eq!(p_str.items.len(), p_mem.items.len());
        }
    }

    #[test]
    fn build_layout_index_matches_in_memory_pagination_exactly() {
        use vvbook_format::{SpanStyle, TextRun, TocEntry, VvbBook, BookMetadata};
        use vvbook_pal::DirEntry;

        struct MemStore(Vec<u8>);
        impl StorageBackend for MemStore {
            fn exists(&self, _: &str) -> bool { true }
            fn file_len(&self, _: &str) -> Result<u64, vvbook_pal::StorageError> { Ok(self.0.len() as u64) }
            fn read_all(&self, _: &str) -> Result<Vec<u8>, vvbook_pal::StorageError> { Ok(self.0.clone()) }
            fn read_range(&self, _: &str, offset: u64, buf: &mut [u8]) -> Result<usize, vvbook_pal::StorageError> {
                let off = offset as usize;
                if off >= self.0.len() { return Ok(0); }
                let n = buf.len().min(self.0.len() - off);
                buf[..n].copy_from_slice(&self.0[off..off + n]);
                Ok(n)
            }
            fn write_all(&mut self, _: &str, _: &[u8]) -> Result<(), vvbook_pal::StorageError> { Ok(()) }
            fn write_atomic(&mut self, _: &str, _: &[u8]) -> Result<(), vvbook_pal::StorageError> { Ok(()) }
            fn append(&mut self, _: &str, _: &[u8]) -> Result<(), vvbook_pal::StorageError> { Ok(()) }
            fn remove(&mut self, _: &str) -> Result<(), vvbook_pal::StorageError> { Ok(()) }
            fn mkdir_all(&mut self, _: &str) -> Result<(), vvbook_pal::StorageError> { Ok(()) }
            fn list_dir(&self, _: &str) -> Result<Vec<DirEntry>, vvbook_pal::StorageError> { Ok(Vec::new()) }
        }

        let book = VvbBook {
            meta: BookMetadata {
                id_hex: String::from("01234567"),
                title: String::from("Rich Typography Suite"),
                author: String::from("Bauhaus Press"),
                language: String::from("en"),
                source_format: String::from("MD"),
                word_count: 180,
                cover_w: 0,
                cover_h: 0,
                cover_2bpp: Vec::new(),
            },
            toc: vec![TocEntry { title: String::from("Ch 1"), block_index: 0 }],
            blocks: vec![
                Block::ChapterTitle { title: String::from("I. Form & Void") },
                Block::Paragraph {
                    runs: vec![TextRun {
                        style: SpanStyle::Regular,
                        text: String::from("Minimalism demands every geometric line and paragraph indent earn its place upon bare paper without ornamental clutter or wasted space across the measure."),
                    }],
                    first_of_chapter: true,
                    is_dialogue: false,
                },
                Block::Heading { level: 2, text: String::from("Subsection Geometry") },
                Block::VerseLine { text: String::from("Circle and bar wait quietly") },
                Block::VerseLine { text: String::from("Across the clean white margin") },
                Block::BlockQuote {
                    runs: vec![TextRun {
                        style: SpanStyle::Italic,
                        text: String::from("Typography is two-dimensional architecture held inside clear bounds."),
                    }],
                },
                Block::CodeBlock { text: String::from("let radius = 38;\nlet bar = 132;") },
                Block::ListItem {
                    bullet: String::from("•"),
                    runs: vec![TextRun { style: SpanStyle::Regular, text: String::from("First optical alignment point") }],
                },
                Block::SceneBreak,
                Block::Paragraph {
                    runs: vec![TextRun {
                        style: SpanStyle::Regular,
                        text: String::from("Second paragraph continuing cleanly across page transitions to verify drop caps, quotes, code lines, headings, verses, and scene breaks produce identical anchors."),
                    }],
                    first_of_chapter: false,
                    is_dialogue: false,
                },
            ],
            content_blake3: [7u8; 32],
        };

        let mut store = MemStore(book.to_bytes());
        let idx = VvbIndex::open_from_storage(&store, "/rich.vvb").unwrap();
        let typo = ReaderTypography::default();
        assert_eq!(build_layout_index(&idx, &mut store, "/rich.vvb", &typo), paginate_in_memory(&book, &typo));
    }

    #[test]
    fn overlong_token_and_russian_preposition_glue_never_overflow_margins() {
        let md = "Текст в лесу, к реке, с другом, у костра, о море. Сверхдлиннаястрокабезограниченийдопустимойшириныдляпроверкипереносаоченьдлинногослова1234567890.";
        let book = ingest_document("edge.md", md.as_bytes()).unwrap();
        let typo = ReaderTypography::default();
        let page = layout_page_with(book.blocks.len(), PageAnchor { block_idx: 0, word_offset: 0 }, &typo, |i| {
            book.blocks.get(i).cloned()
        });
        for it in &page.items {
            if let PageItem::TextLine { spans, .. } = it {
                for sp in spans {
                    assert!(sp.x >= 0 && sp.x < PORTRAIT_W as i32, "span x={} out of bounds", sp.x);
                }
            }
        }
    }
}
