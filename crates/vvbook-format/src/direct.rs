extern crate alloc;

use crate::encoding::{TextEncoding, decode_with, detect_and_decode_to_utf8, detect_encoding};
use crate::parsers::{
    BlockConsumer, InputFormat, ParseError, VecBlockCollector, XmlChunkFilter, decode_xml_entities,
    extract_attr, extract_xml_tag_text, fb2_meta_from_head, floor_char_boundary, has_cyrillic,
    is_spine_text_href, normalize_zip_href, opf_tag_local_name, parse_epub_opf_dc_meta,
    parse_markdown, parse_plain_text, percent_decode_href, read_storage_prefix, source_book_id_hex,
    split_oversize_blocks, title_from_filename, trim_utf8_complete_prefix,
};
use crate::vvb::{Block, BookMetadata, COVER_THUMB_H, COVER_THUMB_W};
use crate::zip::{StorageZip, ZipEntryMeta, ZipEntryRef, ZipInflateScratch};
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use vvbook_pal::StorageBackend;

/// Maximum logical blocks materialized into heap for a single direct-stream section.
pub const DIRECT_MAX_SECTION_BLOCKS: usize = 120;

const BYTE_SLICE_TARGET: u64 = 16 * 1024;
const EPUB_SLICE_TARGET: u64 = 24 * 1024;
const MAX_EPUB_TOC_ENTRIES: usize = 64;
const MAX_SYNTH_TOC_ENTRIES: usize = 64;

#[derive(Clone, Copy, Debug)]
struct EpubSliceRef {
    comp_size: u32,
    uncomp_size: u32,
    local_header_offset: u32,
    method: u16,
    part_idx: u16,
}

impl EpubSliceRef {
    fn from_entry(e: ZipEntryRef, part_idx: u16) -> Self {
        Self {
            comp_size: e.comp_size,
            uncomp_size: e.uncomp_size,
            local_header_offset: e.local_header_offset,
            method: e.method,
            part_idx,
        }
    }

