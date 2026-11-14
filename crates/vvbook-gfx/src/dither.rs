extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;
use vvbook_pal::Tone;

/// Bill Atkinson's original 1/8 error-diffusion ditherer adapted for 4-tone E-Ink.
/// Diffuses 6/8 of quantization error across 6 neighbors, preserving luminous
/// highlights and crisp midtones without muddy Floyd-Steinberg worm artifacts.
pub fn atkinson_dither_4tone(gray: &[u8], w: usize, h: usize) -> Vec<u8> {
    let mut work: Vec<i16> = gray.iter().map(|&v| v as i16).collect();
    let total_px = w * h;
    let mut packed = vec![0xFFu8; total_px.div_ceil(4)];

    for y in 0..h {
        for x in 0..w {
            let idx = y * w + x;
            let old = work[idx].clamp(0, 255);
            // Quantize to 4 tones (0, 85, 170, 255)
            let (tone, new_val) = if old < 43 {
                (Tone::Ink, 0i16)
            } else if old < 128 {
                (Tone::Charcoal, 85i16)
            } else if old < 213 {
                (Tone::Silver, 170i16)
            } else {
                (Tone::Paper, 255i16)
            };

            let p_byte = idx >> 2;
            let shift = 6 - ((idx & 3) << 1);
            packed[p_byte] = (packed[p_byte] & !(0x03 << shift)) | ((tone as u8) << shift);

            let err = (old - new_val) >> 3; // 1/8 of error
            if err == 0 {
                continue;
            }
            if x + 1 < w {
                work[idx + 1] += err;
            }
            if x + 2 < w {
                work[idx + 2] += err;
            }
            if y + 1 < h {
                if x > 0 {
                    work[idx + w - 1] += err;
                }
                work[idx + w] += err;
                if x + 1 < w {
                    work[idx + w + 1] += err;
                }
            }
            if y + 2 < h {
                work[idx + 2 * w] += err;
            }
        }
    }
    packed
}

/// Bilinearly scale an 8-bit grayscale buffer to `(dst_w, dst_h)` and Atkinson-dither it to 2-bpp.
pub fn scale_and_dither_gray(src: &[u8], src_w: usize, src_h: usize, dst_w: usize, dst_h: usize) -> Vec<u8> {
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        return vec![0xFFu8; (dst_w * dst_h).div_ceil(4)];
    }
    let mut scaled = vec![255u8; dst_w * dst_h];
    for dy in 0..dst_h {
        let sy_fp = (dy * (src_h - 1) * 256) / dst_h.max(1);
        let sy0 = (sy_fp >> 8).min(src_h - 1);
        let sy1 = (sy0 + 1).min(src_h - 1);
        let fy = (sy_fp & 0xFF) as u32;
        for dx in 0..dst_w {
            let sx_fp = (dx * (src_w - 1) * 256) / dst_w.max(1);
            let sx0 = (sx_fp >> 8).min(src_w - 1);
            let sx1 = (sx0 + 1).min(src_w - 1);
            let fx = (sx_fp & 0xFF) as u32;

            let p00 = src[sy0 * src_w + sx0] as u32;
            let p10 = src[sy0 * src_w + sx1] as u32;
            let p01 = src[sy1 * src_w + sx0] as u32;
            let p11 = src[sy1 * src_w + sx1] as u32;

            let top = p00 * (256 - fx) + p10 * fx;
            let bot = p01 * (256 - fx) + p11 * fx;
            let val = ((top * (256 - fy) + bot * fy) >> 16).min(255) as u8;
            scaled[dy * dst_w + dx] = val;
        }
    }
    atkinson_dither_4tone(&scaled, dst_w, dst_h)
}
