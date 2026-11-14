extern crate alloc;

use crate::canvas::Canvas;
use alloc::vec;
use alloc::vec::Vec;
use vvbook_pal::Tone;

/// Pure-Rust `#![no_std]` ISO/IEC 18004 QR Code encoder (Byte mode, ECC Level M, Versions 1..=6).
/// Supports up to 106 UTF-8 bytes — plenty for `http://192.168.4.1:8383/#pin=123456`.
#[derive(Clone, Debug)]
pub struct QrCode {
    pub size: usize,
    modules: Vec<u8>, // 1 = dark, 0 = light
}

struct VersionSpec {
    ver: usize,
    size: usize,
    data_cw: usize,
    ec_cw_per_block: usize,
    blocks: usize,
    align: &'static [usize],
}

// ISO/IEC 18004 Table 9, Error Correction Level M, Versions 1..=6
const SPECS: &[VersionSpec] = &[
    VersionSpec { ver: 1, size: 21, data_cw: 16, ec_cw_per_block: 10, blocks: 1, align: &[] },
    VersionSpec { ver: 2, size: 25, data_cw: 28, ec_cw_per_block: 16, blocks: 1, align: &[6, 18] },
    VersionSpec { ver: 3, size: 29, data_cw: 44, ec_cw_per_block: 26, blocks: 1, align: &[6, 22] },
    VersionSpec { ver: 4, size: 33, data_cw: 64, ec_cw_per_block: 18, blocks: 2, align: &[6, 26] },
    VersionSpec { ver: 5, size: 37, data_cw: 86, ec_cw_per_block: 24, blocks: 2, align: &[6, 30] },
    VersionSpec { ver: 6, size: 41, data_cw: 108, ec_cw_per_block: 16, blocks: 4, align: &[6, 34] },
];

impl QrCode {
    pub fn encode(data: &[u8]) -> Option<Self> {
        // Select smallest version whose Byte-mode capacity fits `data`
        let spec = SPECS.iter().find(|s| {
            let cap_bits = s.data_cw * 8;
            // 4-bit mode + 8-bit count + 8*len + 4-bit terminator (max)
            4 + 8 + data.len() * 8 <= cap_bits
        })?;

        let mut bits = BitBuf::new(spec.data_cw * 8);
        bits.push(0b0100, 4); // Byte mode
        bits.push(data.len() as u32, 8);
        for &b in data {
            bits.push(b as u32, 8);
        }
        let rem = spec.data_cw * 8 - bits.len;
        bits.push(0, rem.min(4));
        while !bits.len.is_multiple_of(8) {
            bits.push(0, 1);
        }
        let mut pad_idx = 0usize;
        while bits.len < spec.data_cw * 8 {
            bits.push(if pad_idx.is_multiple_of(2) { 0xEC } else { 0x11 }, 8);
            pad_idx += 1;
        }

        // Split into equal blocks and compute Reed-Solomon ECC codewords
        let block_data_len = spec.data_cw / spec.blocks;
        let poly = rs_generator(spec.ec_cw_per_block);
        let mut data_blocks = Vec::with_capacity(spec.blocks);
        let mut ec_blocks = Vec::with_capacity(spec.blocks);
        for b in 0..spec.blocks {
            let start = b * block_data_len;
            let slice = &bits.bytes[start..start + block_data_len];
            data_blocks.push(slice.to_vec());
            ec_blocks.push(rs_remainder(slice, &poly));
        }

        // Interleave data then ECC codewords
        let mut final_cw = Vec::with_capacity(spec.data_cw + spec.blocks * spec.ec_cw_per_block);
        for i in 0..block_data_len {
            for blk in &data_blocks {
                final_cw.push(blk[i]);
            }
        }
        for i in 0..spec.ec_cw_per_block {
            for blk in &ec_blocks {
                final_cw.push(blk[i]);
            }
        }

        // Build matrix + function mask
        let n = spec.size;
        let mut base = vec![0u8; n * n];
        let mut is_func = vec![false; n * n];
        draw_function_patterns(spec, &mut base, &mut is_func);
        place_codewords(n, &final_cw, &mut base, &is_func);

        // Evaluate all 8 masks and keep lowest penalty
        let mut best_modules = base.clone();
        let mut best_penalty = u32::MAX;
        for mask in 0..8u8 {
            let mut candidate = base.clone();
            apply_mask(n, mask, &mut candidate, &is_func);
            draw_format_bits(n, mask, &mut candidate);
            let p = penalty_score(n, &candidate);
            if p < best_penalty {
                best_penalty = p;
                best_modules = candidate;
            }
        }

        Some(Self {
            size: n,
            modules: best_modules,
        })
    }