    fn entry_ref(self) -> ZipEntryRef {
        ZipEntryRef {
            method: self.method,
            comp_size: self.comp_size,
            uncomp_size: self.uncomp_size,
            local_header_offset: self.local_header_offset,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectTocItem {
    pub title: String,
    pub section_idx: usize,
    pub block_in_section: usize,
}

#[derive(Clone, Debug)]
enum DirectSource {
    Epub {
        file_size: u64,
        chapters: Vec<EpubSliceRef>,
    },
    Fb2Zip {
        file_size: u64,
        entry: ZipEntryRef,
        enc: TextEncoding,
        section_count: usize,
    },
    Mobi {
        file_size: u64,
        num_records: u16,
        text_record_count: u16,
        record_size: u16,
        flow0_end: u32,
        compression: u16,
        encryption_type: u16,
        extra_flags: u16,
        enc: TextEncoding,
        section_count: usize,
    },
    ByteSliced {
        fmt: InputFormat,
        enc: TextEncoding,
        body_start: u64,
        file_size: u64,
        slice_target: u64,
        section_count: usize,
    },
}

/// Zero-write direct streaming reader for large (`> 96 KB`) books.
/// Keeps only compact section offsets / `ZipEntryRef`s (`~16 B`/chapter) in RAM and
/// decodes one ~32 KB seam-aligned section or EPUB spine chapter on demand.
#[derive(Clone, Debug)]
pub struct DirectBook {
    pub path: String,
    pub meta: BookMetadata,
    pub toc: Vec<DirectTocItem>,
    source: DirectSource,
    est_total_words: u32,
}

impl DirectBook {
    pub fn open<S: StorageBackend>(
        storage: &S,
        path: &str,
        file_size: u64,
        forced_id_hex: Option<&str>,
    ) -> Result<Self, ParseError> {
        let fmt = InputFormat::from_filename(path).ok_or(ParseError::UnsupportedFormat)?;
        if file_size == 0 {
            return Err(ParseError::EmptyDocument);
        }
        let id_hex = forced_id_hex
            .map(String::from)
            .unwrap_or_else(|| source_book_id_hex(path, file_size));
        match fmt {
            InputFormat::Epub => {
                let mut scratch = ZipInflateScratch::new();
                open_direct_epub(storage, path, file_size, id_hex, &mut scratch)
            }
            InputFormat::Fb2Zip => open_direct_fb2_zip(storage, path, file_size, id_hex, None),
            InputFormat::Mobi => open_direct_mobi(storage, path, file_size, id_hex),
            InputFormat::Fb2 | InputFormat::Html | InputFormat::Markdown | InputFormat::Txt => {
                open_direct_byte_sliced(storage, path, file_size, fmt, id_hex)
            }
            InputFormat::Vvb => Err(ParseError::UnsupportedFormat),
        }
    }

    pub fn open_with_scratch<S: StorageBackend>(
        storage: &S,
        path: &str,
        file_size: u64,
        forced_id_hex: Option<&str>,
        scratch: &mut ZipInflateScratch,
    ) -> Result<Self, ParseError> {
        let fmt = InputFormat::from_filename(path).ok_or(ParseError::UnsupportedFormat)?;
        if file_size == 0 {
            return Err(ParseError::EmptyDocument);
        }
        let id_hex = forced_id_hex
            .map(String::from)
            .unwrap_or_else(|| source_book_id_hex(path, file_size));

        match fmt {
            InputFormat::Epub => open_direct_epub(storage, path, file_size, id_hex, scratch),
            InputFormat::Fb2Zip => {
                open_direct_fb2_zip(storage, path, file_size, id_hex, Some(scratch))
            }
            InputFormat::Mobi => open_direct_mobi(storage, path, file_size, id_hex),
            InputFormat::Fb2 | InputFormat::Html | InputFormat::Markdown | InputFormat::Txt => {
                open_direct_byte_sliced(storage, path, file_size, fmt, id_hex)
            }
            InputFormat::Vvb => Err(ParseError::UnsupportedFormat),
        }
    }

    pub fn section_count(&self) -> usize {
        match &self.source {
            DirectSource::Epub { chapters, .. } => chapters.len().max(1),
            DirectSource::Fb2Zip { section_count, .. }
            | DirectSource::Mobi { section_count, .. }
            | DirectSource::ByteSliced { section_count, .. } => (*section_count).max(1),
        }
    }

    pub fn estimated_word_count(&self) -> u32 {
        self.est_total_words
    }

    pub fn load_section<S: StorageBackend>(
        &self,
        storage: &S,
        section_idx: usize,
    ) -> Result<Vec<Block>, ParseError> {
        match &self.source {
            DirectSource::Epub { .. } | DirectSource::Fb2Zip { .. } => {
                let mut scratch = ZipInflateScratch::new();
                self.load_section_with_scratch(storage, section_idx, &mut scratch)
            }
            DirectSource::Mobi {
                file_size,
                num_records,
                text_record_count,
                record_size,
                flow0_end,
                compression,
                encryption_type,
                extra_flags,
                enc,
                ..
            } => crate::mobi::load_mobi_section(
                storage,
                &self.path,
                *file_size,
                *num_records,
                *text_record_count,
                *record_size,
                *flow0_end,
                *compression,
                *encryption_type,
                *extra_flags,
                *enc,
                section_idx,
            ),
            DirectSource::ByteSliced {
                fmt,
                enc,
                body_start,
                ..
            } => {
                let (start, end) =
                    resolve_byte_slice_range(storage, &self.path, &self.source, section_idx)?;
                load_byte_slice_section(
                    storage,
                    &self.path,
                    *fmt,
                    *enc,
                    start,
                    end,
                    section_idx > 0 || *body_start > 0,
                )
            }
        }
    }

    pub fn load_section_with_scratch<S: StorageBackend>(
        &self,
        storage: &S,
        section_idx: usize,
        scratch: &mut ZipInflateScratch,
    ) -> Result<Vec<Block>, ParseError> {
        match &self.source {
            DirectSource::Epub {
                file_size,
                chapters,
            } => {
                let slice_ref = *chapters
                    .get(section_idx)
                    .ok_or(ParseError::MalformedDocument)?;
                let entry_ref = slice_ref.entry_ref();
                let part_idx = slice_ref.part_idx as usize;
                stream_zip_xml_section(
                    storage,
                    &self.path,
                    *file_size,
                    entry_ref,
                    false,
                    TextEncoding::Utf8,
                    ZipSliceMode::BlockSlice(part_idx),
                    scratch,
                )
            }
            DirectSource::Fb2Zip {
                file_size,
                entry,
                enc,
                section_count,
            } => {
                if section_idx >= (*section_count).max(1) {
                    return Err(ParseError::MalformedDocument);
                }
                stream_zip_xml_section(
                    storage,
                    &self.path,
                    *file_size,
                    *entry,
                    true,
                    *enc,
                    ZipSliceMode::ByteWindow {
                        idx: section_idx,
                        target: BYTE_SLICE_TARGET,
                    },
                    scratch,
                )
            }
            DirectSource::Mobi {
                file_size,
                num_records,
                text_record_count,
                record_size,
                flow0_end,
                compression,
                encryption_type,
                extra_flags,
                enc,
                ..
            } => crate::mobi::load_mobi_section(
                storage,
                &self.path,
                *file_size,
                *num_records,
                *text_record_count,
                *record_size,
                *flow0_end,
                *compression,
                *encryption_type,
                *extra_flags,
                *enc,
                section_idx,
            ),
            DirectSource::ByteSliced {
                fmt,
                enc,
                body_start,
                ..
            } => {
                let (start, end) =
                    resolve_byte_slice_range(storage, &self.path, &self.source, section_idx)?;
                load_byte_slice_section(
                    storage,
                    &self.path,
                    *fmt,
                    *enc,
                    start,
                    end,
                    section_idx > 0 || *body_start > 0,
                )
            }
        }
    }
}

pub(crate) fn finalize_section_blocks(blocks: &mut [Block]) {
    let mut after_chapter = false;
    for b in blocks.iter_mut() {
        match b {
            Block::ChapterTitle { .. } => after_chapter = true,
            Block::Paragraph {
                first_of_chapter,
                is_dialogue,
                ..
            } => {
                if after_chapter && !*is_dialogue {
                    *first_of_chapter = true;
                    after_chapter = false;
                }
            }
            Block::VerseLine { .. } => after_chapter = false,
            _ => {}
        }
    }
}

#[derive(Clone, Copy)]
enum ZipSliceMode {
    BlockSlice(usize),
    ByteWindow { idx: usize, target: u64 },
}

#[allow(clippy::too_many_arguments)]
fn stream_zip_xml_section<S: StorageBackend>(
    storage: &S,
    path: &str,
    file_size: u64,
    entry_ref: ZipEntryRef,
    fb2_mode: bool,
    enc: TextEncoding,
    mode: ZipSliceMode,
    scratch: &mut ZipInflateScratch,
) -> Result<Vec<Block>, ParseError> {
    scratch.reset();
    let mut stream = StorageZip::open_entry_ref_stream(
        file_size,
        storage,
        path,
        entry_ref,
        128 * 1024 * 1024,
        Some(scratch),
    )
    .map_err(|_| ParseError::InvalidArchive)?;
    let mut filter = XmlChunkFilter::new(fb2_mode);
    if fb2_mode {
        filter.set_encoding(enc);
    }
    let mut collector = match mode {
        ZipSliceMode::BlockSlice(part_idx) => {
            VecBlockCollector::with_slice(DIRECT_MAX_SECTION_BLOCKS, part_idx)
        }
        ZipSliceMode::ByteWindow { idx, .. } => {
            if idx > 0 {
                filter.set_seen_body(true);
            }
            VecBlockCollector::new(DIRECT_MAX_SECTION_BLOCKS)
        }
    };
    let mut buf = [0u8; 2048];

    match mode {
        ZipSliceMode::BlockSlice(_) => {
            while !filter.done && collector.wants_more() {
                let n = stream
                    .next_chunk(storage, &mut buf)
                    .map_err(|_| ParseError::InvalidArchive)?;
                if n == 0 {
                    break;
                }
                filter.feed_bytes(&buf[..n], &mut collector)?;
            }
        }
        ZipSliceMode::ByteWindow {
            idx,
            target: slice_target,
        } => {
            let target_start = (idx as u64).saturating_mul(slice_target);
            let target_end = target_start.saturating_add(slice_target);
            let mut stream_pos = 0u64;
            let mut synced = idx == 0;
            while !filter.done && collector.wants_more() {
                let n = stream
                    .next_chunk(storage, &mut buf)
                    .map_err(|_| ParseError::InvalidArchive)?;
                if n == 0 {
                    break;
                }
                let chunk_start = stream_pos;
                let chunk_end = stream_pos + n as u64;
                stream_pos = chunk_end;
                if chunk_end <= target_start {
                    continue;
                }
                let mut slice_from = if chunk_start < target_start {
                    (target_start - chunk_start) as usize
                } else {
                    0
                };
                if !synced {
                    if let Some(gt) = buf[slice_from..n].iter().position(|&b| b == b'>') {
                        slice_from += gt + 1;
                        synced = true;
                    } else {
                        continue;
                    }
                }
                if slice_from < n {
                    let past_window = chunk_end >= target_end;
                    if past_window {
                        if let Some(gt) = buf[slice_from..n].iter().position(|&b| b == b'>') {
                            let end = slice_from + gt + 1;
                            filter.feed_bytes(&buf[slice_from..end], &mut collector)?;
                            break;
                        }
                        filter.feed_bytes(&buf[slice_from..n], &mut collector)?;
                        if chunk_end >= target_end + 8192 {
                            break;
                        }
                    } else {
                        filter.feed_bytes(&buf[slice_from..n], &mut collector)?;
                    }
                }
            }
        }
    }

    filter.finish(&mut collector)?;
    finalize_section_blocks(&mut collector.blocks);
    Ok(collector.blocks)
}

/// FNV-1a exact (case-sensitive) 64-bit hash for XML manifest IDs.
fn fnv1a64_exact(s: &str) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for &b in s.as_bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Stack-only canonical ZIP path 64-bit mixed hash resolving `./`, `..`, `%XX`, and `#frag`.
pub(crate) fn hash_canonical_zip_href(base_dir: &str, raw_href: &str) -> u64 {
    let no_frag = raw_href.split('#').next().unwrap_or(raw_href);
    let mut buf = [0u8; 384];
    let mut len = 0usize;

    for &b in base_dir.as_bytes() {
        if len >= buf.len() {
            break;
        }
        buf[len] = if b == b'\\' { b'/' } else { b };
        len += 1;
    }

    let href_bytes = no_frag.as_bytes();
    let mut i = 0usize;
    let hex_val = |b: u8| -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(10 + b - b'a'),
            b'A'..=b'F' => Some(10 + b - b'A'),
            _ => None,
        }
    };
    while i < href_bytes.len() && len < buf.len() {
        let b = if href_bytes[i] == b'%'
            && i + 2 < href_bytes.len()
            && let (Some(hi), Some(lo)) = (hex_val(href_bytes[i + 1]), hex_val(href_bytes[i + 2]))
        {
            i += 3;
            (hi << 4) | lo
        } else {
            let raw = href_bytes[i];
            i += 1;
            raw
        };
        buf[len] = if b == b'\\' { b'/' } else { b };
        len += 1;
    }

    let mut segs: [(usize, usize); 16] = [(0, 0); 16];
    let mut seg_cnt = 0usize;
    let mut s = 0usize;
    while s <= len {
        let e = buf[s..len]
            .iter()
            .position(|&b| b == b'/')
            .map(|p| s + p)
            .unwrap_or(len);
        let seg = &buf[s..e];
        if seg == b".." {
            seg_cnt = seg_cnt.saturating_sub(1);
        } else if !seg.is_empty() && seg != b"." && seg_cnt < segs.len() {
            segs[seg_cnt] = (s, e);
            seg_cnt += 1;
        }
        s = e + 1;
    }

    let mut h1 = 0xcbf29ce484222325u64;
    let mut h2 = 0x9e3779b97f4a7c15u64 ^ (seg_cnt as u64);
    for (idx, &(a, b)) in segs[..seg_cnt].iter().enumerate() {
        if idx > 0 {
            h1 ^= b'/' as u64;
            h1 = h1.wrapping_mul(0x100000001b3);
            h2 ^= b'/' as u64;
            h2 = h2.wrapping_mul(0x100000001b3).rotate_left(7);
        }
        for &ch in &buf[a..b] {
            let lc = ch.to_ascii_lowercase() as u64;
            h1 ^= lc;
            h1 = h1.wrapping_mul(0x100000001b3);
            h2 ^= lc ^ 0x55;
            h2 = h2.wrapping_mul(0x100000001b3).rotate_left(7);
        }
    }
    h1 ^ h2.rotate_left(17)
}

fn open_direct_epub<S: StorageBackend>(
    storage: &S,
    path: &str,
    file_size: u64,
    id_hex: String,
    scratch: &mut ZipInflateScratch,
) -> Result<DirectBook, ParseError> {
    let zip = StorageZip::open(storage, path, file_size).map_err(|_| ParseError::InvalidArchive)?;
    let html_bytes = zip.text_uncomp_bytes();

    let opf_path = {
        let c_meta = zip
            .find_entry("META-INF/container.xml")
            .ok_or(ParseError::MalformedDocument)?;
        let c_raw = zip
            .read_entry_prefix_with_scratch(storage, path, c_meta, 4096, Some(scratch))
            .map_err(|_| ParseError::InvalidArchive)?;
        let (_, c_xml) = detect_and_decode_to_utf8(&c_raw, None);
        let raw_fp = extract_attr(&c_xml, "full-path").ok_or(ParseError::MalformedDocument)?;
        normalize_zip_href(&percent_decode_href(&raw_fp))
    };

    let opf_meta = zip
        .find_entry(&opf_path)
        .ok_or(ParseError::MalformedDocument)?
        .clone();
    let opf_dir = match opf_path.rfind('/') {
        Some(idx) => &opf_path[..=idx],
        None => "",
    };

    // Pass 1: stream OPF to extract DC metadata + stack-hashed manifest/spine, then drop manifest BEFORE CD scan.
    let (title, author, language, mut spine_targets, ncx_hash) =
        stream_epub_opf_compact(storage, file_size, path, &opf_meta, opf_dir, scratch)?;
    if spine_targets.is_empty() {
        return Err(ParseError::MalformedDocument);
    }

    let spine_len = spine_targets.len();
    spine_targets.sort_unstable_by_key(|&(h, _)| h);

    let sentinel = ZipEntryRef {
        method: u16::MAX,
        comp_size: 0,
        uncomp_size: 0,
        local_header_offset: 0,
    };
    let mut raw_chapters: Vec<ZipEntryRef> = vec![sentinel; spine_len];
    zip.scan_cd_entries(storage, path, |name, entry_ref| {
        let clean = name.trim_start_matches("./");
        let h = hash_canonical_zip_href("", clean);
        let mut lo = spine_targets.partition_point(|&(kh, _)| kh < h);
        while lo < spine_targets.len() && spine_targets[lo].0 == h {
            let s_idx = spine_targets[lo].1 as usize;
            if s_idx < raw_chapters.len() {
                raw_chapters[s_idx] = entry_ref;
            }
            lo += 1;
        }
        Ok(())
    })
    .map_err(|_| ParseError::InvalidArchive)?;

    let ncx_meta: Option<ZipEntryMeta> = zip
        .entries()
        .iter()
        .find(|e| {
            let clean = e.name.trim_start_matches("./");
            if let Some(nh) = ncx_hash
                && hash_canonical_zip_href("", clean) == nh
            {
                return true;
            }
            clean.to_ascii_lowercase().ends_with(".ncx")
        })
        .cloned();
    drop(zip);

    let total_uncomp: u64 = raw_chapters
        .iter()
        .filter(|c| c.method != u16::MAX)
        .map(|c| c.uncomp_size as u64)
        .sum();
    let slice_target = if total_uncomp > 48 * 1024 * 1024 {
        EPUB_SLICE_TARGET.max(total_uncomp.div_ceil(2048))
    } else {
        EPUB_SLICE_TARGET
    };

    let needs_expand = raw_chapters
        .iter()
        .any(|c| c.method == u16::MAX || (c.uncomp_size as u64) > slice_target);
    let chapters: Vec<EpubSliceRef> = if needs_expand {
        let mut total_secs = 0usize;
        for c in &raw_chapters {
            if c.method != u16::MAX {
                let parts = ((c.uncomp_size as u64).div_ceil(slice_target)).clamp(1, u16::MAX as u64)
                    as usize;
                total_secs += parts;
            }
        }
        let mut expanded = Vec::with_capacity(total_secs.min(2048));
        let mut remap = vec![u32::MAX; spine_len];
        for (orig_idx, entry) in raw_chapters.into_iter().enumerate() {
            if entry.method != u16::MAX {
                remap[orig_idx] = expanded.len() as u32;
                let parts = ((entry.uncomp_size as u64).div_ceil(slice_target))
                    .clamp(1, u16::MAX as u64) as usize;
                for p in 0..parts {
                    expanded.push(EpubSliceRef::from_entry(entry, p as u16));
                }
            }
        }
        for item in &mut spine_targets {
            let orig = item.1 as usize;
            item.1 = remap.get(orig).copied().unwrap_or(u32::MAX);
        }
        expanded
    } else {
        raw_chapters
            .into_iter()
            .map(|e| EpubSliceRef::from_entry(e, 0))
            .collect()
    };
    if chapters.is_empty() {
        return Err(ParseError::MalformedDocument);
    }

    let mut toc = if let Some(nm) = ncx_meta {
        parse_ncx_toc_stream(storage, path, file_size, &nm, &spine_targets, scratch).unwrap_or_default()
    } else {
        Vec::new()
    };
    drop(spine_targets);

    if toc.is_empty() {
        toc = build_synthetic_toc(chapters.len());
    }

    let est_words = ((html_bytes / 12).max(120)).min(u32::MAX as u64) as u32;

    Ok(DirectBook {
        path: String::from(path),
        meta: BookMetadata {
            id_hex,
            title,
            author,
            language,
            source_format: String::from("EPUB"),
            word_count: est_words,
            cover_w: COVER_THUMB_W,
            cover_h: COVER_THUMB_H,
            cover_2bpp: Vec::new(),
        },
        toc,
        source: DirectSource::Epub {
            file_size,
            chapters,
        },
        est_total_words: est_words,
    })
}

#[allow(clippy::type_complexity)]
pub(crate) fn stream_epub_opf_compact<S: StorageBackend>(
    storage: &S,
    file_size: u64,
    path: &str,
    opf_meta: &ZipEntryMeta,
    opf_dir: &str,
    scratch: &mut ZipInflateScratch,
) -> Result<(String, String, String, Vec<(u64, u32)>, Option<u64>), ParseError> {
    scratch.reset();
    let mut stream = StorageZip::open_entry_ref_stream(
        file_size,
        storage,
        path,
        ZipEntryRef::from(opf_meta),
        512 * 1024,
        Some(scratch),
    )
    .map_err(|_| ParseError::InvalidArchive)?;
    let mut carry: Vec<u8> = Vec::new();
    let mut text = String::new();
    let mut chunk = [0u8; 2048];
    let mut enc: Option<TextEncoding> = None;

    let mut manifest: Vec<(u64, u64)> = Vec::new();
    let mut manifest_sorted = false;
    let mut spine_targets: Vec<(u64, u32)> = Vec::new();
    let mut ncx_hash: Option<u64> = None;

    let mut head_meta_buf = String::new();
    let mut meta_captured = false;
    let mut spine_idx = 0u32;

    loop {
        let n = stream
            .next_chunk(storage, &mut chunk)
            .map_err(|_| ParseError::InvalidArchive)?;
        let eof = n == 0;
        if n > 0 {
            carry.extend_from_slice(&chunk[..n]);
            if enc.is_none() && (carry.len() >= 64 || eof) {
                let utf8_safe = trim_utf8_complete_prefix(&carry);
                enc = Some(detect_encoding(&carry[..utf8_safe], None));
            }
            let use_enc = enc.unwrap_or(TextEncoding::Utf8);
            let usable = if use_enc == TextEncoding::Utf8 {
                trim_utf8_complete_prefix(&carry)
            } else {
                carry.len()
            };
            let decoded = decode_with(&carry[..usable], use_enc);
            if !meta_captured && head_meta_buf.len() < 8192 {
                let room = 8192 - head_meta_buf.len();
                let take = floor_char_boundary(&decoded, room.min(decoded.len()));
                head_meta_buf.push_str(&decoded[..take]);
                if head_meta_buf.len() >= 8192 {
                    meta_captured = true;
                }
            }
            text.push_str(&decoded);
            carry.drain(..usable);
        } else if !carry.is_empty() {
            let decoded = decode_with(&carry, enc.unwrap_or(TextEncoding::Utf8));
            if !meta_captured && head_meta_buf.len() < 8192 {
                let room = 8192 - head_meta_buf.len();
                let take = floor_char_boundary(&decoded, room.min(decoded.len()));
                head_meta_buf.push_str(&decoded[..take]);
            }
            text.push_str(&decoded);
            carry.clear();
        }

        while let Some(pos) = text.find('<') {
            if pos > 0 {
                text.drain(..pos);
            }
            let Some(gt) = text.find('>') else {
                break;
            };
            {
                let tag = &text[..=gt];
                if !tag.starts_with("</") {
                    let lname = opf_tag_local_name(tag);
                    if lname.eq_ignore_ascii_case("item")
                        && let (Some(id), Some(href)) =
                            (extract_attr(tag, "id"), extract_attr(tag, "href"))
                    {
                        if is_spine_text_href(&href) {
                            let h = hash_canonical_zip_href(opf_dir, &href);
                            manifest.push((fnv1a64_exact(&id), h));
                            manifest_sorted = false;
                        } else if ncx_hash.is_none()
                            && (href.to_ascii_lowercase().ends_with(".ncx")
                                || extract_attr(tag, "media-type")
                                    .is_some_and(|mt| mt.contains("dtbncx")))
                        {
                            ncx_hash = Some(hash_canonical_zip_href(opf_dir, &href));
                        }
                    } else if lname.eq_ignore_ascii_case("itemref")
                        && let Some(idref) = extract_attr(tag, "idref")
                    {
                        if !manifest_sorted {
                            manifest.sort_unstable_by_key(|&(id_h, _)| id_h);
                            manifest_sorted = true;
                        }
                        let needle = fnv1a64_exact(&idref);
                        if let Ok(idx) = manifest.binary_search_by_key(&needle, |&(id_h, _)| id_h) {
                            let h = manifest[idx].1;
                            spine_targets.push((h, spine_idx));
                            spine_idx = spine_idx.saturating_add(1);
                        }
                    }
                }
            }
            text.drain(..=gt);
        }
        if !text.contains('<') {
            text.clear();
        }

        if eof {
            break;
        }
    }

    drop(manifest);
    drop(text);
    drop(carry);

    let (title, author, language) = parse_epub_opf_dc_meta(&head_meta_buf, path);
    Ok((title, author, language, spine_targets, ncx_hash))
}

fn parse_ncx_toc_stream<S: StorageBackend>(
    storage: &S,
    path: &str,
    file_size: u64,
    ncx_meta: &ZipEntryMeta,
    spine_targets: &[(u64, u32)],
    scratch: &mut ZipInflateScratch,
) -> Result<Vec<DirectTocItem>, ParseError> {
    let ncx_dir = match ncx_meta.name.trim_start_matches("./").rfind('/') {
        Some(idx) => &ncx_meta.name.trim_start_matches("./")[..=idx],
        None => "",
    };
    let entry_ref = ZipEntryRef::from(ncx_meta);
    scratch.reset();
    let mut stream = StorageZip::open_entry_ref_stream(
        file_size,
        storage,
        path,
        entry_ref,
        512 * 1024,
        Some(scratch),
    )
    .map_err(|_| ParseError::InvalidArchive)?;

    let mut carry = Vec::new();
    let mut text = String::new();
    let mut chunk = [0u8; 2048];
    let mut enc: Option<TextEncoding> = None;

    let mut toc = Vec::with_capacity(MAX_EPUB_TOC_ENTRIES.min(32));
    let mut pending_label = String::new();
    let mut in_text_tag = false;
    let mut cur_label_buf = String::new();
    let mut seen_idx = 0usize;
    let mut stride = 1usize;
    let mut last_item: Option<DirectTocItem> = None;

    loop {
        let n = stream
            .next_chunk(storage, &mut chunk)
            .map_err(|_| ParseError::InvalidArchive)?;
        let eof = n == 0;
        if n > 0 {
            carry.extend_from_slice(&chunk[..n]);
            if enc.is_none() && (carry.len() >= 64 || eof) {
                let utf8_safe = trim_utf8_complete_prefix(&carry);
                enc = Some(detect_encoding(&carry[..utf8_safe], None));
            }
            let use_enc = enc.unwrap_or(TextEncoding::Utf8);
            let usable = if use_enc == TextEncoding::Utf8 {
                trim_utf8_complete_prefix(&carry)
            } else {
                carry.len()
            };
            text.push_str(&decode_with(&carry[..usable], use_enc));
            carry.drain(..usable);
        } else if !carry.is_empty() {
            text.push_str(&decode_with(&carry, enc.unwrap_or(TextEncoding::Utf8)));
            carry.clear();
        }

        while let Some(pos) = text.find('<') {
            if pos > 0 && in_text_tag && cur_label_buf.len() < 256 {
                let room = 256 - cur_label_buf.len();
                let take = floor_char_boundary(&text[..pos], room.min(pos));
                cur_label_buf.push_str(&text[..take]);
            }
            if pos > 0 {
                text.drain(..pos);
            }
            let Some(gt) = text.find('>') else {
                break;
            };
            {
                let tag = &text[..=gt];
                let lname = opf_tag_local_name(tag);
                if tag.starts_with("</") {
                    if lname.eq_ignore_ascii_case("text") && in_text_tag {
                        in_text_tag = false;
                        let cleaned = decode_xml_entities(&cur_label_buf);
                        let trimmed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
                        if !trimmed.is_empty() {
                            pending_label = trimmed;
                        }
                        cur_label_buf.clear();
                    }
                } else if lname.eq_ignore_ascii_case("text") && !tag.ends_with("/>") {
                    in_text_tag = true;
                    cur_label_buf.clear();
                } else if lname.eq_ignore_ascii_case("content")
                    && let Some(src) = extract_attr(tag, "src")
                {
                    let h = hash_canonical_zip_href(ncx_dir, &src);
                    let mut lo = spine_targets.partition_point(|&(kh, _)| kh < h);
                    while lo < spine_targets.len() && spine_targets[lo].0 == h {
                        let sec_idx = spine_targets[lo].1 as usize;
                        if sec_idx != u32::MAX as usize {
                            let label = if pending_label.is_empty() {
                                format!("Chapter {}", sec_idx + 1)
                            } else {
                                core::mem::take(&mut pending_label)
                            };
                            let item = DirectTocItem {
                                title: label,
                                section_idx: sec_idx,
                                block_in_section: 0,
                            };
                            if seen_idx.is_multiple_of(stride)
                                && toc.last().is_none_or(|prev: &DirectTocItem| prev.section_idx != sec_idx)
                            {
                                if toc.len() >= MAX_EPUB_TOC_ENTRIES - 1 {
                                    let mut w = 0usize;
                                    for r in (0..toc.len()).step_by(2) {
                                        toc.swap(w, r);
                                        w += 1;
                                    }
                                    toc.truncate(w);
                                    stride = stride.saturating_mul(2);
                                }
                                toc.push(item.clone());
                            }
                            last_item = Some(item);
                            seen_idx = seen_idx.saturating_add(1);
                            break;
                        }
                        lo += 1;
                    }
                    pending_label.clear();
                }
            }
            text.drain(..=gt);
        }
        if !text.contains('<') && !in_text_tag {
            text.clear();
        }

        if eof {
            break;
        }
    }

    if let Some(last) = last_item
        && toc.last().is_none_or(|prev| prev.section_idx != last.section_idx)
    {
        if toc.len() >= MAX_EPUB_TOC_ENTRIES {
            toc.pop();
        }
        toc.push(last);
    }

    Ok(toc)
}

fn build_synthetic_toc(section_count: usize) -> Vec<DirectTocItem> {
    if section_count <= 1 {
        return vec![DirectTocItem {
            title: String::from("Start"),
            section_idx: 0,
            block_in_section: 0,
        }];
    }
    let count = section_count.min(MAX_SYNTH_TOC_ENTRIES);
    let mut toc = Vec::with_capacity(count);
    for i in 0..count {
        let sec = if i + 1 == count {
            section_count - 1
        } else {
            (i * (section_count - 1)) / (count - 1)
        };
        if toc.last().is_none_or(|p: &DirectTocItem| p.section_idx != sec) {
            toc.push(DirectTocItem {
                title: format!("Section {}", sec + 1),
                section_idx: sec,
                block_in_section: 0,
            });
        }
    }
    toc
}

fn open_direct_fb2_zip<S: StorageBackend>(
    storage: &S,
    path: &str,
    file_size: u64,
    id_hex: String,
    scratch: Option<&mut ZipInflateScratch>,
) -> Result<DirectBook, ParseError> {
    let zip = StorageZip::open(storage, path, file_size).map_err(|_| ParseError::InvalidArchive)?;
    let fb2_meta = zip
        .entries()
        .iter()
        .find(|e| e.name.to_ascii_lowercase().ends_with(".fb2"))
        .or_else(|| zip.entries().first())
        .ok_or(ParseError::MalformedDocument)?
        .clone();
    let entry_ref = ZipEntryRef::from(&fb2_meta);
    let uncomp_sz = fb2_meta.uncomp_size;
    let head = zip
        .read_entry_prefix_with_scratch(storage, path, &fb2_meta, 6 * 1024, scratch)
        .map_err(|_| ParseError::InvalidArchive)?;
    drop(zip);

    let utf8_safe = trim_utf8_complete_prefix(&head);
    let enc = detect_encoding(&head[..utf8_safe], None);
    let (title, author, language) = fb2_meta_from_head(&head, path);
    let section_count = uncomp_sz
        .div_ceil(BYTE_SLICE_TARGET as usize)
        .clamp(1, 2048);
    let est_words = ((uncomp_sz as u64 / 14).max(80)).min(u32::MAX as u64) as u32;

    Ok(DirectBook {
        path: String::from(path),
        meta: BookMetadata {
            id_hex,
            title,
            author,
            language,
            source_format: String::from("FB2.ZIP"),
            word_count: est_words,
            cover_w: COVER_THUMB_W,
            cover_h: COVER_THUMB_H,
            cover_2bpp: Vec::new(),
        },
        toc: build_synthetic_toc(section_count),
        source: DirectSource::Fb2Zip {
            file_size,
            entry: entry_ref,
            enc,
            section_count,
        },
        est_total_words: est_words,
    })
}

fn open_direct_mobi<S: StorageBackend>(
    storage: &S,
    path: &str,
    file_size: u64,
    id_hex: String,
) -> Result<DirectBook, ParseError> {
    let hdr = crate::mobi::probe_mobi_header(storage, path, file_size)?;
    if hdr.encryption_type != 0
        || !matches!(hdr.compression, 1 | 2)
        || hdr.text_record_count == 0
        || hdr.flow0_end == 0
    {
        return Err(ParseError::UnsupportedFormat);
    }
    let section_count =
        ((hdr.text_record_count as usize).div_ceil(crate::mobi::MOBI_RECORDS_PER_SECTION)).max(1);
    let mut toc = crate::mobi::extract_mobi_toc(storage, path, file_size, &hdr, section_count);
    if toc.is_empty() {
        toc = build_synthetic_toc(section_count);
    }
    let est_words = ((hdr.flow0_end as u64 / 12).max(80)).min(u32::MAX as u64) as u32;
    Ok(DirectBook {
        path: String::from(path),
        meta: BookMetadata {
            id_hex,
            title: hdr.title,
            author: hdr.author,
            language: hdr.language,
            source_format: String::from(hdr.source_format),
            word_count: est_words,
            cover_w: COVER_THUMB_W,
            cover_h: COVER_THUMB_H,
            cover_2bpp: Vec::new(),
        },
        toc,
        source: DirectSource::Mobi {
            file_size,
            num_records: hdr.num_records,
            text_record_count: hdr.text_record_count,
            record_size: hdr.record_size,
            flow0_end: hdr.flow0_end,
            compression: hdr.compression,
            encryption_type: hdr.encryption_type,
            extra_flags: hdr.extra_flags,
            enc: hdr.enc,
            section_count,
        },
        est_total_words: est_words,
    })
}

fn open_direct_byte_sliced<S: StorageBackend>(
    storage: &S,
    path: &str,
    file_size: u64,
    fmt: InputFormat,
    id_hex: String,
) -> Result<DirectBook, ParseError> {
    let head = read_storage_prefix(storage, path, 6 * 1024)?;
    let utf8_safe = trim_utf8_complete_prefix(&head);
    let enc = detect_encoding(&head[..utf8_safe], None);
    let head_str = decode_with(&head[..utf8_safe], enc);

    let (title, author, language, src_fmt_str) = match fmt {
        InputFormat::Fb2 => {
            let (t, a, l) = fb2_meta_from_head(&head, path);
            (t, a, l, "FB2")
        }
        InputFormat::Html => {
            let title = extract_xml_tag_text(&head_str, "title")
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| title_from_filename(path));
            let lang = if has_cyrillic(&title) || has_cyrillic(&head_str) {
                "ru"
            } else {
                "en"
            };
            (title, String::from("Web Article"), String::from(lang), "HTML")
        }
        InputFormat::Markdown => {
            let title = head_str
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .map(|l| String::from(l.trim_start_matches('#').trim()))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| title_from_filename(path));
            let lang = if has_cyrillic(&head_str) { "ru" } else { "en" };
            (title, String::from("Markdown Folio"), String::from(lang), "MD")
        }
        InputFormat::Txt => {
            let title = head_str
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .filter(|l| l.chars().count() < 80)
                .map(|l| l.chars().take(80).collect::<String>())
                .unwrap_or_else(|| title_from_filename(path));
            let cyr = has_cyrillic(&head_str);
            let author = if cyr { "Текст" } else { "Plain Text" };
            let lang = if cyr { "ru" } else { "en" };
            (title, String::from(author), String::from(lang), "TXT")
        }
        _ => return Err(ParseError::UnsupportedFormat),
    };

    let mut cur = 0u64;
    if fmt == InputFormat::Fb2 {
        let mut scan_off = 0u64;
        let scan_limit = file_size.min(64 * 1024);
        let mut win = [0u8; 2048 + 16];
        let mut overlap = 0usize;
        let mut found_body_start = false;
        while scan_off < scan_limit {
            let want = ((scan_limit - scan_off) as usize).min(2048);
            let n = storage
                .read_range(path, scan_off, &mut win[overlap..overlap + want])
                .map_err(|_| ParseError::MalformedDocument)?;
            if n == 0 {
                break;
            }
            let buf_len = overlap + n;
            let buf_base = scan_off - overlap as u64;
            let scan_from = if found_body_start { overlap } else { 0 };
            for i in scan_from..buf_len {
                if !found_body_start
                    && i + 5 <= buf_len
                    && win[i..i + 5].eq_ignore_ascii_case(b"<body")
                    && (i + 5 == buf_len
                        || matches!(win[i + 5], b'>' | b' ' | b'\t' | b'\r' | b'\n'))
                {
                    found_body_start = true;
                    if let Some(rel_gt) = win[i + 5..buf_len].iter().position(|&b| b == b'>') {
                        cur = buf_base + (i + 5 + rel_gt + 1) as u64;
                        scan_off = scan_limit;
                        break;
                    }
                } else if found_body_start && win[i] == b'>' {
                    cur = buf_base + (i + 1) as u64;
                    scan_off = scan_limit;
                    break;
                }
            }
            if scan_off >= scan_limit {
                break;
            }
            scan_off += n as u64;
            let keep = buf_len.min(16);
            win.copy_within(buf_len - keep..buf_len, 0);
            overlap = keep;
        }
    }

    let body_start = cur.min(file_size);
    let rem = file_size.saturating_sub(body_start);
    let slice_target = BYTE_SLICE_TARGET.max(rem.div_ceil(1024));
    let section_count = rem.div_ceil(slice_target).max(1) as usize;

    let est_words = ((file_size / 14).max(80)).min(u32::MAX as u64) as u32;
    let toc = build_synthetic_toc(section_count);

    Ok(DirectBook {
        path: String::from(path),
        meta: BookMetadata {
            id_hex,
            title,
            author,
            language,
            source_format: String::from(src_fmt_str),
            word_count: est_words,
            cover_w: COVER_THUMB_W,
            cover_h: COVER_THUMB_H,
            cover_2bpp: Vec::new(),
        },
        toc,
        source: DirectSource::ByteSliced {
            fmt,
            enc,
            body_start,
            file_size,
            slice_target,
            section_count,
        },
        est_total_words: est_words,
    })
}

