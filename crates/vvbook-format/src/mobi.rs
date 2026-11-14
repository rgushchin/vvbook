extern crate alloc;

use crate::direct::{DIRECT_MAX_SECTION_BLOCKS, DirectTocItem, finalize_section_blocks};
use crate::encoding::{TextEncoding, decode_with};
use crate::parsers::{
    ParseError, VecBlockCollector, XmlChunkFilter, decode_xml_entities, extract_attr,
    has_cyrillic, parse_html, split_oversize_blocks, title_from_filename,
    trim_utf8_complete_prefix,
};
use crate::vvb::{Block, VvbBook};
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use vvbook_pal::{DirEntry, StorageBackend, StorageError};

/// Number of compressed PalmDOC text records decoded per streaming section (~16 KB uncompressed HTML).
pub const MOBI_RECORDS_PER_SECTION: usize = 4;

#[derive(Clone, Debug)]
pub struct MobiHeaderInfo {
    pub title: String,
    pub author: String,
    pub language: String,
    pub source_format: &'static str,
    pub num_records: u16,
    pub text_record_count: u16,
    pub text_length: u32,
    pub mobi_version: u32,
    pub record_size: u16,
    pub flow0_end: u32,
    pub compression: u16,
    pub encryption_type: u16,
    pub extra_flags: u16,
    pub enc: TextEncoding,
}

#[inline]
fn be_u16(b: &[u8], off: usize) -> Option<u16> {
    let s = b.get(off..off + 2)?;
    Some(u16::from_be_bytes([s[0], s[1]]))
}

#[inline]
fn be_u32(b: &[u8], off: usize) -> Option<u32> {
    let s = b.get(off..off + 4)?;
    Some(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

pub fn mobi_source_format(path: &str) -> &'static str {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".azw3") {
        "AZW3"
    } else if lower.ends_with(".azw") {
        "AZW"
    } else if lower.ends_with(".prc") {
        "PRC"
    } else {
        "MOBI"
    }
}

