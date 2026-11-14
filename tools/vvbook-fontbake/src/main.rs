use fontdue::{Font, FontSettings};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

struct FontSpec {
    const_name: &'static str,
    primary_path: &'static str,
    fallback_path: &'static str,
    px: f32,
    dropcap_only: bool,
}

const SPECS: &[FontSpec] = &[
    // Cupertino UI Sans (OpenSans Semibold/Bold — verified Cyrillic + crisp hinting at 257 PPI)
    FontSpec {
        const_name: "FONT_UI_19_SEMI",
        primary_path: "/usr/share/fonts/open-sans/OpenSans-Semibold.ttf",
        fallback_path: "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans-Bold.ttf",
        px: 19.0,
        dropcap_only: false,
    },
    FontSpec {
        const_name: "FONT_UI_22_SEMI",
        primary_path: "/usr/share/fonts/open-sans/OpenSans-Semibold.ttf",
        fallback_path: "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans-Bold.ttf",
        px: 22.0,
        dropcap_only: false,
    },
    FontSpec {
        const_name: "FONT_UI_25_BOLD",
        primary_path: "/usr/share/fonts/open-sans/OpenSans-Bold.ttf",
        fallback_path: "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans-Bold.ttf",
        px: 25.0,
        dropcap_only: false,
    },
    FontSpec {
        const_name: "FONT_UI_32_BOLD",
        primary_path: "/usr/share/fonts/open-sans/OpenSans-Bold.ttf",
        fallback_path: "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans-Bold.ttf",
        px: 32.0,
        dropcap_only: false,
    },
    FontSpec {
        const_name: "FONT_UI_44_BOLD",
        primary_path: "/usr/share/fonts/open-sans/OpenSans-Bold.ttf",
        fallback_path: "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans-Bold.ttf",
        px: 44.0,
        dropcap_only: false,
    },
    // Primary Book Serif — Century Schoolbook (C059 — wide counters, sturdy stems for 257 PPI E-Ink)
    FontSpec {
        const_name: "FONT_SCHOOL_23",
        primary_path: "/usr/share/fonts/urw-base35/C059-Roman.otf",
        fallback_path: "/usr/share/fonts/liberation-serif-fonts/LiberationSerif-Regular.ttf",
        px: 23.0,
        dropcap_only: false,
    },
    FontSpec {
        const_name: "FONT_SCHOOL_23_ITALIC",
        primary_path: "/usr/share/fonts/urw-base35/C059-Italic.otf",
        fallback_path: "/usr/share/fonts/liberation-serif-fonts/LiberationSerif-Italic.ttf",
        px: 23.0,
        dropcap_only: false,
    },
    FontSpec {
        const_name: "FONT_SCHOOL_23_BOLD",
        primary_path: "/usr/share/fonts/urw-base35/C059-Bold.otf",
        fallback_path: "/usr/share/fonts/liberation-serif-fonts/LiberationSerif-Bold.ttf",
        px: 23.0,
        dropcap_only: false,
    },
    FontSpec {
        const_name: "FONT_SCHOOL_27",
        primary_path: "/usr/share/fonts/urw-base35/C059-Roman.otf",
        fallback_path: "/usr/share/fonts/liberation-serif-fonts/LiberationSerif-Regular.ttf",
        px: 27.0,
        dropcap_only: false,
    },
    FontSpec {
        const_name: "FONT_SCHOOL_27_ITALIC",
        primary_path: "/usr/share/fonts/urw-base35/C059-Italic.otf",
        fallback_path: "/usr/share/fonts/liberation-serif-fonts/LiberationSerif-Italic.ttf",
        px: 27.0,
        dropcap_only: false,
    },
    FontSpec {
        const_name: "FONT_SCHOOL_27_BOLD",
        primary_path: "/usr/share/fonts/urw-base35/C059-Bold.otf",
        fallback_path: "/usr/share/fonts/liberation-serif-fonts/LiberationSerif-Bold.ttf",
        px: 27.0,
        dropcap_only: false,
    },
    FontSpec {
        const_name: "FONT_SCHOOL_32",
        primary_path: "/usr/share/fonts/urw-base35/C059-Roman.otf",
        fallback_path: "/usr/share/fonts/liberation-serif-fonts/LiberationSerif-Regular.ttf",
        px: 32.0,
        dropcap_only: false,
    },
    FontSpec {
        const_name: "FONT_SCHOOL_32_ITALIC",
        primary_path: "/usr/share/fonts/urw-base35/C059-Italic.otf",
        fallback_path: "/usr/share/fonts/liberation-serif-fonts/LiberationSerif-Italic.ttf",
        px: 32.0,
        dropcap_only: false,
    },
    FontSpec {
        const_name: "FONT_SCHOOL_32_BOLD",
        primary_path: "/usr/share/fonts/urw-base35/C059-Bold.otf",
        fallback_path: "/usr/share/fonts/liberation-serif-fonts/LiberationSerif-Bold.ttf",
        px: 32.0,
        dropcap_only: false,
    },
    FontSpec {
        const_name: "FONT_SCHOOL_38",
        primary_path: "/usr/share/fonts/urw-base35/C059-Roman.otf",
        fallback_path: "/usr/share/fonts/liberation-serif-fonts/LiberationSerif-Regular.ttf",
        px: 38.0,
        dropcap_only: false,
    },
    FontSpec {
        const_name: "FONT_SCHOOL_38_ITALIC",
        primary_path: "/usr/share/fonts/urw-base35/C059-Italic.otf",
        fallback_path: "/usr/share/fonts/liberation-serif-fonts/LiberationSerif-Italic.ttf",
        px: 38.0,
        dropcap_only: false,
    },
    FontSpec {
        const_name: "FONT_SCHOOL_38_BOLD",
        primary_path: "/usr/share/fonts/urw-base35/C059-Bold.otf",
        fallback_path: "/usr/share/fonts/liberation-serif-fonts/LiberationSerif-Bold.ttf",
        px: 38.0,
        dropcap_only: false,
    },
    // Secondary Literary Serif — Palatino (P052 — Apple Books aesthetic + full Cyrillic)
    FontSpec {
        const_name: "FONT_SERIF_23",
        primary_path: "/usr/share/fonts/urw-base35/P052-Roman.otf",
        fallback_path: "/usr/share/fonts/liberation-serif-fonts/LiberationSerif-Regular.ttf",
        px: 23.0,
        dropcap_only: false,
    },
    FontSpec {
        const_name: "FONT_SERIF_23_ITALIC",
        primary_path: "/usr/share/fonts/urw-base35/P052-Italic.otf",
        fallback_path: "/usr/share/fonts/liberation-serif-fonts/LiberationSerif-Italic.ttf",
        px: 23.0,
        dropcap_only: false,
    },
    FontSpec {
        const_name: "FONT_SERIF_23_BOLD",
        primary_path: "/usr/share/fonts/urw-base35/P052-Bold.otf",
        fallback_path: "/usr/share/fonts/liberation-serif-fonts/LiberationSerif-Bold.ttf",
        px: 23.0,
        dropcap_only: false,
    },
    FontSpec {
        const_name: "FONT_SERIF_27",
        primary_path: "/usr/share/fonts/urw-base35/P052-Roman.otf",
        fallback_path: "/usr/share/fonts/liberation-serif-fonts/LiberationSerif-Regular.ttf",
        px: 27.0,
        dropcap_only: false,
    },
    FontSpec {
        const_name: "FONT_SERIF_27_ITALIC",
        primary_path: "/usr/share/fonts/urw-base35/P052-Italic.otf",
        fallback_path: "/usr/share/fonts/liberation-serif-fonts/LiberationSerif-Italic.ttf",
        px: 27.0,
        dropcap_only: false,
    },
    FontSpec {
        const_name: "FONT_SERIF_27_BOLD",
        primary_path: "/usr/share/fonts/urw-base35/P052-Bold.otf",
        fallback_path: "/usr/share/fonts/liberation-serif-fonts/LiberationSerif-Bold.ttf",
        px: 27.0,
        dropcap_only: false,
    },
    FontSpec {
        const_name: "FONT_SERIF_32",
        primary_path: "/usr/share/fonts/urw-base35/P052-Roman.otf",
        fallback_path: "/usr/share/fonts/liberation-serif-fonts/LiberationSerif-Regular.ttf",
        px: 32.0,
        dropcap_only: false,
    },
    FontSpec {
        const_name: "FONT_SERIF_32_ITALIC",
        primary_path: "/usr/share/fonts/urw-base35/P052-Italic.otf",
        fallback_path: "/usr/share/fonts/liberation-serif-fonts/LiberationSerif-Italic.ttf",
        px: 32.0,
        dropcap_only: false,
    },
    FontSpec {
        const_name: "FONT_SERIF_32_BOLD",
        primary_path: "/usr/share/fonts/urw-base35/P052-Bold.otf",
        fallback_path: "/usr/share/fonts/liberation-serif-fonts/LiberationSerif-Bold.ttf",
        px: 32.0,
        dropcap_only: false,
    },
    FontSpec {
        const_name: "FONT_SERIF_38",
        primary_path: "/usr/share/fonts/urw-base35/P052-Roman.otf",
        fallback_path: "/usr/share/fonts/liberation-serif-fonts/LiberationSerif-Regular.ttf",
        px: 38.0,
        dropcap_only: false,
    },
    FontSpec {
        const_name: "FONT_SERIF_38_ITALIC",
        primary_path: "/usr/share/fonts/urw-base35/P052-Italic.otf",
        fallback_path: "/usr/share/fonts/liberation-serif-fonts/LiberationSerif-Italic.ttf",
        px: 38.0,
        dropcap_only: false,
    },
    FontSpec {
        const_name: "FONT_SERIF_38_BOLD",
        primary_path: "/usr/share/fonts/urw-base35/P052-Bold.otf",
        fallback_path: "/usr/share/fonts/liberation-serif-fonts/LiberationSerif-Bold.ttf",
        px: 38.0,
        dropcap_only: false,
    },
    FontSpec {
        const_name: "FONT_SERIF_64_DROPCAP",
        primary_path: "/usr/share/fonts/urw-base35/C059-Bold.otf",
        fallback_path: "/usr/share/fonts/liberation-serif-fonts/LiberationSerif-Bold.ttf",
        px: 64.0,
        dropcap_only: true,
    },
    // Playful Kid Display (Comfortaa Bold — verified Cyrillic!)
    FontSpec {
        const_name: "FONT_KID_22_BOLD",
        primary_path: "/usr/share/fonts/aajohan-comfortaa-fonts/Comfortaa-Bold.otf",
        fallback_path: "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans-Bold.ttf",
        px: 22.0,
        dropcap_only: false,
    },
    FontSpec {
        const_name: "FONT_KID_30_BOLD",
        primary_path: "/usr/share/fonts/aajohan-comfortaa-fonts/Comfortaa-Bold.otf",
        fallback_path: "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans-Bold.ttf",
        px: 30.0,
        dropcap_only: false,
    },
    // Monospace (SourceCodePro Semibold + LiberationMono Bold fallback)
    FontSpec {
        const_name: "FONT_MONO_20_SEMI",
        primary_path: "/usr/share/fonts/adobe-source-code-pro-fonts/SourceCodePro-Semibold.otf",
        fallback_path: "/usr/share/fonts/liberation-mono-fonts/LiberationMono-Bold.ttf",
        px: 20.0,
        dropcap_only: false,
    },
];