fn resolve_byte_slice_range<S: StorageBackend>(
    storage: &S,
    path: &str,
    source: &DirectSource,
    section_idx: usize,
) -> Result<(u64, u64), ParseError> {
    let DirectSource::ByteSliced {
        fmt,
        enc,
        body_start,
        file_size,
        slice_target,
        section_count,
    } = *source
    else {
        return Err(ParseError::MalformedDocument);
    };
    let sec_count = section_count.max(1);
    if section_idx >= sec_count {
        return Err(ParseError::MalformedDocument);
    }
    let is_xml = matches!(fmt, InputFormat::Fb2 | InputFormat::Html);
    let snap_at = |boundary_idx: usize| -> Result<u64, ParseError> {
        if boundary_idx == 0 {
            return Ok(body_start);
        }
        if boundary_idx >= sec_count {
            return Ok(file_size);
        }
        let probe = body_start
            .saturating_add((boundary_idx as u64).saturating_mul(slice_target))
            .min(file_size);
        let prev_nom = body_start
            .saturating_add(((boundary_idx - 1) as u64).saturating_mul(slice_target));
        let win_start = probe
            .saturating_sub(512)
            .max(prev_nom.saturating_add(1024))
            .min(probe);
        let want = (probe - win_start).min(512) as usize;
        let mut seam_buf = [0u8; 512];
        let read_len = if want > 0 {
            storage
                .read_range(path, win_start, &mut seam_buf[..want])
                .map_err(|_| ParseError::MalformedDocument)?
        } else {
            0
        };
        let snapped = snap_byte_slice_seam(&seam_buf[..read_len], win_start, probe, is_xml, enc);
        Ok(snapped.clamp(prev_nom.saturating_add(1).min(file_size), file_size))
    };
    let start = snap_at(section_idx)?;
    let end = snap_at(section_idx + 1)?.max(start);
    Ok((start, end))
}