pub fn probe_mobi_header<S: StorageBackend>(
    storage: &S,
    path: &str,
    file_size: u64,
) -> Result<MobiHeaderInfo, ParseError> {
    if file_size < 86 {
        return Err(ParseError::MalformedDocument);
    }
    let mut hdr = [0u8; 94];
    let hdr_want = (file_size as usize).min(hdr.len());
    let hdr_n = storage
        .read_range(path, 0, &mut hdr[..hdr_want])
        .map_err(|_| ParseError::MalformedDocument)?;
    if hdr_n < 86 {
        return Err(ParseError::MalformedDocument);
    }
    let ident = &hdr[60..68];
    if ident != b"BOOKMOBI" && ident != b"TEXtREAd" {
        return Err(ParseError::MalformedDocument);
    }
    let num_records = be_u16(&hdr, 76).ok_or(ParseError::MalformedDocument)?;
    if num_records == 0 {
        return Err(ParseError::MalformedDocument);
    }
    let rec0_off = be_u32(&hdr, 78).ok_or(ParseError::MalformedDocument)? as u64;
    if rec0_off >= file_size {
        return Err(ParseError::MalformedDocument);
    }
    let rec1_off = if num_records > 1 && hdr_n >= 90 {
        be_u32(&hdr, 86).unwrap_or(file_size as u32) as u64
    } else {
        file_size
    };
    let rec0_len = rec1_off
        .saturating_sub(rec0_off)
        .clamp(16, 8192)
        .min(file_size - rec0_off) as usize;
    if rec0_len < 16 {
        return Err(ParseError::MalformedDocument);
    }
    let mut r0 = vec![0u8; rec0_len];
    let r0_n = storage
        .read_range(path, rec0_off, &mut r0)
        .map_err(|_| ParseError::MalformedDocument)?;
    r0.truncate(r0_n);
    if r0.len() < 16 {
        return Err(ParseError::MalformedDocument);
    }

    let compression = be_u16(&r0, 0).ok_or(ParseError::MalformedDocument)?;
    let text_length = be_u32(&r0, 4).ok_or(ParseError::MalformedDocument)?;
    let mut text_record_count = be_u16(&r0, 8)
        .ok_or(ParseError::MalformedDocument)?
        .min(num_records.saturating_sub(1));
    let record_size = be_u16(&r0, 10).unwrap_or(4096).max(1);
    let encryption_type = be_u16(&r0, 12).ok_or(ParseError::MalformedDocument)?;

    let mut enc = TextEncoding::Utf8;
    let mut title = title_from_filename(path);
    if title == "Untitled" || title.is_empty() {
        let raw_pdb_name = hdr[..32].split(|&b| b == 0).next().unwrap_or(&[]);
        let s = decode_with(raw_pdb_name, TextEncoding::Utf8).trim().to_string();
        if !s.is_empty() {
            title = s;
        }
    }
    let mut author = String::from("Unknown Author");
    let mut language = String::from("en");
    let mut extra_flags = 0u16;
    let mut mobi_version = 0u32;
    let mut flow0_end = text_length;

    if r0.len() >= 24 && &r0[16..20] == b"MOBI" {
        let mobi_hdr_len = be_u32(&r0, 20).unwrap_or(0) as usize;
        let codepage = be_u32(&r0, 28).unwrap_or(65001);
        mobi_version = be_u32(&r0, 36).unwrap_or(0);
        enc = match codepage {
            1251 => TextEncoding::Windows1251,
            1252 => TextEncoding::Iso8859_1,
            _ => TextEncoding::Utf8,
        };
        if let (Some(fn_off), Some(fn_len)) = (be_u32(&r0, 84), be_u32(&r0, 88)) {
            let off = fn_off as usize;
            let len = fn_len as usize;
            if len > 0 && len <= 512 && off.saturating_add(len) <= r0.len() {
                let candidate = decode_with(&r0[off..off + len], enc)
                    .replace('\0', "")
                    .trim()
                    .to_string();
                if !candidate.is_empty() {
                    title = candidate;
                }
            }
        }
        // MOBI Extra Record Data Flags at offset 240 is a big-endian uint32 (low bits at 242..244).
        if mobi_hdr_len >= 228
            && let Some(flags32) = be_u32(&r0, 240)
        {
            extra_flags = (flags32 & 0xFFFF) as u16;
        }
        if let Some(exth_flags) = be_u32(&r0, 128)
            && (exth_flags & 0x40) != 0
        {
            let exth_off = 16usize.saturating_add(mobi_hdr_len);
            if exth_off.saturating_add(12) <= r0.len()
                && &r0[exth_off..exth_off + 4] == b"EXTH"
                && let Some(exth_count) = be_u32(&r0, exth_off + 8)
            {
                let mut pos = exth_off + 12;
                for _ in 0..exth_count.min(128) {
                    let Some(rtype) = be_u32(&r0, pos) else { break };
                    let Some(rlen_u32) = be_u32(&r0, pos + 4) else { break };
                    let rlen = rlen_u32 as usize;
                    if rlen < 8 || pos.saturating_add(rlen) > r0.len() {
                        break;
                    }
                    let val = &r0[pos + 8..pos + rlen];
                    match rtype {
                        100 => {
                            let a = decode_with(val, enc).replace('\0', "").trim().to_string();
                            if !a.is_empty() {
                                author = a;
                            }
                        }
                        503 => {
                            let t = decode_with(val, enc).replace('\0', "").trim().to_string();
                            if !t.is_empty() {
                                title = t;
                            }
                        }
                        524 => {
                            let l = decode_with(val, enc).trim().to_ascii_lowercase();
                            if l.starts_with("ru") {
                                language = String::from("ru");
                            }
                        }
                        _ => {}
                    }
                    pos += rlen;
                }
            }
        }
        // KF8 (mobi_version >= 8) FDST flow table: clamp to Flow 0 (XHTML) so trailing CSS flows never leak.
        if encryption_type == 0
            && matches!(compression, 1 | 2)
            && mobi_version >= 8
            && mobi_hdr_len >= 184
            && r0.len() >= 200
            && let (Some(fdst_rec), Some(fdst_cnt)) = (be_u32(&r0, 192), be_u32(&r0, 196))
            && fdst_rec != 0xFFFF_FFFF
            && fdst_rec > 0
            && (fdst_rec as usize) < num_records as usize
            && fdst_cnt >= 1
        {
            let tbl_off = 78u64 + (fdst_rec as u64) * 8;
            let has_next = (fdst_rec as usize) + 1 < num_records as usize;
            let mut entry_buf = [0u8; 16];
            let want_tbl = if has_next { 16 } else { 8 };
            if storage.read_range(path, tbl_off, &mut entry_buf[..want_tbl]).ok() == Some(want_tbl) {
                let fdst_start = be_u32(&entry_buf, 0).unwrap_or(0) as u64;
                let fdst_end = if has_next {
                    (be_u32(&entry_buf, 8).unwrap_or(0) as u64).min(file_size)
                } else {
                    file_size
                };
                if fdst_end > fdst_start {
                    let fdst_len = (fdst_end - fdst_start).min(64) as usize;
                    let mut fdst_buf = [0u8; 64];
                    if fdst_len >= 20
                        && storage
                            .read_range(path, fdst_start, &mut fdst_buf[..fdst_len])
                            .ok()
                            .is_some_and(|n| n >= 20)
                        && &fdst_buf[0..4] == b"FDST"
                        && let Some(sec_off_u32) = be_u32(&fdst_buf, 4)
                    {
                        let sec_off = sec_off_u32 as usize;
                        if sec_off >= 12
                            && sec_off + 8 <= fdst_len
                            && let (Some(flow0_start), Some(end0)) =
                                (be_u32(&fdst_buf, sec_off), be_u32(&fdst_buf, sec_off + 4))
                            && flow0_start == 0
                            && end0 > 0
                            && end0 <= text_length
                        {
                            flow0_end = end0;
                            let max_recs = flow0_end.div_ceil(record_size as u32);
                            text_record_count =
                                (text_record_count as u32).min(max_recs) as u16;
                        }
                    }
                }
            }
        }
    }

    if has_cyrillic(&title) || has_cyrillic(&author) {
        language = String::from("ru");
    }

    Ok(MobiHeaderInfo {
        title,
        author,
        language,
        source_format: mobi_source_format(path),
        num_records,
        text_record_count,
        text_length,
        mobi_version,
        record_size,
        flow0_end,
        compression,
        encryption_type,
        extra_flags,
        enc,
    })
}