fn build_charset(dropcap_only: bool) -> Vec<char> {
    let mut set = BTreeSet::new();
    if dropcap_only {
        for c in 'A'..='Z' {
            set.insert(c);
        }
        for c in '0'..='9' {
            set.insert(c);
        }
        for cp in 0x0410u32..=0x042F {
            if let Some(c) = char::from_u32(cp) {
                set.insert(c);
            }
        }
        for c in ['Ё', 'Є', 'І', 'Ї', 'Ґ', 'Ў', '«', '„', '“', '‘', '—', ' '] {
            set.insert(c);
        }
        return set.into_iter().collect();
    }

    // ASCII printable
    for cp in 0x0020u32..=0x007E {
        if let Some(c) = char::from_u32(cp) {
            set.insert(c);
        }
    }
    // Essential Latin-1
    for cp in 0x00A0u32..=0x00FF {
        if let Some(c) = char::from_u32(cp) {
            set.insert(c);
        }
    }
    // Full Unicode Cyrillic Basic (U+0400..=U+045F) + Ukrainian Ghe with upturn (U+0490..=U+0491)
    for cp in 0x0400u32..=0x045F {
        if let Some(c) = char::from_u32(cp) {
            set.insert(c);
        }
    }
    set.insert('\u{0490}');
    set.insert('\u{0491}');

    // Literary typography & UI punctuation
    let extra = [
        '«', '»', '„', '“', '”', '‘', '’', '—', '–', '‐', '…', '№', '§', '•', '·', '°', '©', '™',
        '₽', '€', '★', '☆', '✓', '→', '←', '↑', '↓', '▲', '▼', '◄', '►', '◀', '▶', '↶', '↺',
        '×', '÷', '±', '≈', '≠', '≤', '≥', '♦', '♥', '♠', '♣',
        '\u{00AD}', // soft hyphen renders as '-' when drawn explicitly
    ];
    for c in extra {
        set.insert(c);
    }

    set.into_iter().collect()
}