    #[inline]
    pub fn get(&self, x: usize, y: usize) -> bool {
        self.modules[y * self.size + x] != 0
    }

    /// Render onto `Canvas` centered in a crisp white quiet zone (`quiet` modules around).
    pub fn draw(&self, canvas: &mut Canvas<'_>, x: i32, y: i32, module_px: i32, quiet: i32) {
        let total = (self.size as i32 + quiet * 2) * module_px;
        canvas.fill_rect(x, y, total, total, Tone::Paper);
        for my in 0..self.size {
            let py = y + (my as i32 + quiet) * module_px;
            if !canvas.intersects_y(py, py + module_px) {
                continue;
            }
            for mx in 0..self.size {
                if self.get(mx, my) {
                    let px = x + (mx as i32 + quiet) * module_px;
                    canvas.fill_rect(px, py, module_px, module_px, Tone::Ink);
                }
            }
        }
    }
}

struct BitBuf {
    bytes: Vec<u8>,
    len: usize,
}

impl BitBuf {
    fn new(cap_bits: usize) -> Self {
        Self {
            bytes: vec![0u8; cap_bits.div_ceil(8)],
            len: 0,
        }
    }

    fn push(&mut self, val: u32, count: usize) {
        for i in (0..count).rev() {
            let bit = ((val >> i) & 1) as u8;
            if bit != 0 {
                self.bytes[self.len >> 3] |= 1 << (7 - (self.len & 7));
            }
            self.len += 1;
        }
    }
}

fn gf_mul(mut a: u8, mut b: u8) -> u8 {
    let mut r = 0u8;
    for _ in 0..8 {
        if b & 1 != 0 {
            r ^= a;
        }
        let hi = a & 0x80;
        a <<= 1;
        if hi != 0 {
            a ^= 0x1D; // GF(256) primitive polynomial x^8 + x^4 + x^3 + x^2 + 1
        }
        b >>= 1;
    }
    r
}

fn rs_generator(degree: usize) -> Vec<u8> {
    let mut res = vec![0u8; degree];
    res[degree - 1] = 1;
    let mut root = 1u8;
    for _ in 0..degree {
        for j in 0..degree {
            res[j] = gf_mul(res[j], root);
            if j + 1 < degree {
                res[j] ^= res[j + 1];
            }
        }
        root = gf_mul(root, 0x02);
    }
    res
}

fn rs_remainder(data: &[u8], poly: &[u8]) -> Vec<u8> {
    let mut rem = vec![0u8; poly.len()];
    for &b in data {
        let factor = b ^ rem[0];
        rem.copy_within(1.., 0);
        rem[poly.len() - 1] = 0;
        for (r, &g) in rem.iter_mut().zip(poly.iter()) {
            *r ^= gf_mul(g, factor);
        }
    }
    rem
}

fn set_func(n: usize, x: usize, y: usize, dark: bool, m: &mut [u8], f: &mut [bool]) {
    let idx = y * n + x;
    m[idx] = if dark { 1 } else { 0 };
    f[idx] = true;
}

fn draw_finder(n: usize, ox: usize, oy: usize, m: &mut [u8], f: &mut [bool]) {
    for dy in -1i32..=7 {
        for dx in -1i32..=7 {
            let x = ox as i32 + dx;
            let y = oy as i32 + dy;
            if x >= 0 && x < n as i32 && y >= 0 && y < n as i32 {
                let dist = dx.abs_diff(3).max(dy.abs_diff(3));
                let dark = dist != 2 && dist <= 3;
                set_func(n, x as usize, y as usize, dark, m, f);
            }
        }
    }
}

fn draw_alignment(n: usize, cx: usize, cy: usize, m: &mut [u8], f: &mut [bool]) {
    for dy in -2i32..=2 {
        for dx in -2i32..=2 {
            let dist = dx.abs().max(dy.abs());
            let dark = dist != 1;
            set_func(n, (cx as i32 + dx) as usize, (cy as i32 + dy) as usize, dark, m, f);
        }
    }
}