fn trim_mobi_trailing_bytes(mut raw: &[u8], extra_flags: u16) -> &[u8] {
    let mut flags = extra_flags >> 1;
    while flags != 0 {
        if (flags & 1) != 0 && !raw.is_empty() {
            let mut n = 0usize;
            let mut shift = 0u32;
            let mut pos = raw.len();
            while pos > 0 && shift < 28 {
                pos -= 1;
                let b = raw[pos];
                n |= ((b & 0x7F) as usize) << shift;
                shift += 7;
                if (b & 0x80) != 0 {
                    break;
                }
            }
            if n > 0 && n <= raw.len() {
                raw = &raw[..raw.len() - n];
            }
        }
        flags >>= 1;
    }
    if (extra_flags & 1) != 0 && !raw.is_empty() {
        let overlap = ((raw[raw.len() - 1] & 0x03) + 1) as usize;
        if overlap <= raw.len() {
            raw = &raw[..raw.len() - overlap];
        }
    }
    raw
}

fn palmdoc_lz77_decompress(src: &[u8], out: &mut Vec<u8>, max_out: usize) {
    let mut i = 0usize;
    let start_len = out.len();
    while i < src.len() && out.len().saturating_sub(start_len) < max_out {
        let c = src[i];
        i += 1;
        if c == 0 {
            out.push(0);
        } else if c <= 8 {
            let take = (c as usize).min(src.len() - i);
            let avail = max_out.saturating_sub(out.len().saturating_sub(start_len));
            let copy = take.min(avail);
            out.extend_from_slice(&src[i..i + copy]);
            i += take;
        } else if c <= 0x7F {
            out.push(c);
        } else if c >= 0xC0 {
            out.push(b' ');
            if out.len().saturating_sub(start_len) < max_out {
                out.push(c ^ 0x80);
            }
        } else {
            if i >= src.len() {
                break;
            }
            let c2 = src[i];
            i += 1;
            let pair = (((c & 0x3F) as usize) << 8) | (c2 as usize);
            let dist = pair >> 3;
            let len = (pair & 0x07) + 3;
            if dist == 0 || dist > out.len() {
                continue;
            }
            for _ in 0..len {
                if out.len().saturating_sub(start_len) >= max_out {
                    break;
                }
                let byte = out[out.len() - dist];
                out.push(byte);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn read_mobi_text_record<S: StorageBackend>(
    storage: &S,
    path: &str,
    file_size: u64,
    num_records: u16,
    rec_idx: usize,
    compression: u16,
    extra_flags: u16,
    out: &mut Vec<u8>,
) -> Result<(), ParseError> {
    if rec_idx >= num_records as usize {
        return Err(ParseError::MalformedDocument);
    }
    let tbl_off = 78u64 + (rec_idx as u64) * 8;
    let has_next = rec_idx + 1 < num_records as usize;
    let mut entry_buf = [0u8; 16];
    let want_tbl = if has_next { 16 } else { 8 };
    let n_tbl = storage
        .read_range(path, tbl_off, &mut entry_buf[..want_tbl])
        .map_err(|_| ParseError::MalformedDocument)?;
    if n_tbl < want_tbl {
        return Err(ParseError::MalformedDocument);
    }
    let rec_start = be_u32(&entry_buf, 0).ok_or(ParseError::MalformedDocument)? as u64;
    let rec_end = if has_next {
        (be_u32(&entry_buf, 8).ok_or(ParseError::MalformedDocument)? as u64).min(file_size)
    } else {
        file_size
    };
    if rec_end <= rec_start {
        return Ok(());
    }
    let raw_len = (rec_end - rec_start) as usize;
    if raw_len > 65_536 {
        return Err(ParseError::MalformedDocument);
    }
    if raw_len <= 4608 {
        let mut stack_buf = [0u8; 4608];
        let n = storage
            .read_range(path, rec_start, &mut stack_buf[..raw_len])
            .map_err(|_| ParseError::MalformedDocument)?;
        let trimmed = trim_mobi_trailing_bytes(&stack_buf[..n], extra_flags);
        if compression == 1 {
            out.extend_from_slice(trimmed);
        } else {
            palmdoc_lz77_decompress(trimmed, out, 16_384);
        }
    } else {
        let mut heap_buf = vec![0u8; raw_len];
        let n = storage
            .read_range(path, rec_start, &mut heap_buf)
            .map_err(|_| ParseError::MalformedDocument)?;
        let trimmed = trim_mobi_trailing_bytes(&heap_buf[..n], extra_flags);
        if compression == 1 {
            out.extend_from_slice(trimmed);
        } else {
            palmdoc_lz77_decompress(trimmed, out, 16_384);
        }
    }
    Ok(())
}

fn mobi_record_seam_offset(rec: &[u8]) -> usize {
    if rec.is_empty() {
        return 0;
    }
    let win = &rec[..rec.len().min(512)];
    let first_lt = win.iter().position(|&b| b == b'<');
    let first_gt = win.iter().position(|&b| b == b'>');
    let scan_from = match (first_gt, first_lt) {
        (Some(gt), Some(lt)) if gt < lt => gt + 1,
        (Some(gt), None) => return gt + 1,
        _ => 0,
    };
    if let Some(rel_lt) = win[scan_from..].iter().position(|&b| b == b'<') {
        let lt = scan_from + rel_lt;
        if win[lt..].starts_with(b"</")
            && let Some(rel_gt) = win[lt..].iter().position(|&b| b == b'>')
        {
            return lt + rel_gt + 1;
        }
        return lt;
    }
    // Never split a multi-byte UTF-8 sequence across records.
    let mut safe = 0usize;
    while safe < 4 && safe < rec.len() && (rec[safe] & 0xC0) == 0x80 {
        safe += 1;
    }
    safe
}

fn find_ascii_ci(haystack: &str, needle_lower: &[u8]) -> Option<usize> {
    if needle_lower.is_empty() {
        return Some(0);
    }
    haystack
        .as_bytes()
        .windows(needle_lower.len())
        .position(|w| w.eq_ignore_ascii_case(needle_lower))
}

pub fn extract_mobi_toc<S: StorageBackend>(
    storage: &S,
    path: &str,
    file_size: u64,
    hdr: &MobiHeaderInfo,
    section_count: usize,
) -> Vec<DirectTocItem> {
    if hdr.encryption_type != 0
        || !matches!(hdr.compression, 1 | 2)
        || hdr.text_record_count == 0
        || hdr.flow0_end == 0
    {
        return Vec::new();
    }
    let probe_recs = (hdr.text_record_count as usize).min(6);
    let max_probe_bytes = (hdr.flow0_end as usize).min(24 * 1024);
    let sec_bytes = (MOBI_RECORDS_PER_SECTION * 4096).max(1);

    let mut rec_buf = Vec::with_capacity(4352);
    let mut utf8_carry = [0u8; 4];
    let mut carry_len = 0usize;
    let mut window = String::with_capacity(5120);
    let mut toc = Vec::new();
    let mut total_probed = 0usize;

    for r in 1..=probe_recs {
        if toc.len() >= 64 || total_probed >= max_probe_bytes {
            break;
        }
        rec_buf.clear();
        if carry_len > 0 {
            rec_buf.extend_from_slice(&utf8_carry[..carry_len]);
            carry_len = 0;
        }
        let prefix = rec_buf.len();
        if read_mobi_text_record(
            storage,
            path,
            file_size,
            hdr.num_records,
            r,
            hdr.compression,
            hdr.extra_flags,
            &mut rec_buf,
        )
        .is_err()
        {
            break;
        }
        let new_bytes = rec_buf.len().saturating_sub(prefix);
        let rem_flow = max_probe_bytes.saturating_sub(total_probed);
        if new_bytes > rem_flow {
            rec_buf.truncate(prefix + rem_flow);
        }
        total_probed += rec_buf.len().saturating_sub(prefix);
        if rec_buf.is_empty() {
            continue;
        }

        if hdr.enc == TextEncoding::Utf8 {
            let is_last = r == probe_recs || total_probed >= max_probe_bytes;
            let usable = if is_last {
                rec_buf.len()
            } else {
                trim_utf8_complete_prefix(&rec_buf)
            };
            if usable < rec_buf.len() {
                let rem = &rec_buf[usable..];
                let n = rem.len().min(4);
                utf8_carry[..n].copy_from_slice(&rem[..n]);
                carry_len = n;
            }
            if let Ok(valid) = core::str::from_utf8(&rec_buf[..usable]) {
                window.push_str(valid);
            } else {
                let mut slice = &rec_buf[..usable];
                while !slice.is_empty() {
                    match core::str::from_utf8(slice) {
                        Ok(valid) => {
                            window.push_str(valid);
                            break;
                        }
                        Err(e) => {
                            let ok_len = e.valid_up_to();
                            if ok_len > 0
                                && let Ok(valid) = core::str::from_utf8(&slice[..ok_len])
                            {
                                window.push_str(valid);
                            }
                            window.push('\u{FFFD}');
                            let skip = ok_len + e.error_len().unwrap_or(1).max(1);
                            slice = &slice[skip.min(slice.len())..];
                        }
                    }
                }
            }
        } else {
            let decoded = decode_with(&rec_buf, hdr.enc);
            window.push_str(&decoded);
        }

        let mut pos = 0usize;
        while toc.len() < 64
            && let Some(rel) = find_ascii_ci(&window[pos..], b"<a")
        {
            let start = pos + rel;
            if start + 2 >= window.len() {
                break;
            }
            let next_b = window.as_bytes()[start + 2];
            if !matches!(next_b, b' ' | b'\t' | b'\r' | b'\n') {
                pos = start + 2;
                continue;
            }
            let Some(gt_rel) = window[start..].find('>') else {
                break;
            };
            let body_start = start + gt_rel + 1;
            let Some(end_rel) = find_ascii_ci(&window[body_start..], b"</a>") else {
                break;
            };
            let tag = &window[start..body_start];
            let body = &window[body_start..body_start + end_rel];
            pos = body_start + end_rel + 4;

            let Some(fp_raw) = extract_attr(tag, "filepos") else {
                continue;
            };
            let digits: String = fp_raw
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            let Ok(filepos) = digits.parse::<usize>() else {
                continue;
            };
            let label = strip_inline_tags(body);
            if label.is_empty() {
                continue;
            }
            let sec_idx = (filepos / sec_bytes).min(section_count.saturating_sub(1));
            if toc
                .last()
                .is_some_and(|prev: &DirectTocItem| prev.title == label && prev.section_idx == sec_idx)
            {
                continue;
            }
            toc.push(DirectTocItem {
                title: label,
                section_idx: sec_idx,
                block_in_section: 0,
            });
        }

        // Keep only any trailing incomplete `<a...` tail (capped at <= 512 B) across record boundaries.
        let keep_from = if let Some(rel) = find_ascii_ci(&window[pos..], b"<a") {
            pos + rel
        } else if window.ends_with('<') {
            window.len() - 1
        } else {
            window.len()
        };
        let min_keep = if window.len() > 512 {
            let mut idx = window.len() - 512;
            while idx < window.len() && !window.is_char_boundary(idx) {
                idx += 1;
            }
            idx
        } else {
            0
        };
        let drain_to = keep_from.max(min_keep).min(window.len());
        if drain_to > 0 {
            window.drain(..drain_to);
        }
    }
    toc
}

fn strip_inline_tags(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for ch in s.chars() {
        if ch == '<' {
            in_tag = true;
        } else if ch == '>' {
            in_tag = false;
        } else if !in_tag {
            out.push(ch);
        }
    }
    let decoded = decode_xml_entities(&out);
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[allow(clippy::too_many_arguments)]
pub fn load_mobi_section<S: StorageBackend>(
    storage: &S,
    path: &str,
    file_size: u64,
    num_records: u16,
    text_record_count: u16,
    record_size: u16,
    flow0_end: u32,
    compression: u16,
    encryption_type: u16,
    extra_flags: u16,
    enc: TextEncoding,
    section_idx: usize,
) -> Result<Vec<Block>, ParseError> {
    if encryption_type != 0
        || !matches!(compression, 1 | 2)
        || text_record_count == 0
        || flow0_end == 0
    {
        return Err(ParseError::UnsupportedFormat);
    }

    let sec_count = ((text_record_count as usize).div_ceil(MOBI_RECORDS_PER_SECTION)).max(1);
    if section_idx >= sec_count {
        return Err(ParseError::MalformedDocument);
    }

    let rec_start = 1 + section_idx * MOBI_RECORDS_PER_SECTION;
    let rec_end = (rec_start + MOBI_RECORDS_PER_SECTION).min(text_record_count as usize + 1);
    let rec_unit = record_size.max(1) as usize;

    let mut filter = XmlChunkFilter::new(false);
    filter.set_encoding(enc);
    if section_idx > 0 {
        filter.set_seen_body(true);
    }
    let mut collector = VecBlockCollector::new(DIRECT_MAX_SECTION_BLOCKS);
    let mut rec_buf = Vec::with_capacity(4352);

    for r in rec_start..rec_end {
        if filter.done || collector.blocks.len() >= DIRECT_MAX_SECTION_BLOCKS {
            break;
        }
        rec_buf.clear();
        read_mobi_text_record(
            storage,
            path,
            file_size,
            num_records,
            r,
            compression,
            extra_flags,
            &mut rec_buf,
        )?;
        let rec_byte_base = (r - 1).saturating_mul(rec_unit);
        let max_in_flow0 = (flow0_end as usize).saturating_sub(rec_byte_base);
        if rec_buf.len() > max_in_flow0 {
            rec_buf.truncate(max_in_flow0);
        }
        if rec_buf.is_empty() {
            continue;
        }
        let slice_from = if r == rec_start && section_idx > 0 {
            mobi_record_seam_offset(&rec_buf)
        } else {
            0
        };
        if slice_from < rec_buf.len() {
            filter.feed_bytes(&rec_buf[slice_from..], &mut collector)?;
        }
    }

    if rec_end <= text_record_count as usize
        && !filter.done
        && collector.blocks.len() < DIRECT_MAX_SECTION_BLOCKS
    {
        rec_buf.clear();
        if read_mobi_text_record(
            storage,
            path,
            file_size,
            num_records,
            rec_end,
            compression,
            extra_flags,
            &mut rec_buf,
        )
        .is_ok()
        {
            let rec_byte_base = (rec_end - 1).saturating_mul(rec_unit);
            let max_in_flow0 = (flow0_end as usize).saturating_sub(rec_byte_base);
            if rec_buf.len() > max_in_flow0 {
                rec_buf.truncate(max_in_flow0);
            }
            if !rec_buf.is_empty() {
                let seam = mobi_record_seam_offset(&rec_buf).min(rec_buf.len());
                if seam > 0 {
                    filter.feed_bytes(&rec_buf[..seam], &mut collector)?;
                }
            }
        }
    }

    filter.finish(&mut collector)?;
    finalize_section_blocks(&mut collector.blocks);
    Ok(collector.blocks)
}

struct SliceStorage<'a>(&'a [u8]);

impl<'a> StorageBackend for SliceStorage<'a> {
    fn exists(&self, _path: &str) -> bool {
        true
    }
    fn file_len(&self, _path: &str) -> Result<u64, StorageError> {
        Ok(self.0.len() as u64)
    }
    fn read_all(&self, _path: &str) -> Result<Vec<u8>, StorageError> {
        Ok(self.0.to_vec())
    }
    fn read_range(&self, _path: &str, offset: u64, buf: &mut [u8]) -> Result<usize, StorageError> {
        let off = offset as usize;
        if off >= self.0.len() {
            return Ok(0);
        }
        let n = buf.len().min(self.0.len() - off);
        buf[..n].copy_from_slice(&self.0[off..off + n]);
        Ok(n)
    }
    fn write_all(&mut self, _path: &str, _data: &[u8]) -> Result<(), StorageError> {
        Err(StorageError::Io)
    }
    fn write_atomic(&mut self, _path: &str, _data: &[u8]) -> Result<(), StorageError> {
        Err(StorageError::Io)
    }
    fn append(&mut self, _path: &str, _data: &[u8]) -> Result<(), StorageError> {
        Err(StorageError::Io)
    }
    fn remove(&mut self, _path: &str) -> Result<(), StorageError> {
        Err(StorageError::Io)
    }
    fn mkdir_all(&mut self, _path: &str) -> Result<(), StorageError> {
        Err(StorageError::Io)
    }
    fn list_dir(&self, _path: &str) -> Result<Vec<DirEntry>, StorageError> {
        Ok(Vec::new())
    }
}

pub fn parse_mobi_in_memory(raw: &[u8], filename: &str) -> Result<VvbBook, ParseError> {
    let store = SliceStorage(raw);
    let file_size = raw.len() as u64;
    let hdr = probe_mobi_header(&store, filename, file_size)?;
    if hdr.encryption_type != 0
        || !matches!(hdr.compression, 1 | 2)
        || hdr.text_record_count == 0
        || hdr.flow0_end == 0
    {
        return Err(ParseError::UnsupportedFormat);
    }

    let max_bytes = (hdr.flow0_end as usize).min(2 * 1024 * 1024);
    let mut html_bytes = Vec::with_capacity(max_bytes.min(256 * 1024));
    for r in 1..=(hdr.text_record_count as usize) {
        read_mobi_text_record(
            &store,
            filename,
            file_size,
            hdr.num_records,
            r,
            hdr.compression,
            hdr.extra_flags,
            &mut html_bytes,
        )?;
        if html_bytes.len() >= max_bytes {
            html_bytes.truncate(max_bytes);
            break;
        }
    }
    let html = decode_with(&html_bytes, hdr.enc);
    drop(html_bytes);
    let mut book = parse_html(&html, filename);
    drop(html);
    if book.blocks.is_empty() {
        return Err(ParseError::EmptyDocument);
    }
    split_oversize_blocks(&mut book.blocks);
    book.blocks.truncate(4096);
    book.meta.title = hdr.title;
    book.meta.author = hdr.author;
    book.meta.language = hdr.language;
    book.meta.source_format = String::from(hdr.source_format);
    Ok(book)
}