fn snap_byte_slice_seam(
    win: &[u8],
    win_start: u64,
    fallback: u64,
    is_xml: bool,
    enc: TextEncoding,
) -> u64 {
    if win.is_empty() {
        return fallback;
    }
    let rel_end = if is_xml {
        win.windows(4)
            .rposition(|w| w.eq_ignore_ascii_case(b"</p>"))
            .map(|p| p + 4)
            .or_else(|| win.iter().rposition(|&b| b == b'>').map(|p| p + 1))
    } else {
        win.windows(2)
            .rposition(|w| w == b"\n\n")
            .map(|p| p + 2)
            .or_else(|| win.iter().rposition(|&b| b == b'\n').map(|p| p + 1))
            .or_else(|| win.iter().rposition(|&b| b == b' ').map(|p| p + 1))
    };

    if let Some(rel) = rel_end.filter(|&r| r > 0) {
        return win_start + rel as u64;
    }

    if enc == TextEncoding::Utf8 {
        // Walk backward up to 4 bytes so we never split a UTF-8 codepoint at `fallback`.
        let off_in_win = (fallback - win_start).min(win.len() as u64) as usize;
        for back in 0..4usize.min(off_in_win) {
            let b = win[off_in_win - 1 - back];
            if (b & 0xC0) != 0x80 {
                // Start byte (either ASCII `< 0x80` or leading multi-byte). Split BEFORE multi-byte or wait:
                // if ASCII, split right after it; if leading byte, split AT `off_in_win - 1 - back`.
                return if b < 0x80 {
                    win_start + (off_in_win - back) as u64
                } else {
                    win_start + (off_in_win - 1 - back) as u64
                };
            }
        }
    }
    fallback
}

