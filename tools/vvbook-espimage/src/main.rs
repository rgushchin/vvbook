#![forbid(unsafe_code)]

use std::env;
use std::fs;
use std::path::Path;
use std::process;

const ESP_IMAGE_MAGIC: u8 = 0xE9;
const ESP32_C3_CHIP_ID: u16 = 0x0005;
const PT_LOAD: u32 = 1;

const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// Pure-Rust FIPS 180-4 SHA-256 implementation for ESP32-C3 image validation digests.
pub fn sha256(msg: &[u8]) -> [u8; 32] {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut padded = msg.to_vec();
    let bit_len = (msg.len() as u64) * 8;
    padded.push(0x80);
    while (padded.len() % 64) != 56 {
        padded.push(0x00);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for block in padded.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for (i, dst) in w.iter_mut().enumerate().take(16) {
            let o = i * 4;
            *dst = u32::from_be_bytes([block[o], block[o + 1], block[o + 2], block[o + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA256_K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

fn read_u16_le(bytes: &[u8], off: usize) -> Result<u16, String> {
    let s = bytes
        .get(off..off + 2)
        .ok_or_else(|| "ELF truncated reading u16".to_string())?;
    Ok(u16::from_le_bytes([s[0], s[1]]))
}

fn read_u32_le(bytes: &[u8], off: usize) -> Result<u32, String> {
    let s = bytes
        .get(off..off + 4)
        .ok_or_else(|| "ELF truncated reading u32".to_string())?;
    Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn is_drom_addr(addr: u32) -> bool {
    (0x3C00_0000..0x3C80_0000).contains(&addr)
}

fn is_irom_addr(addr: u32) -> bool {
    (0x4200_0000..0x4280_0000).contains(&addr)
}

fn merge_adjacent_segments(mut segs: Vec<(u32, Vec<u8>)>, max_gap: u32) -> Vec<(u32, Vec<u8>)> {
    segs.sort_by_key(|s| s.0);
    let mut merged: Vec<(u32, Vec<u8>)> = Vec::with_capacity(segs.len());
    for (next_addr, next_data) in segs {
        if let Some((cur_addr, cur_data)) = merged.last_mut() {
            let end_addr = cur_addr.saturating_add(cur_data.len() as u32);
            if (*cur_addr >> 24) == (next_addr >> 24)
                && next_addr >= end_addr
                && next_addr - end_addr <= max_gap
            {
                cur_data.resize((next_addr - *cur_addr) as usize, 0);
                cur_data.extend_from_slice(&next_data);
                while (cur_data.len() % 4) != 0 {
                    cur_data.push(0x00);
                }
                continue;
            }
        }
        merged.push((next_addr, next_data));
    }
    merged
}

fn pad_gap_to_target(cur_off: usize, target_mod: usize) -> usize {
    let hdr_mod = (target_mod + 0x10000 - 8) & 0xFFFF;
    let cur_mod = cur_off & 0xFFFF;
    let mut gap = if hdr_mod >= cur_mod {
        hdr_mod - cur_mod
    } else {
        0x10000 - (cur_mod - hdr_mod)
    };
    if gap > 0 && gap < 8 {
        gap += 0x10000;
    }
    gap
}

/// Converts a RISC-V ELF32 binary (`riscv32imc-unknown-none-elf`) into an ESP32-C3
/// flash/OTA application image (`chip_id = 0x0005`, XOR checksum + trailing SHA-256).
pub fn elf32_to_esp32c3_image(elf: &[u8]) -> Result<(Vec<u8>, u32, usize), String> {
    if elf.len() < 52 || &elf[0..4] != b"\x7fELF" {
        return Err("Not a valid ELF binary".to_string());
    }
    if elf[4] != 1 || elf[5] != 1 {
        return Err("Expected 32-bit little-endian ELF32".to_string());
    }
    let e_machine = read_u16_le(elf, 18)?;
    if e_machine != 0xF3 {
        return Err(format!("Expected RISC-V ELF machine (0xF3), got {e_machine:#x}"));
    }
    let e_entry = read_u32_le(elf, 24)?;
    let e_phoff = read_u32_le(elf, 28)? as usize;
    let e_phentsize = read_u16_le(elf, 42)? as usize;
    let e_phnum = read_u16_le(elf, 44)? as usize;
    if e_phentsize < 32 {
        return Err("Invalid ELF program header size".to_string());
    }

    let mut segments: Vec<(u32, Vec<u8>)> = Vec::new();
    for idx in 0..e_phnum {
        let ph = e_phoff + idx * e_phentsize;
        let p_type = read_u32_le(elf, ph)?;
        if p_type != PT_LOAD {
            continue;
        }
        let p_offset = read_u32_le(elf, ph + 4)? as usize;
        let p_vaddr = read_u32_le(elf, ph + 8)?;
        let p_filesz = read_u32_le(elf, ph + 16)? as usize;
        if p_filesz == 0 {
            continue;
        }
        let payload = elf
            .get(p_offset..p_offset + p_filesz)
            .ok_or_else(|| "Segment extends past ELF end".to_string())?;
        let mut padded = payload.to_vec();
        while (padded.len() % 4) != 0 {
            padded.push(0x00);
        }
        segments.push((p_vaddr, padded));
    }

    let has_flash = segments
        .iter()
        .any(|(addr, _)| is_drom_addr(*addr) || is_irom_addr(*addr));

    let final_segments: Vec<(u32, Vec<u8>)> = if has_flash {
        let mut drom_segs: Vec<(u32, Vec<u8>)> = Vec::new();
        let mut irom_segs: Vec<(u32, Vec<u8>)> = Vec::new();
        let mut ram_segs: Vec<(u32, Vec<u8>)> = Vec::new();

        for seg in segments {
            if is_drom_addr(seg.0) {
                drom_segs.push(seg);
            } else if is_irom_addr(seg.0) {
                irom_segs.push(seg);
            } else {
                ram_segs.push(seg);
            }
        }
        let drom_segs = merge_adjacent_segments(drom_segs, 65_536);
        let irom_segs = merge_adjacent_segments(irom_segs, 65_536);
        let mut ram_segs = merge_adjacent_segments(ram_segs, 256);

        let mut flash_segs = drom_segs;
        flash_segs.extend(irom_segs);

        if let Some((_, payload)) = flash_segs.first_mut()
            && payload.len() >= 256
            && payload[0..4] == 0xABCD_5432u32.to_le_bytes()
            && payload[144..176] == [0u8; 32]
        {
            let elf_digest = sha256(elf);
            payload[144..176].copy_from_slice(&elf_digest);
        }

        let mut ordered = Vec::new();
        let mut cur_off = 24usize;

        for (f_addr, f_data) in flash_segs {
            let target_mod = (f_addr as usize) & 0xFFFF;
            loop {
                let gap = pad_gap_to_target(cur_off, target_mod);
                if let Some(pos) = ram_segs.iter().position(|(_, r_data)| {
                    let need = 8 + r_data.len();
                    need == gap || need + 8 <= gap
                }) {
                    let (r_addr, r_data) = ram_segs.remove(pos);
                    cur_off += 8 + r_data.len();
                    ordered.push((r_addr, r_data));
                } else {
                    break;
                }
            }
            let gap = pad_gap_to_target(cur_off, target_mod);
            if gap >= 8 {
                ordered.push((0x0000_0000, vec![0u8; gap - 8]));
                cur_off += gap;
            }
            assert_eq!((cur_off + 8) & 0xFFFF, target_mod);
            cur_off += 8 + f_data.len();
            ordered.push((f_addr, f_data));
        }

        ordered.extend(ram_segs);
        ordered
    } else {
        segments
    };

    if final_segments.is_empty() || final_segments.len() > 16 {
        return Err(format!(
            "Invalid loadable segment count: {}",
            final_segments.len()
        ));
    }

    // 24-byte ESP32-C3 extended image header
    let mut img = Vec::with_capacity(64 * 1024);
    img.push(ESP_IMAGE_MAGIC);
    img.push(final_segments.len() as u8);
    img.push(2); // DIO
    img.push(if has_flash { 0x4F } else { 0x20 }); // 16MB|80m for flash XIP, 4MB|40m for RAM probe
    img.extend_from_slice(&e_entry.to_le_bytes());
    img.push(0xEE); // wp_pin disabled
    img.extend_from_slice(&[0u8; 3]); // spi_pin_drv
    img.extend_from_slice(&ESP32_C3_CHIP_ID.to_le_bytes());
    img.push(0x00); // min_chip_rev (legacy)
    img.extend_from_slice(&0u16.to_le_bytes()); // min_chip_rev_full = v0.0
    img.extend_from_slice(&0xFFFFu16.to_le_bytes()); // max_chip_rev_full = v655.35
    img.extend_from_slice(&[0u8; 4]); // reserved
    img.push(1); // append_digest = 1

    let mut xor_csum: u8 = 0xEF;
    for (load_addr, data) in &final_segments {
        img.extend_from_slice(&load_addr.to_le_bytes());
        img.extend_from_slice(&(data.len() as u32).to_le_bytes());
        for &b in data {
            xor_csum ^= b;
        }
        img.extend_from_slice(data);
    }

    while (img.len() % 16) != 15 {
        img.push(0x00);
    }
    img.push(xor_csum);

    let digest = sha256(&img);
    img.extend_from_slice(&digest);

    Ok((img, e_entry, final_segments.len()))
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 4 || args[2] != "-o" {
        eprintln!("Usage: vvbook-espimage <input.elf> -o <output.bin>");
        process::exit(2);
    }
    let input = &args[1];
    let output = &args[3];
    let elf = fs::read(input).unwrap_or_else(|e| {
        eprintln!("Failed to read {input}: {e}");
        process::exit(1);
    });
    let (img, entry, segs) = elf32_to_esp32c3_image(&elf).unwrap_or_else(|e| {
        eprintln!("Failed to pack ESP32-C3 image: {e}");
        process::exit(1);
    });
    if let Some(parent) = Path::new(output).parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(output, &img).unwrap_or_else(|e| {
        eprintln!("Failed to write {output}: {e}");
        process::exit(1);
    });
    println!(
        "Packed ESP32-C3 OTA image: entry={entry:#010x}, segments={segs}, size={} bytes -> {output}",
        img.len()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn to_hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn nist_sha256_vectors() {
        assert_eq!(
            to_hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            to_hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            to_hex(&sha256(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn elf32_riscv_to_esp32c3_image_packing() {
        // Minimal valid RISC-V ELF32 header + 1 PT_LOAD segment
        let mut elf = vec![0u8; 256];
        elf[0..4].copy_from_slice(b"\x7fELF");
        elf[4] = 1; // 32-bit
        elf[5] = 1; // little endian
        elf[18..20].copy_from_slice(&(0xF3u16).to_le_bytes()); // RISC-V
        elf[24..28].copy_from_slice(&(0x4200_0020u32).to_le_bytes()); // e_entry
        elf[28..32].copy_from_slice(&(52u32).to_le_bytes()); // e_phoff
        elf[42..44].copy_from_slice(&(32u16).to_le_bytes()); // e_phentsize
        elf[44..46].copy_from_slice(&(1u16).to_le_bytes()); // e_phnum

        // PHDR[0] at offset 52
        elf[52..56].copy_from_slice(&PT_LOAD.to_le_bytes());
        elf[56..60].copy_from_slice(&(128u32).to_le_bytes()); // p_offset
        elf[60..64].copy_from_slice(&(0x4200_0020u32).to_le_bytes()); // p_vaddr
        elf[68..72].copy_from_slice(&(6u32).to_le_bytes()); // p_filesz (unaligned 6 -> padded 8)
        elf[128..134].copy_from_slice(b"VVBOOK");

        let (img, entry, segs) = elf32_to_esp32c3_image(&elf).unwrap();
        assert_eq!(entry, 0x4200_0020);
        assert_eq!(segs, 1);
        assert_eq!(img[0], ESP_IMAGE_MAGIC);
        assert_eq!(img[3], 0x4F);
        assert_eq!(u16::from_le_bytes([img[12], img[13]]), ESP32_C3_CHIP_ID);
        assert_eq!(u16::from_le_bytes([img[17], img[18]]), 0xFFFF);
        assert_eq!(img[23], 1); // append_digest
        assert_eq!(img.len() % 16, 0);
        let body_len = img.len() - 32;
        assert_eq!(sha256(&img[..body_len]), img[body_len..]);
    }

    #[test]
    fn flash_xip_segments_are_64k_mmu_aligned() {
        let mut elf = vec![0u8; 1024];
        elf[0..4].copy_from_slice(b"\x7fELF");
        elf[4] = 1;
        elf[5] = 1;
        elf[18..20].copy_from_slice(&(0xF3u16).to_le_bytes());
        elf[24..28].copy_from_slice(&(0x4038_0400u32).to_le_bytes());
        elf[28..32].copy_from_slice(&(52u32).to_le_bytes());
        elf[42..44].copy_from_slice(&(32u16).to_le_bytes());
        elf[44..46].copy_from_slice(&(3u16).to_le_bytes());

        // PHDR 0: IRAM @ 0x4038_0400 (len 32)
        elf[52..56].copy_from_slice(&PT_LOAD.to_le_bytes());
        elf[56..60].copy_from_slice(&(256u32).to_le_bytes());
        elf[60..64].copy_from_slice(&(0x4038_0400u32).to_le_bytes());
        elf[68..72].copy_from_slice(&(32u32).to_le_bytes());

        // PHDR 1: IROM @ 0x4200_0020 (len 64)
        elf[84..88].copy_from_slice(&PT_LOAD.to_le_bytes());
        elf[88..92].copy_from_slice(&(320u32).to_le_bytes());
        elf[92..96].copy_from_slice(&(0x4200_0020u32).to_le_bytes());
        elf[100..104].copy_from_slice(&(64u32).to_le_bytes());

        // PHDR 2: DROM @ 0x3C20_0020 (len 300) containing ESP_APP_DESC magic
        elf[116..120].copy_from_slice(&PT_LOAD.to_le_bytes());
        elf[120..124].copy_from_slice(&(400u32).to_le_bytes());
        elf[124..128].copy_from_slice(&(0x3C20_0020u32).to_le_bytes());
        elf[132..136].copy_from_slice(&(300u32).to_le_bytes());
        elf[400..404].copy_from_slice(&0xABCD_5432u32.to_le_bytes());

        let (img, entry, segs) = elf32_to_esp32c3_image(&elf).unwrap();
        assert_eq!(entry, 0x4038_0400);
        assert_eq!(segs, 4); // DROM, IRAM, PADDING, IROM
        assert_eq!(img[3], 0x4F);
        assert_eq!(u16::from_le_bytes([img[17], img[18]]), 0xFFFF);

        // Segment 0 at 0x18 must be DROM (0x3C20_0020)
        assert_eq!(read_u32_le(&img, 24).unwrap(), 0x3C20_0020);
        let expected_sha = sha256(&elf);
        assert_eq!(&img[32 + 144..32 + 176], &expected_sha);

        // Walk segments to find IROM and confirm 64 KB MMU page alignment
        let mut off = 24usize;
        let mut found_irom = false;
        for _ in 0..segs {
            let addr = read_u32_le(&img, off).unwrap();
            let len = read_u32_le(&img, off + 4).unwrap() as usize;
            if addr == 0x4200_0020 {
                assert_eq!((off + 8) & 0xFFFF, 0x0020);
                found_irom = true;
            }
            off += 8 + len;
        }
        assert!(found_irom);
    }

    #[test]
    fn merges_multi_segment_esphal_elf_under_segment_limit() {
        // Simulate esp-hal + esp-wifi layout: split DROM (.rodata_desc + .rodata), split IROM, split IRAM (.trap + .rwtext + .rwtext.wifi), split DRAM (.data + .data.wifi)
        let mut elf = vec![0u8; 4096];
        elf[0..4].copy_from_slice(b"\x7fELF");
        elf[4] = 1;
        elf[5] = 1;
        elf[18..20].copy_from_slice(&(0xF3u16).to_le_bytes());
        elf[24..28].copy_from_slice(&(0x4038_0080u32).to_le_bytes());
        elf[28..32].copy_from_slice(&(52u32).to_le_bytes());
        elf[42..44].copy_from_slice(&(32u16).to_le_bytes());
        elf[44..46].copy_from_slice(&(7u16).to_le_bytes());

        let layout = [
            (0x3C00_0020u32, 512usize, 256u32),  // DROM .rodata_desc (256B)
            (0x3C00_0140u32, 800usize, 128u32),  // DROM .rodata (+32B gap -> merge!)
            (0x4038_0080u32, 1000usize, 64u32),  // IRAM .trap
            (0x4038_00E0u32, 1100usize, 128u32), // IRAM .rwtext (+32B gap -> merge!)
            (0x4038_0190u32, 1300usize, 64u32),  // IRAM .rwtext.wifi (+48B gap -> merge!)
            (0x3FC8_0000u32, 1400usize, 64u32),  // DRAM .data
            (0x3FC8_0050u32, 1500usize, 64u32),  // DRAM .data.wifi (+16B gap -> merge!)
            (0x4200_0020u32, 1600usize, 128u32), // IROM .text
            (0x4200_0100u32, 1800usize, 96u32),  // IROM extra (+96B gap -> merge!)
        ];
        // Update e_phnum to 9
        elf[44..46].copy_from_slice(&(layout.len() as u16).to_le_bytes());
        for (i, (vaddr, off, len)) in layout.iter().enumerate() {
            let ph = 52 + i * 32;
            elf[ph..ph + 4].copy_from_slice(&PT_LOAD.to_le_bytes());
            elf[ph + 4..ph + 8].copy_from_slice(&(*off as u32).to_le_bytes());
            elf[ph + 8..ph + 12].copy_from_slice(&vaddr.to_le_bytes());
            elf[ph + 16..ph + 20].copy_from_slice(&len.to_le_bytes());
        }
        // Populate ESP_APP_DESC magic in DROM segment 0 and sentinel bytes in merged parts
        elf[512..516].copy_from_slice(&0xABCD_5432u32.to_le_bytes());
        elf[800..804].copy_from_slice(b"RODA");
        elf[1800..1804].copy_from_slice(b"IROM");

        let (img, _entry, segs) = elf32_to_esp32c3_image(&elf).unwrap();
        assert!(segs <= 8, "expected merged image segments <= 8, got {segs}");
        assert_eq!(read_u32_le(&img, 24).unwrap(), 0x3C00_0020);
        assert_eq!(&img[32 + 144..32 + 176], &sha256(&elf));
        // Confirm .rodata bytes preserved at 0x3C00_0140 - 0x3C00_0020 = 0x120 inside merged DROM segment
        assert_eq!(&img[32 + 0x120..32 + 0x124], b"RODA");
    }
}
