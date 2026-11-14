#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod baked_fonts;
pub mod canvas;
pub mod dither;
pub mod font;
pub mod icons;
pub mod pencil;
pub mod qr;

pub use canvas::{Align, Canvas, X3_CHIN_H, X3_CHIN_W, X3_CHIN_X, X3_CHIN_Y, render_to_sink};
pub use font::BakedFont;
pub use icons::{ChessPieceKind, PipHat, PipMood, UiIcon};
pub use qr::QrCode;

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{vec, vec::Vec};
    use vvbook_pal::{DisplaySink, PORTRAIT_H, PORTRAIT_W, ROW_2BPP_BYTES, RefreshMode, Tone};

    struct TestSink {
        fb: Vec<u8>,
    }
    impl TestSink {
        fn new() -> Self {
            Self {
                fb: vec![0xFFu8; PORTRAIT_H * ROW_2BPP_BYTES],
            }
        }
    }
    impl DisplaySink for TestSink {
        fn begin_frame(&mut self, _mode: RefreshMode) {}
        fn write_strip_2bpp(&mut self, y0: usize, rows: usize, packed: &[u8]) {
            let start = y0 * ROW_2BPP_BYTES;
            let len = rows * ROW_2BPP_BYTES;
            self.fb[start..start + len].copy_from_slice(&packed[..len]);
        }
        fn end_frame(&mut self, _mode: RefreshMode) {}
    }

    #[test]
    fn strip_rendering_matches_full_frame_pixel_for_pixel() {
        let mut full_buf = vec![0xFFu8; PORTRAIT_H * ROW_2BPP_BYTES];
        let draw_complex_scene = |c: &mut Canvas<'_>| {
            c.clear(Tone::Paper);
            c.draw_drop_shadow(28, 36, 472, 160, 18);
            c.fill_rrect(28, 36, 472, 160, 18, Tone::Paper);
            c.stroke_rrect(28, 36, 472, 160, 18, 2, Tone::Ink);
            icons::draw_battery_capsule(c, 456, 44, 78, true);
            icons::draw_ui_icon(c, UiIcon::PocketPip, 44, 60, 44, Tone::Ink);
            icons::draw_ui_icon(c, UiIcon::Chess, 108, 60, 44, Tone::Charcoal);
            for (idx, &hat) in PipHat::ALL.iter().enumerate() {
                icons::draw_pip_fox(c, 70 + idx as i32 * 76, 330, 56, PipMood::Happy, hat);
            }
            icons::draw_starberry(c, 320, 250, 16, true);
            icons::draw_chess_piece(c, ChessPieceKind::Queen, true, true, 380, 260, 56);
            icons::draw_chess_piece(c, ChessPieceKind::Knight, false, false, 444, 260, 56);
            if let Some(qr) = QrCode::encode(b"http://192.168.4.1:8383/#pin=123456") {
                qr.draw(c, 60, 460, 4, 3);
            }
            c.hline_dotted(36, 360, 456, 2, Tone::Ink);
            c.vline_dotted(36, 370, 120, 3, Tone::Ink);
            c.fill_stipple_25(300, 380, 120, 80, Tone::Ink, Tone::Paper);
            c.fill_checker_50(300, 470, 120, 80, Tone::Ink, Tone::Paper);
            pencil::quad_bezier(c, 40, 620, 160, 580, 260, 650, 24, 2, Tone::Ink);
            let lines = c.draw_text_lines(
                &baked_fonts::FONT_SERIF_32_BOLD,
                36,
                590,
                456,
                38,
                3,
                "Form Follows Function Across Strips Without Visual Drift",
                Tone::Ink,
            );
            assert!(lines >= 2);
            c.draw_chin_bar(
                &baked_fonts::FONT_UI_19_SEMI,
                ["Back", "Open", "Prev", "Next"],
                Some(1),
            );
            c.draw_text(
                &baked_fonts::FONT_UI_25_BOLD,
                44,
                128,
                "Привет, мир! «Типографика» № 1 — 100%",
                Tone::Ink,
            );
        };

        let mut full = Canvas::new(&mut full_buf, 0, PORTRAIT_H);
        draw_complex_scene(&mut full);

        let mut strip_sink = TestSink::new();
        render_to_sink(&mut strip_sink, RefreshMode::FullGc, draw_complex_scene);
        assert_eq!(strip_sink.fb, full_buf);
    }

    #[test]
    fn all_33_baked_fonts_contain_full_russian_cyrillic_and_typographic_punctuation() {
        let upper_only: Vec<char> = ('А'..='Я').chain(['Ё', 'A', 'Z']).collect();
        for &ch in &upper_only {
            assert!(
                baked_fonts::FONT_SERIF_64_DROPCAP.glyph(ch).is_some(),
                "Dropcap missing '{}'",
                ch
            );
        }

        let fonts: [&BakedFont; 32] = [
            &baked_fonts::FONT_UI_19_SEMI,
            &baked_fonts::FONT_UI_22_SEMI,
            &baked_fonts::FONT_UI_25_BOLD,
            &baked_fonts::FONT_UI_32_BOLD,
            &baked_fonts::FONT_UI_44_BOLD,
            &baked_fonts::FONT_SCHOOL_23,
            &baked_fonts::FONT_SCHOOL_23_ITALIC,
            &baked_fonts::FONT_SCHOOL_23_BOLD,
            &baked_fonts::FONT_SCHOOL_27,
            &baked_fonts::FONT_SCHOOL_27_ITALIC,
            &baked_fonts::FONT_SCHOOL_27_BOLD,
            &baked_fonts::FONT_SCHOOL_32,
            &baked_fonts::FONT_SCHOOL_32_ITALIC,
            &baked_fonts::FONT_SCHOOL_32_BOLD,
            &baked_fonts::FONT_SCHOOL_38,
            &baked_fonts::FONT_SCHOOL_38_ITALIC,
            &baked_fonts::FONT_SCHOOL_38_BOLD,
            &baked_fonts::FONT_SERIF_23,
            &baked_fonts::FONT_SERIF_23_ITALIC,
            &baked_fonts::FONT_SERIF_23_BOLD,
            &baked_fonts::FONT_SERIF_27,
            &baked_fonts::FONT_SERIF_27_ITALIC,
            &baked_fonts::FONT_SERIF_27_BOLD,
            &baked_fonts::FONT_SERIF_32,
            &baked_fonts::FONT_SERIF_32_ITALIC,
            &baked_fonts::FONT_SERIF_32_BOLD,
            &baked_fonts::FONT_SERIF_38,
            &baked_fonts::FONT_SERIF_38_ITALIC,
            &baked_fonts::FONT_SERIF_38_BOLD,
            &baked_fonts::FONT_KID_22_BOLD,
            &baked_fonts::FONT_KID_30_BOLD,
            &baked_fonts::FONT_MONO_20_SEMI,
        ];
        let required: Vec<char> = ('А'..='Я')
            .chain('а'..='я')
            .chain(['Ё', 'ё', '«', '»', '—', '№', '•', '★', '✓', '→', '←', '▲', '▼', '◄', '►'])
            .collect();

        for font in fonts {
            for &ch in &required {
                assert!(
                    font.glyph(ch).is_some(),
                    "Baked font {:?} (px={}) missing required glyph '{}' (U+{:04X})",
                    font.name,
                    font.px,
                    ch,
                    ch as u32
                );
            }
        }
    }

    #[test]
    fn blend_coverage_preserves_polarity_for_1bpp_epd() {
        let mut buf = vec![0xFFu8; PORTRAIT_H * ROW_2BPP_BYTES];
        let mut c = Canvas::new(&mut buf, 0, PORTRAIT_H);
        c.clear(Tone::Paper);

        c.blend_coverage(10, 10, 2, Tone::Ink);
        assert!(c.get_pixel(10, 10).is_dark());
        c.blend_coverage(11, 10, 2, Tone::Charcoal);
        assert!(c.get_pixel(11, 10).is_dark());
        c.blend_coverage(12, 10, 1, Tone::Charcoal);
        assert_eq!(c.get_pixel(12, 10), Tone::Silver);

        c.fill_rect(20, 20, 4, 4, Tone::Ink);
        c.blend_coverage(21, 21, 2, Tone::Paper);
        assert!(!c.get_pixel(21, 21).is_dark());
        c.blend_coverage(22, 21, 1, Tone::Paper);
        assert!(c.get_pixel(22, 21).is_dark());
    }

    #[test]
    fn qr_encoder_has_valid_finders_and_respects_capacity() {
        let qr = QrCode::encode(b"http://192.168.4.1:8383/#pin=654321").unwrap();
        assert!(qr.size >= 21 && qr.size <= 41);
        // Top-left 7x7 finder outer border must be dark, inner ring light, 3x3 center dark
        for i in 0..7 {
            assert!(qr.get(i, 0));
            assert!(qr.get(0, i));
            assert!(qr.get(i, 6));
            assert!(qr.get(6, i));
        }
        assert!(!qr.get(1, 1));
        assert!(qr.get(3, 3));
        // Over-capacity payload gracefully returns None instead of panicking
        assert!(QrCode::encode(&[b'x'; 200]).is_none());
    }

    #[test]
    fn atkinson_dithering_preserves_extremes_and_four_tone_output() {
        let white = vec![255u8; 64 * 64];
        let black = vec![0u8; 64 * 64];
        let w_out = dither::atkinson_dither_4tone(&white, 64, 64);
        let b_out = dither::atkinson_dither_4tone(&black, 64, 64);
        assert!(w_out.iter().all(|&b| b == 0xFF));
        assert!(b_out.iter().all(|&b| b == 0x00));

        let mut grad = vec![0u8; 64 * 64];
        for y in 0..64 {
            for x in 0..64 {
                grad[y * 64 + x] = ((x * 255) / 63) as u8;
            }
        }
        let d = dither::scale_and_dither_gray(&grad, 64, 64, PORTRAIT_W / 4, PORTRAIT_H / 8);
        assert_eq!(d.len(), ((PORTRAIT_W / 4) * (PORTRAIT_H / 8)).div_ceil(4));
    }
}