fn load_font(path: &str) -> Font {
    let bytes = fs::read(path).unwrap_or_else(|e| panic!("failed to read font {path}: {e}"));
    Font::from_bytes(bytes, FontSettings::default())
        .unwrap_or_else(|e| panic!("failed to parse font {path}: {e}"))
}

fn quantize_2bpp(cov: u8) -> u8 {
    // Darkened optical E-Ink gamma curve for 257 PPI so stems stay full-bodied
    // on both 1-bpp and 2-bpp waveforms without eroding serif hairlines.
    if cov >= 164 {
        3
    } else if cov >= 88 {
        2
    } else if cov >= 36 {
        1
    } else {
        0
    }
}

fn pack_2bpp(bitmap: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bitmap.len().div_ceil(4));
    for chunk in bitmap.chunks(4) {
        let mut b = 0u8;
        for (i, &cov) in chunk.iter().enumerate() {
            let q = quantize_2bpp(cov);
            b |= q << (6 - i * 2);
        }
        out.push(b);
    }
    out
}

fn must_have_cyrillic_bitmap(c: char) -> bool {
    matches!(c, 'А'..='Я' | 'а'..='я' | 'Ё' | 'ё' | 'І' | 'і' | 'Ї' | 'ї' | 'Є' | 'є' | 'Ґ' | 'ґ' | 'Ў' | 'ў')
}