fn load_byte_slice_section<S: StorageBackend>(
    storage: &S,
    path: &str,
    fmt: InputFormat,
    enc: TextEncoding,
    start: u64,
    end: u64,
    seen_body_hint: bool,
) -> Result<Vec<Block>, ParseError> {
    let mut off = start;
    let mut buf = [0u8; 2048];

    match fmt {
        InputFormat::Fb2 | InputFormat::Html => {
            let is_fb2 = fmt == InputFormat::Fb2;
            let mut filter = XmlChunkFilter::new(is_fb2);
            filter.set_encoding(enc);
            if seen_body_hint {
                filter.set_seen_body(true);
            }
            let mut collector = VecBlockCollector::new(DIRECT_MAX_SECTION_BLOCKS);
            while off < end && !filter.done && collector.wants_more() {
                let want = ((end - off) as usize).min(buf.len());
                let n = storage
                    .read_range(path, off, &mut buf[..want])
                    .map_err(|_| ParseError::MalformedDocument)?;
                if n == 0 {
                    break;
                }
                off += n as u64;
                filter.feed_bytes(&buf[..n], &mut collector)?;
            }
            filter.finish(&mut collector)?;
            finalize_section_blocks(&mut collector.blocks);
            Ok(collector.blocks)
        }
        InputFormat::Markdown | InputFormat::Txt => {
            let is_md = fmt == InputFormat::Markdown;
            let mut raw = Vec::with_capacity(((end - start) as usize).min(18 * 1024));
            while off < end && raw.len() < 18 * 1024 {
                let want = ((end - off) as usize)
                    .min(buf.len())
                    .min((18 * 1024usize).saturating_sub(raw.len()));
                let n = storage
                    .read_range(path, off, &mut buf[..want])
                    .map_err(|_| ParseError::MalformedDocument)?;
                if n == 0 {
                    break;
                }
                off += n as u64;
                raw.extend_from_slice(&buf[..n]);
            }
            let txt = if enc == TextEncoding::Utf8 {
                match String::from_utf8(raw) {
                    Ok(s) => s,
                    Err(e) => {
                        let bytes = e.into_bytes();
                        let s = String::from(String::from_utf8_lossy(&bytes).as_ref());
                        drop(bytes);
                        s
                    }
                }
            } else {
                let s = decode_with(&raw, enc);
                drop(raw);
                s
            };
            let mut parsed = if is_md {
                parse_markdown(&txt, path)
            } else {
                parse_plain_text(&txt, path)
            };
            drop(txt);
            split_oversize_blocks(&mut parsed.blocks);
            parsed.blocks.truncate(DIRECT_MAX_SECTION_BLOCKS);
            Ok(parsed.blocks)
        }
        _ => Err(ParseError::UnsupportedFormat),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::BTreeMap;
    use miniz_oxide::deflate::compress_to_vec;
    use vvbook_pal::{DirEntry, StorageError};

    struct TestStore {
        files: BTreeMap<String, Vec<u8>>,
        writes: usize,
    }

    impl TestStore {
        fn new() -> Self {
            Self {
                files: BTreeMap::new(),
                writes: 0,
            }
        }
    }

    impl StorageBackend for TestStore {
        fn exists(&self, p: &str) -> bool {
            self.files.contains_key(p)
        }
        fn file_len(&self, p: &str) -> Result<u64, StorageError> {
            self.files
                .get(p)
                .map(|v| v.len() as u64)
                .ok_or(StorageError::NotFound)
        }
        fn read_all(&self, p: &str) -> Result<Vec<u8>, StorageError> {
            panic!("read_all forbidden on DirectBook path ({p})!");
        }
        fn read_range(&self, p: &str, offset: u64, buf: &mut [u8]) -> Result<usize, StorageError> {
            let d = self.files.get(p).ok_or(StorageError::NotFound)?;
            let off = offset as usize;
            if off >= d.len() {
                return Ok(0);
            }
            let n = buf.len().min(d.len() - off);
            buf[..n].copy_from_slice(&d[off..off + n]);
            Ok(n)
        }
        fn write_all(&mut self, p: &str, data: &[u8]) -> Result<(), StorageError> {
            self.writes += 1;
            self.files.insert(String::from(p), data.to_vec());
            Ok(())
        }
        fn write_atomic(&mut self, p: &str, data: &[u8]) -> Result<(), StorageError> {
            self.writes += 1;
            self.files.insert(String::from(p), data.to_vec());
            Ok(())
        }
        fn append(&mut self, p: &str, data: &[u8]) -> Result<(), StorageError> {
            self.writes += 1;
            self.files
                .entry(String::from(p))
                .or_default()
                .extend_from_slice(data);
            Ok(())
        }
        fn remove(&mut self, p: &str) -> Result<(), StorageError> {
            self.files.remove(p);
            Ok(())
        }
        fn mkdir_all(&mut self, _: &str) -> Result<(), StorageError> {
            Ok(())
        }
        fn list_dir(&self, _: &str) -> Result<Vec<DirEntry>, StorageError> {
            Ok(Vec::new())
        }
    }

    fn make_custom_zip(files: &[(&[u8], &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut cd = Vec::new();
        for (name, payload) in files {
            let lfh_offset = out.len() as u32;
            let comp = compress_to_vec(payload, 4);
            out.extend_from_slice(&0x04034B50u32.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&8u16.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(&(comp.len() as u32).to_le_bytes());
            out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name);
            out.extend_from_slice(&comp);

            cd.extend_from_slice(&0x02014B50u32.to_le_bytes());
            cd.extend_from_slice(&20u16.to_le_bytes());
            cd.extend_from_slice(&20u16.to_le_bytes());
            cd.extend_from_slice(&0u16.to_le_bytes());
            cd.extend_from_slice(&8u16.to_le_bytes());
            cd.extend_from_slice(&0u32.to_le_bytes());
            cd.extend_from_slice(&0u32.to_le_bytes());
            cd.extend_from_slice(&(comp.len() as u32).to_le_bytes());
            cd.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            cd.extend_from_slice(&(name.len() as u16).to_le_bytes());
            cd.extend_from_slice(&0u16.to_le_bytes());
            cd.extend_from_slice(&0u16.to_le_bytes());
            cd.extend_from_slice(&0u16.to_le_bytes());
            cd.extend_from_slice(&0u16.to_le_bytes());
            cd.extend_from_slice(&0u32.to_le_bytes());
            cd.extend_from_slice(&lfh_offset.to_le_bytes());
            cd.extend_from_slice(name);
        }
        let cd_offset = out.len() as u32;
        let cd_size = cd.len() as u32;
        out.extend_from_slice(&cd);
        out.extend_from_slice(&0x06054B50u32.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    #[test]
    fn direct_epub_650_chapters_cp866_svg_br_zero_writes() {
        let mut manifest = String::new();
        let mut spine = String::new();
        let mut ncx_nav = String::new();
        let mut files_owned: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();

        files_owned.push((
            b"META-INF/container.xml".to_vec(),
            br#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#.to_vec(),
        ));

        for i in 0..650usize {
            let id = format!("c{i:04}");
            let href = format!("Text/ch_{i:04}.xhtml");
            manifest.push_str(&format!(
                "<item id=\"{id}\" href=\"{href}\" media-type=\"application/xhtml+xml\"/>"
            ));
            spine.push_str(&format!("<itemref idref=\"{id}\"/>"));
            ncx_nav.push_str(&format!(
                "<navPoint id=\"np{i}\"><navLabel><text>Chapter {}</text></navLabel><content src=\"{href}\"/></navPoint>",
                i + 1
            ));

            let body = if i == 0 {
                let mut long_br = String::from("<svg><path d=\"M0 0 L999 999\"/></svg><p>");
                for w in 0..3200usize {
                    long_br.push_str("word");
                    long_br.push_str(&format!("{w}"));
                    if w % 10 == 9 {
                        long_br.push_str("<br/>");
                    } else {
                        long_br.push(' ');
                    }
                }
                long_br.push_str("</p>");
                format!("<html><body><h1>Chapter 1</h1>{long_br}</body></html>")
            } else {
                format!("<html><body><h1>Chapter {}</h1><p>Content for chapter {}.</p></body></html>", i + 1, i + 1)
            };
            files_owned.push((format!("OEBPS/{href}").into_bytes(), body.into_bytes()));
        }

        // Non-UTF8 CP866 asset entry name that previously failed UTF-8 CD parsing
        files_owned.push((b"OEBPS/Images/\x8e\xa1\xeb.jpg".to_vec(), vec![0xFFu8; 64]));

        manifest.push_str("<item id=\"ncx\" href=\"toc.ncx\" media-type=\"application/x-dtbncx+xml\"/>");
        let opf = format!(
            "<?xml version=\"1.0\"?><package><metadata><dc:title>Mega 650</dc:title><dc:creator>Author</dc:creator><dc:language>en</dc:language></metadata><manifest>{manifest}</manifest><spine toc=\"ncx\">{spine}</spine></package>"
        );
        let ncx = format!("<?xml version=\"1.0\"?><ncx><navMap>{ncx_nav}</navMap></ncx>");
        files_owned.push((b"OEBPS/content.opf".to_vec(), opf.into_bytes()));
        files_owned.push((b"OEBPS/toc.ncx".to_vec(), ncx.into_bytes()));

        let refs: Vec<(&[u8], &[u8])> = files_owned
            .iter()
            .map(|(n, d)| (n.as_slice(), d.as_slice()))
            .collect();
        let epub_bytes = make_custom_zip(&refs);
        let epub_len = epub_bytes.len() as u64;

        let mut storage = TestStore::new();
        storage.write_atomic("/mega.epub", &epub_bytes).unwrap();
        let writes_before = storage.writes;

        let book = DirectBook::open(&storage, "/mega.epub", epub_len, None).unwrap();
        assert_eq!(storage.writes, writes_before);
        assert!(book.section_count() >= 650);
        let last_sec = book.section_count() - 1;
        assert!(book.toc.len() >= 32 && book.toc.len() <= MAX_EPUB_TOC_ENTRIES);
        assert_eq!(book.toc.last().unwrap().section_idx, last_sec);

        let sec0 = book.load_section(&storage, 0).unwrap();
        assert_eq!(storage.writes, writes_before);
        assert!(!sec0.is_empty());
        assert!(sec0.len() <= DIRECT_MAX_SECTION_BLOCKS);
        let has_svg = sec0.iter().any(|b| format!("{b:?}").contains("M0 0"));
        assert!(!has_svg, "<svg> content must be skipped");

        let sec_last = book.load_section(&storage, last_sec).unwrap();
        assert_eq!(storage.writes, writes_before);
        assert!(sec_last.iter().any(|b| matches!(b, Block::ChapterTitle { title } if title.contains("650"))));
    }

    #[test]
    fn direct_byte_sliced_txt_150kb_splits_seams() {
        let mut txt = String::new();
        for i in 0..3000usize {
            txt.push_str(&format!("Paragraph {i}: Привет мир и спокойное чтение на X3.\n\n"));
        }
        let mut storage = TestStore::new();
        storage.write_atomic("/big.txt", txt.as_bytes()).unwrap();
        let writes_before = storage.writes;
        let book = DirectBook::open(&storage, "/big.txt", txt.len() as u64, None).unwrap();
        assert_eq!(storage.writes, writes_before);
        assert!(book.section_count() >= 4);
        let last = book.load_section(&storage, book.section_count() - 1).unwrap();
        assert_eq!(storage.writes, writes_before);
        assert!(!last.is_empty());
    }
}