fn draw_function_patterns(spec: &VersionSpec, m: &mut [u8], f: &mut [bool]) {
    let n = spec.size;
    // Timing strips
    for i in 0..n {
        set_func(n, 6, i, i.is_multiple_of(2), m, f);
        set_func(n, i, 6, i.is_multiple_of(2), m, f);
    }
    // Finders
    draw_finder(n, 0, 0, m, f);
    draw_finder(n, n - 7, 0, m, f);
    draw_finder(n, 0, n - 7, m, f);
    // Alignments
    let len = spec.align.len();
    for (i, &ay) in spec.align.iter().enumerate() {
        for (j, &ax) in spec.align.iter().enumerate() {
            let on_finder = (i == 0 && j == 0) || (i == 0 && j == len - 1) || (i == len - 1 && j == 0);
            if !on_finder {
                draw_alignment(n, ax, ay, m, f);
            }
        }
    }
    // Reserve format info cells & dark module
    for i in 0..=8 {
        if i != 6 {
            set_func(n, 8, i, false, m, f);
            set_func(n, i, 8, false, m, f);
        }
    }
    for i in 0..7 {
        set_func(n, 8, n - 1 - i, false, m, f);
    }
    for i in 0..8 {
        set_func(n, n - 1 - i, 8, false, m, f);
    }
    set_func(n, 8, n - 8, true, m, f);
    let _ = spec.ver;
}

fn place_codewords(n: usize, cw: &[u8], m: &mut [u8], f: &[bool]) {
    let mut bit_idx = 0usize;
    let total_bits = cw.len() * 8;
    let mut right = (n - 1) as i32;
    let mut upward = true;
    while right >= 1 {
        if right == 6 {
            right -= 1;
        }
        for step in 0..n {
            let y = if upward { n - 1 - step } else { step };
            for dx in 0..2 {
                let x = (right - dx) as usize;
                if !f[y * n + x] {
                    let dark = if bit_idx < total_bits {
                        ((cw[bit_idx >> 3] >> (7 - (bit_idx & 7))) & 1) != 0
                    } else {
                        false
                    };
                    m[y * n + x] = if dark { 1 } else { 0 };
                    bit_idx += 1;
                }
            }
        }
        upward = !upward;
        right -= 2;
    }
}

fn apply_mask(n: usize, mask: u8, m: &mut [u8], f: &[bool]) {
    for y in 0..n {
        for x in 0..n {
            if f[y * n + x] {
                continue;
            }
            let flip = match mask {
                0 => (x + y).is_multiple_of(2),
                1 => y.is_multiple_of(2),
                2 => x.is_multiple_of(3),
                3 => (x + y).is_multiple_of(3),
                4 => (y / 2 + x / 3).is_multiple_of(2),
                5 => (x * y) % 2 + (x * y) % 3 == 0,
                6 => ((x * y) % 2 + (x * y) % 3).is_multiple_of(2),
                _ => ((x + y) % 2 + (x * y) % 3).is_multiple_of(2),
            };
            if flip {
                m[y * n + x] ^= 1;
            }
        }
    }
}

fn draw_format_bits(n: usize, mask: u8, m: &mut [u8]) {
    // ECC level M = 0b00, 3-bit mask -> 5-bit format data
    let data = mask as u32;
    let mut rem = data << 10;
    for i in (0..5).rev() {
        if (rem >> (i + 10)) & 1 != 0 {
            rem ^= 0x537 << i;
        }
    }
    let bits = ((data << 10) | rem) ^ 0x5412;

    for i in 0..=5 {
        let v = ((bits >> i) & 1) as u8;
        m[i * n + 8] = v;
    }
    m[7 * n + 8] = ((bits >> 6) & 1) as u8;
    m[8 * n + 8] = ((bits >> 7) & 1) as u8;
    m[8 * n + 7] = ((bits >> 8) & 1) as u8;
    for i in 9..15 {
        m[8 * n + (14 - i)] = ((bits >> i) & 1) as u8;
    }
    for i in 0..8 {
        m[8 * n + (n - 1 - i)] = ((bits >> i) & 1) as u8;
    }
    for i in 8..15 {
        m[(n - 15 + i) * n + 8] = ((bits >> i) & 1) as u8;
    }
}

fn penalty_score(n: usize, m: &[u8]) -> u32 {
    let mut score = 0u32;
    for y in 0..n {
        let mut run = 1u32;
        for x in 1..n {
            if m[y * n + x] == m[y * n + x - 1] {
                run += 1;
            } else {
                if run >= 5 {
                    score += run - 2;
                }
                run = 1;
            }
        }
        if run >= 5 {
            score += run - 2;
        }
    }
    for x in 0..n {
        let mut run = 1u32;
        for y in 1..n {
            if m[y * n + x] == m[(y - 1) * n + x] {
                run += 1;
            } else {
                if run >= 5 {
                    score += run - 2;
                }
                run = 1;
            }
        }
        if run >= 5 {
            score += run - 2;
        }
    }
    for y in 0..n - 1 {
        for x in 0..n - 1 {
            let c = m[y * n + x];
            if m[y * n + x + 1] == c && m[(y + 1) * n + x] == c && m[(y + 1) * n + x + 1] == c {
                score += 3;
            }
        }
    }
    score
}