fn main() {
    let mut out = String::new();
    out.push_str("// @generated by tools/vvbook-fontbake. Do not edit manually.\n");
    out.push_str("#![allow(clippy::excessive_precision, clippy::unreadable_literal)]\n\n");
    out.push_str("use crate::font::{BakedFont, GlyphEntry, KernPair};\n\n");

    let symbol_fallback = load_font("/usr/share/fonts/dejavu-sans-fonts/DejaVuSans-Bold.ttf");

    let mut total_bytes = 0usize;
    let mut font_cache: BTreeMap<&'static str, Font> = BTreeMap::new();

    for spec in SPECS {
        if !font_cache.contains_key(spec.primary_path) {
            font_cache.insert(spec.primary_path, load_font(spec.primary_path));
        }
        if !font_cache.contains_key(spec.fallback_path) {
            font_cache.insert(spec.fallback_path, load_font(spec.fallback_path));
        }
        let primary = &font_cache[spec.primary_path];
        let fallback = &font_cache[spec.fallback_path];

        let line_metrics = primary
            .horizontal_line_metrics(spec.px)
            .unwrap_or_else(|| fallback.horizontal_line_metrics(spec.px).unwrap());
        let ascent = line_metrics.ascent.round() as i16;
        let descent = line_metrics.descent.round() as i16;
        let line_gap = line_metrics.line_gap.round() as i16;

        let chars = build_charset(spec.dropcap_only);
        let mut glyph_entries = Vec::with_capacity(chars.len());
        let mut atlas = Vec::<u8>::new();

        for &ch in &chars {
            let raster_ch = if ch == '\u{00AD}' { '-' } else { ch };
            let use_font = if primary.lookup_glyph_index(raster_ch) != 0 {
                primary
            } else if fallback.lookup_glyph_index(raster_ch) != 0 {
                fallback
            } else {
                &symbol_fallback
            };

            let (metrics, bitmap) = use_font.rasterize(raster_ch, spec.px);

            if must_have_cyrillic_bitmap(ch) && (metrics.width == 0 || metrics.height == 0) {
                panic!(
                    "CRITICAL: Cyrillic glyph '{}' (U+{:04X}) missing in {} / {}",
                    ch, ch as u32, spec.primary_path, spec.fallback_path
                );
            }

            let packed = pack_2bpp(&bitmap);
            let offset = atlas.len() as u32;
            atlas.extend_from_slice(&packed);

            let adv_q4 = (metrics.advance_width * 16.0).round() as i16;
            glyph_entries.push((
                ch as u32,
                metrics.xmin as i8,
                metrics.ymin as i8,
                metrics.width as u8,
                metrics.height as u8,
                adv_q4,
                offset,
            ));
        }

        // Collect non-zero kerning pairs for primary font across baked charset.
        let mut kerns = Vec::<(u32, i8)>::new();
        for (li, &lc) in chars.iter().enumerate() {
            if primary.lookup_glyph_index(lc) == 0 {
                continue;
            }
            for (ri, &rc) in chars.iter().enumerate() {
                if primary.lookup_glyph_index(rc) == 0 {
                    continue;
                }
                if let Some(k) = primary.horizontal_kern(lc, rc, spec.px) {
                    let k_q4 = (k * 16.0).round() as i32;
                    if k_q4 != 0 && (-127..=127).contains(&k_q4) {
                        let key = ((li as u32) << 16) | (ri as u32);
                        kerns.push((key, k_q4 as i8));
                    }
                }
            }
        }

        total_bytes += atlas.len() + glyph_entries.len() * 14 + kerns.len() * 6;
        eprintln!(
            "{}: {} glyphs, {} kerns, {} bytes atlas",
            spec.const_name,
            glyph_entries.len(),
            kerns.len(),
            atlas.len()
        );

        let lower = spec.const_name.to_lowercase();
        out.push_str(&format!(
            "static {upper}_GLYPHS: &[GlyphEntry] = &[\n",
            upper = spec.const_name
        ));
        for (cp, xmin, ymin, w, h, adv_q4, offset) in &glyph_entries {
            out.push_str(&format!(
                "    GlyphEntry {{ codepoint: 0x{cp:04X}, xmin: {xmin}, ymin: {ymin}, width: {w}, height: {h}, advance_q4: {adv_q4}, offset: {offset} }},\n"
            ));
        }
        out.push_str("];\n\n");

        out.push_str(&format!(
            "static {upper}_KERNS: &[KernPair] = &[\n",
            upper = spec.const_name
        ));
        for (key, delta_q4) in &kerns {
            out.push_str(&format!(
                "    KernPair {{ key: 0x{key:08X}, delta_q4: {delta_q4} }},\n"
            ));
        }
        out.push_str("];\n\n");

        out.push_str(&format!(
            "static {upper}_ATLAS: &[u8] = &[\n",
            upper = spec.const_name
        ));
        for chunk in atlas.chunks(16) {
            out.push_str("    ");
            for b in chunk {
                out.push_str(&format!("0x{b:02X}, "));
            }
            out.push('\n');
        }
        out.push_str("];\n\n");

        let px_u8 = spec.px.round() as u8;
        out.push_str(&format!(
            "pub const {upper}: BakedFont = BakedFont {{\n    name: \"{lower}\",\n    px: {px_u8},\n    ascent: {ascent},\n    descent: {descent},\n    line_gap: {line_gap},\n    glyphs: {upper}_GLYPHS,\n    kerns: {upper}_KERNS,\n    atlas: {upper}_ATLAS,\n}};\n\n",
            upper = spec.const_name,
            lower = lower
        ));
    }

    let dest = Path::new("crates/vvbook-gfx/src/baked_fonts.rs");
    fs::write(dest, out).expect("failed to write baked_fonts.rs");
    eprintln!("Total baked font rodata size: {:.1} KB", total_bytes as f64 / 1024.0);
}
