extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use vvbook_pal::StorageBackend;

pub const VVBK_MAGIC: [u8; 4] = *b"VVBK";
pub const VVBK_VERSION: u16 = 1;
pub const VVB_FLAG_TRAILER_INDEX: u16 = 0x0001;
pub const VVB_FLAG_SPARSE_CHECKPOINTS: u16 = 0x0002;
pub const MAX_VVB_CHECKPOINTS: usize = 4096;
pub const VVBT_FOOTER_MAGIC: [u8; 4] = *b"VVBT";
pub const VVB_PREAMBLE_LEN: u32 = 44;
pub const VVLC_MAGIC: [u8; 4] = *b"VVLC"; // layout page cache
pub const VVCC_MAGIC: [u8; 4] = *b"VVCC"; // catalog index

pub const COVER_THUMB_W: u16 = 112;
pub const COVER_THUMB_H: u16 = 164;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpanStyle {
    Regular = 0,
    Italic = 1,
    Bold = 2,
    BoldItalic = 3,
    Code = 4,
}

impl SpanStyle {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Italic,
            2 => Self::Bold,
            3 => Self::BoldItalic,
            4 => Self::Code,
            _ => Self::Regular,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextRun {
    pub style: SpanStyle,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    ChapterTitle { title: String },
    Heading { level: u8, text: String },
    Paragraph { runs: Vec<TextRun>, first_of_chapter: bool, is_dialogue: bool },
    VerseLine { text: String },
    BlockQuote { runs: Vec<TextRun> },
    CodeBlock { text: String },
    ListItem { bullet: String, runs: Vec<TextRun> },
    SceneBreak,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TocEntry {
    pub title: String,
    pub block_index: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BookMetadata {
    pub id_hex: String,
    pub title: String,
    pub author: String,
    pub language: String,
    pub source_format: String,
    pub word_count: u32,
    pub cover_w: u16,
    pub cover_h: u16,
    /// Packed 2-bpp Atkinson-dithered thumbnail (`cover_w * cover_h / 4` bytes).
    pub cover_2bpp: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VvbBook {
    pub meta: BookMetadata,
    pub toc: Vec<TocEntry>,
    pub blocks: Vec<Block>,
    pub content_blake3: [u8; 32],
}

/// Lightweight metadata + chapter header for `VvbStorageReader` so ESP32-C3 can
/// open any 4,000+ page book on the 16 GB SD card using <= 16 KB checkpoint RAM!
#[derive(Clone, Debug)]
pub struct VvbIndex {
    pub meta: BookMetadata,
    pub toc: Vec<TocEntry>,
    pub block_count: usize,
    pub checkpoint_shift: u8,
    pub block_checkpoints: Vec<u32>,
    pub block_stream_base: u32,
    pub content_blake3: [u8; 32],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VvbError {
    BadMagic,
    UnsupportedVersion,
    Truncated,
    Corrupted,
}

fn push_str(out: &mut Vec<u8>, s: &str) {
    let b = s.as_bytes();
    let len = b.len().min(u32::MAX as usize) as u32;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&b[..len as usize]);
}

fn read_u8(src: &[u8], pos: &mut usize) -> Result<u8, VvbError> {
    if *pos >= src.len() {
        return Err(VvbError::Truncated);
    }
    let v = src[*pos];
    *pos += 1;
    Ok(v)
}

fn read_u16(src: &[u8], pos: &mut usize) -> Result<u16, VvbError> {
    let end = pos
        .checked_add(2)
        .filter(|&e| e <= src.len())
        .ok_or(VvbError::Truncated)?;
    let v = u16::from_le_bytes([src[*pos], src[*pos + 1]]);
    *pos = end;
    Ok(v)
}

fn read_u32(src: &[u8], pos: &mut usize) -> Result<u32, VvbError> {
    let end = pos
        .checked_add(4)
        .filter(|&e| e <= src.len())
        .ok_or(VvbError::Truncated)?;
    let v = u32::from_le_bytes(src[*pos..end].try_into().unwrap());
    *pos = end;
    Ok(v)
}

fn read_str(src: &[u8], pos: &mut usize) -> Result<String, VvbError> {
    let len = usize::try_from(read_u32(src, pos)?).map_err(|_| VvbError::Truncated)?;
    let end = pos
        .checked_add(len)
        .filter(|&e| e <= src.len())
        .ok_or(VvbError::Truncated)?;
    let s = core::str::from_utf8(&src[*pos..end]).map_err(|_| VvbError::Corrupted)?;
    *pos = end;
    Ok(String::from(s))
}

fn encode_runs(out: &mut Vec<u8>, runs: &[TextRun]) {
    let count = runs.len().min(u16::MAX as usize) as u16;
    out.extend_from_slice(&count.to_le_bytes());
    for r in runs.iter().take(count as usize) {
        out.push(r.style as u8);
        push_str(out, &r.text);
    }
}

fn decode_runs(src: &[u8], pos: &mut usize) -> Result<Vec<TextRun>, VvbError> {
    let count = read_u16(src, pos)? as usize;
    let mut runs = Vec::with_capacity(count);
    for _ in 0..count {
        if *pos >= src.len() {
            return Err(VvbError::Truncated);
        }
        let style = SpanStyle::from_u8(src[*pos]);
        *pos += 1;
        let text = read_str(src, pos)?;
        runs.push(TextRun { style, text });
    }
    Ok(runs)
}

pub fn encode_block(block: &Block) -> Vec<u8> {
    let mut out = Vec::new();
    match block {
        Block::ChapterTitle { title } => {
            out.push(0);
            push_str(&mut out, title);
        }
        Block::Heading { level, text } => {
            out.push(1);
            out.push(*level);
            push_str(&mut out, text);
        }
        Block::Paragraph {
            runs,
            first_of_chapter,
            is_dialogue,
        } => {
            out.push(2);
            let flags = ((*first_of_chapter as u8) & 1) | (((*is_dialogue as u8) & 1) << 1);
            out.push(flags);
            encode_runs(&mut out, runs);
        }
        Block::VerseLine { text } => {
            out.push(3);
            push_str(&mut out, text);
        }
        Block::BlockQuote { runs } => {
            out.push(4);
            encode_runs(&mut out, runs);
        }
        Block::CodeBlock { text } => {
            out.push(5);
            push_str(&mut out, text);
        }
        Block::ListItem { bullet, runs } => {
            out.push(6);
            push_str(&mut out, bullet);
            encode_runs(&mut out, runs);
        }
        Block::SceneBreak => {
            out.push(7);
        }
    }
    out
}

pub fn decode_block(src: &[u8]) -> Result<Block, VvbError> {
    if src.is_empty() {
        return Err(VvbError::Truncated);
    }
    let tag = src[0];
    let mut pos = 1usize;
    match tag {
        0 => Ok(Block::ChapterTitle {
            title: read_str(src, &mut pos)?,
        }),
        1 => {
            if pos >= src.len() {
                return Err(VvbError::Truncated);
            }
            let level = src[pos];
            pos += 1;
            let text = read_str(src, &mut pos)?;
            Ok(Block::Heading { level, text })
        }
        2 => {
            if pos >= src.len() {
                return Err(VvbError::Truncated);
            }
            let flags = src[pos];
            pos += 1;
            let runs = decode_runs(src, &mut pos)?;
            Ok(Block::Paragraph {
                runs,
                first_of_chapter: (flags & 1) != 0,
                is_dialogue: (flags & 2) != 0,
            })
        }
        3 => Ok(Block::VerseLine {
            text: read_str(src, &mut pos)?,
        }),
        4 => Ok(Block::BlockQuote {
            runs: decode_runs(src, &mut pos)?,
        }),
        5 => Ok(Block::CodeBlock {
            text: read_str(src, &mut pos)?,
        }),
        6 => {
            let bullet = read_str(src, &mut pos)?;
            let runs = decode_runs(src, &mut pos)?;
            Ok(Block::ListItem { bullet, runs })
        }
        7 => Ok(Block::SceneBreak),
        _ => Err(VvbError::Corrupted),
    }
}

pub fn encode_vvb_meta_and_toc(
    meta: &BookMetadata,
    toc: &[TocEntry],
    block_count: u32,
) -> Vec<u8> {
    let mut meta_buf = Vec::new();
    push_str(&mut meta_buf, &meta.id_hex);
    push_str(&mut meta_buf, &meta.title);
    push_str(&mut meta_buf, &meta.author);
    push_str(&mut meta_buf, &meta.language);
    push_str(&mut meta_buf, &meta.source_format);
    meta_buf.extend_from_slice(&meta.word_count.to_le_bytes());
    meta_buf.extend_from_slice(&meta.cover_w.to_le_bytes());
    meta_buf.extend_from_slice(&meta.cover_h.to_le_bytes());
    meta_buf.extend_from_slice(&(meta.cover_2bpp.len() as u32).to_le_bytes());
    meta_buf.extend_from_slice(&meta.cover_2bpp);

    meta_buf.extend_from_slice(&(toc.len() as u32).to_le_bytes());
    for t in toc {
        push_str(&mut meta_buf, &t.title);
        meta_buf.extend_from_slice(&t.block_index.to_le_bytes());
    }
    meta_buf.extend_from_slice(&block_count.to_le_bytes());
    meta_buf
}

pub fn build_vvb_trailer_preamble(content_blake3: [u8; 32]) -> [u8; 44] {
    let mut out = [0u8; 44];
    out[0..4].copy_from_slice(&VVBK_MAGIC);
    out[4..6].copy_from_slice(&VVBK_VERSION.to_le_bytes());
    out[6..8].copy_from_slice(&VVB_FLAG_TRAILER_INDEX.to_le_bytes());
    out[8..40].copy_from_slice(&content_blake3);
    out[40..44].copy_from_slice(&VVB_PREAMBLE_LEN.to_le_bytes());
    out
}

pub fn build_vvb_sparse_trailer_preamble(content_blake3: [u8; 32]) -> [u8; 44] {
    let mut out = [0u8; 44];
    out[0..4].copy_from_slice(&VVBK_MAGIC);
    out[4..6].copy_from_slice(&VVBK_VERSION.to_le_bytes());
    out[6..8].copy_from_slice(
        &(VVB_FLAG_TRAILER_INDEX | VVB_FLAG_SPARSE_CHECKPOINTS).to_le_bytes(),
    );
    out[8..40].copy_from_slice(&content_blake3);
    out[40..44].copy_from_slice(&VVB_PREAMBLE_LEN.to_le_bytes());
    out
}

pub fn build_vvb_header_prefix(
    meta: &BookMetadata,
    toc: &[TocEntry],
    block_count: u32,
    content_blake3: [u8; 32],
) -> (Vec<u8>, u32) {
    let meta_buf = encode_vvb_meta_and_toc(meta, toc, block_count);
    let stream_base = 44 + meta_buf.len() as u32 + block_count * 4;
    let mut out = Vec::with_capacity(44 + meta_buf.len());
    out.extend_from_slice(&VVBK_MAGIC);
    out.extend_from_slice(&VVBK_VERSION.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&content_blake3);
    out.extend_from_slice(&stream_base.to_le_bytes());
    out.extend_from_slice(&meta_buf);
    (out, stream_base)
}

pub fn build_vvb_header(
    meta: &BookMetadata,
    toc: &[TocEntry],
    offsets: &[u32],
    content_blake3: [u8; 32],
) -> Vec<u8> {
    let (mut out, _) = build_vvb_header_prefix(meta, toc, offsets.len() as u32, content_blake3);
    out.reserve(offsets.len() * 4);
    for off in offsets {
        out.extend_from_slice(&off.to_le_bytes());
    }
    out
}

impl VvbBook {
    pub fn to_bytes(&self) -> Vec<u8> {
        // Pre-encode blocks so we know exact seek offsets
        let mut block_stream = Vec::new();
        let mut offsets = Vec::with_capacity(self.blocks.len());
        for blk in &self.blocks {
            offsets.push(block_stream.len() as u32);
            let encoded = encode_block(blk);
            block_stream.extend_from_slice(&(encoded.len() as u32).to_le_bytes());
            block_stream.extend_from_slice(&encoded);
        }

        let mut out = build_vvb_header(&self.meta, &self.toc, &offsets, self.content_blake3);
        out.extend_from_slice(&block_stream);
        out
    }

    pub fn from_bytes(raw: &[u8]) -> Result<Self, VvbError> {
        let idx = VvbIndex::from_header_bytes(raw)?;
        let max_blocks = raw.len() / 4;
        if idx.block_count > max_blocks {
            return Err(VvbError::Truncated);
        }
        let mut blocks = Vec::with_capacity(idx.block_count);
        let cp0 = usize::try_from(idx.block_checkpoints.first().copied().unwrap_or(0))
            .map_err(|_| VvbError::Truncated)?;
        let mut p = usize::try_from(idx.block_stream_base)
            .ok()
            .and_then(|base| base.checked_add(cp0))
            .ok_or(VvbError::Truncated)?;
        for _ in 0..idx.block_count {
            let len = usize::try_from(read_u32(raw, &mut p)?).map_err(|_| VvbError::Truncated)?;
            let end = p
                .checked_add(len)
                .filter(|&e| e <= raw.len())
                .ok_or(VvbError::Truncated)?;
            blocks.push(decode_block(&raw[p..end])?);
            p = end;
        }
        Ok(Self {
            meta: idx.meta,
            toc: idx.toc,
            blocks,
            content_blake3: idx.content_blake3,
        })
    }
}

struct StorageCursor<'a, S: StorageBackend> {
    storage: &'a S,
    path: &'a str,
    pos: u64,
    end: u64,
    buf: [u8; 4096],
    b_pos: usize,
    b_len: usize,
}

impl<'a, S: StorageBackend> StorageCursor<'a, S> {
    fn new(storage: &'a S, path: &'a str, start_off: u64, end_off: u64, seed: &[u8]) -> Self {
        let mut buf = [0u8; 4096];
        let seed_len = seed.len().min(buf.len());
        buf[..seed_len].copy_from_slice(&seed[..seed_len]);
        Self {
            storage,
            path,
            pos: start_off + seed_len as u64,
            end: end_off,
            buf,
            b_pos: 0,
            b_len: seed_len,
        }
    }

    #[inline]
    fn remaining(&self) -> usize {
        let unbuffered = self.end.saturating_sub(self.pos) as usize;
        unbuffered.saturating_add(self.b_len.saturating_sub(self.b_pos))
    }

    fn refill(&mut self) -> Result<(), VvbError> {
        if self.b_pos < self.b_len {
            return Ok(());
        }
        if self.pos >= self.end {
            return Err(VvbError::Truncated);
        }
        let want = ((self.end - self.pos) as usize).min(self.buf.len());
        let n = self
            .storage
            .read_range(self.path, self.pos, &mut self.buf[..want])
            .map_err(|_| VvbError::Truncated)?;
        if n == 0 {
            return Err(VvbError::Truncated);
        }
        self.pos += n as u64;
        self.b_pos = 0;
        self.b_len = n;
        Ok(())
    }

    fn read_exact(&mut self, mut out: &mut [u8]) -> Result<(), VvbError> {
        while !out.is_empty() {
            self.refill()?;
            let avail = self.b_len - self.b_pos;
            let take = avail.min(out.len());
            out[..take].copy_from_slice(&self.buf[self.b_pos..self.b_pos + take]);
            self.b_pos += take;
            out = &mut out[take..];
        }
        Ok(())
    }

    fn skip(&mut self, mut n: usize) -> Result<(), VvbError> {
        if n > self.remaining() {
            return Err(VvbError::Truncated);
        }
        let buf_avail = self.b_len - self.b_pos;
        if n <= buf_avail {
            self.b_pos += n;
            return Ok(());
        }
        n -= buf_avail;
        self.b_pos = 0;
        self.b_len = 0;
        self.pos += n as u64;
        Ok(())
    }

    fn read_u8(&mut self) -> Result<u8, VvbError> {
        let mut b = [0u8; 1];
        self.read_exact(&mut b)?;
        Ok(b[0])
    }

    fn read_u16(&mut self) -> Result<u16, VvbError> {
        let mut b = [0u8; 2];
        self.read_exact(&mut b)?;
        Ok(u16::from_le_bytes(b))
    }

    fn read_u32(&mut self) -> Result<u32, VvbError> {
        let mut b = [0u8; 4];
        self.read_exact(&mut b)?;
        Ok(u32::from_le_bytes(b))
    }

    fn read_str(&mut self, max_len: usize) -> Result<String, VvbError> {
        let len = self.read_u32()? as usize;
        if len > self.remaining() {
            return Err(VvbError::Truncated);
        }
        if len > 16 * 1024 {
            return Err(VvbError::Corrupted);
        }
        let keep = len.min(max_len);
        let mut raw = vec![0u8; keep];
        self.read_exact(&mut raw)?;
        if len > keep {
            self.skip(len - keep)?;
            let trimmed = String::from_utf8_lossy(&raw).into_owned();
            return Ok(trimmed);
        }
        String::from_utf8(raw).map_err(|_| VvbError::Corrupted)
    }
}

impl VvbIndex {
    pub fn from_header_bytes(raw: &[u8]) -> Result<Self, VvbError> {
        if raw.len() < 44 {
            return Err(VvbError::Truncated);
        }
        if raw[0..4] != VVBK_MAGIC {
            return Err(VvbError::BadMagic);
        }
        let ver = u16::from_le_bytes([raw[4], raw[5]]);
        if ver != VVBK_VERSION {
            return Err(VvbError::UnsupportedVersion);
        }
        let mut content_blake3 = [0u8; 32];
        content_blake3.copy_from_slice(&raw[8..40]);
        let stream_base = u32::from_le_bytes(raw[40..44].try_into().unwrap());
        let flags = u16::from_le_bytes([raw[6], raw[7]]);
        let is_sparse = (flags & VVB_FLAG_SPARSE_CHECKPOINTS) != 0;
        let (hdr, mut pos) = if (flags & VVB_FLAG_TRAILER_INDEX) != 0 {
            if (flags & !(VVB_FLAG_TRAILER_INDEX | VVB_FLAG_SPARSE_CHECKPOINTS)) != 0 {
                return Err(VvbError::UnsupportedVersion);
            }
            if raw.len() < 52 {
                return Err(VvbError::Truncated);
            }
            if stream_base != VVB_PREAMBLE_LEN {
                return Err(VvbError::Corrupted);
            }
            if raw[raw.len() - 4..] != VVBT_FOOTER_MAGIC {
                return Err(VvbError::Truncated);
            }
            let end = raw.len() - 8;
            let trailer_off = u32::from_le_bytes(raw[end..end + 4].try_into().unwrap()) as usize;
            if !(44..=end).contains(&trailer_off) {
                return Err(VvbError::Corrupted);
            }
            (&raw[..end], trailer_off)
        } else {
            if flags != 0 {
                return Err(VvbError::UnsupportedVersion);
            }
            (raw, 44usize)
        };

        let id_hex = read_str(hdr, &mut pos)?;
        let title = read_str(hdr, &mut pos)?;
        let author = read_str(hdr, &mut pos)?;
        let language = read_str(hdr, &mut pos)?;
        let source_format = read_str(hdr, &mut pos)?;
        let word_count = read_u32(hdr, &mut pos)?;
        let cover_w = read_u16(hdr, &mut pos)?;
        let cover_h = read_u16(hdr, &mut pos)?;
        let cover_len = usize::try_from(read_u32(hdr, &mut pos)?).map_err(|_| VvbError::Truncated)?;
        let cover_end = pos
            .checked_add(cover_len)
            .filter(|&e| e <= hdr.len())
            .ok_or(VvbError::Truncated)?;
        let cover_2bpp = hdr[pos..cover_end].to_vec();
        pos = cover_end;

        let toc_count = usize::try_from(read_u32(hdr, &mut pos)?).map_err(|_| VvbError::Truncated)?;
        if toc_count > (hdr.len() - pos) / 8 {
            return Err(VvbError::Truncated);
        }
        let mut toc = Vec::with_capacity(toc_count);
        for _ in 0..toc_count {
            let t = read_str(hdr, &mut pos)?;
            let bi = read_u32(hdr, &mut pos)?;
            toc.push(TocEntry {
                title: t,
                block_index: bi,
            });
        }

        let block_count =
            usize::try_from(read_u32(hdr, &mut pos)?).map_err(|_| VvbError::Truncated)?;
        let (checkpoint_shift, block_checkpoints) = if is_sparse {
            let shift = read_u8(hdr, &mut pos)?;
            let cp_count =
                usize::try_from(read_u32(hdr, &mut pos)?).map_err(|_| VvbError::Truncated)?;
            if shift > 24 || cp_count > MAX_VVB_CHECKPOINTS {
                return Err(VvbError::Corrupted);
            }
            if cp_count > (hdr.len() - pos) / 4 {
                return Err(VvbError::Truncated);
            }
            let mut cps = Vec::with_capacity(cp_count);
            for _ in 0..cp_count {
                cps.push(read_u32(hdr, &mut pos)?);
            }
            (shift, cps)
        } else {
            if block_count > (hdr.len() - pos) / 4 {
                return Err(VvbError::Truncated);
            }
            let mut shift = 0u8;
            while shift < 24 && block_count.div_ceil(1usize << shift) > MAX_VVB_CHECKPOINTS {
                shift += 1;
            }
            let mask = (1usize << shift) - 1;
            let mut cps = Vec::with_capacity(block_count.div_ceil(1usize << shift));
            for i in 0..block_count {
                let off = read_u32(hdr, &mut pos)?;
                if (i & mask) == 0 {
                    cps.push(off);
                }
            }
            (shift, cps)
        };

        Ok(Self {
            meta: BookMetadata {
                id_hex,
                title,
                author,
                language,
                source_format,
                word_count,
                cover_w,
                cover_h,
                cover_2bpp,
            },
            toc,
            block_count,
            checkpoint_shift,
            block_checkpoints,
            block_stream_base: stream_base,
            content_blake3,
        })
    }

    /// Stream ONLY metadata + TOC + <= 16 KB block checkpoints from SD via a 4 KB stack cursor.
    pub fn open_from_storage<S: StorageBackend>(storage: &S, path: &str) -> Result<Self, VvbError> {
        const MAX_HEADER_BYTES: usize = 512 * 1024;
        const MAX_COVER_BYTES: usize = 32 * 1024;
        const MAX_TOC_ENTRIES: usize = 4096;
        const KEEP_TOC_ENTRIES: usize = 512;
        const MAX_BLOCKS: usize = 262_144;

        let mut hdr = [0u8; 512];
        let n = storage
            .read_range(path, 0, &mut hdr)
            .map_err(|_| VvbError::Truncated)?;
        if n < 44 {
            return Err(VvbError::Truncated);
        }
        if hdr[0..4] != VVBK_MAGIC {
            return Err(VvbError::BadMagic);
        }
        let ver = u16::from_le_bytes([hdr[4], hdr[5]]);
        if ver != VVBK_VERSION {
            return Err(VvbError::UnsupportedVersion);
        }
        let mut content_blake3 = [0u8; 32];
        content_blake3.copy_from_slice(&hdr[8..40]);
        let stream_base = u32::from_le_bytes(hdr[40..44].try_into().unwrap()) as usize;
        let flags = u16::from_le_bytes([hdr[6], hdr[7]]);
        let is_sparse = (flags & VVB_FLAG_SPARSE_CHECKPOINTS) != 0;
        let mut cur = if (flags & VVB_FLAG_TRAILER_INDEX) != 0 {
            if (flags & !(VVB_FLAG_TRAILER_INDEX | VVB_FLAG_SPARSE_CHECKPOINTS)) != 0 {
                return Err(VvbError::UnsupportedVersion);
            }
            if stream_base != VVB_PREAMBLE_LEN as usize {
                return Err(VvbError::Corrupted);
            }
            let file_len = storage.file_len(path).map_err(|_| VvbError::Truncated)?;
            if file_len < 52 {
                return Err(VvbError::Truncated);
            }
            let mut footer = [0u8; 8];
            if storage
                .read_range(path, file_len - 8, &mut footer)
                .map_err(|_| VvbError::Truncated)?
                < 8
                || footer[4..8] != VVBT_FOOTER_MAGIC
            {
                return Err(VvbError::Truncated);
            }
            let trailer_off = u32::from_le_bytes(footer[0..4].try_into().unwrap()) as u64;
            let trailer_end = file_len - 8;
            if trailer_off < VVB_PREAMBLE_LEN as u64
                || trailer_off > trailer_end
                || (trailer_end - trailer_off) > MAX_HEADER_BYTES as u64
            {
                return Err(VvbError::Corrupted);
            }
            StorageCursor::new(storage, path, trailer_off, trailer_end, &[])
        } else {
            if flags != 0 {
                return Err(VvbError::UnsupportedVersion);
            }
            if !(44..=MAX_HEADER_BYTES).contains(&stream_base) {
                return Err(VvbError::Corrupted);
            }
            let seed_end = n.min(stream_base);
            StorageCursor::new(storage, path, 44, stream_base as u64, &hdr[44..seed_end])
        };

        let id_hex = cur.read_str(256)?;
        let title = cur.read_str(1024)?;
        let author = cur.read_str(512)?;
        let language = cur.read_str(64)?;
        let source_format = cur.read_str(64)?;
        let word_count = cur.read_u32()?;
        let mut cover_w = cur.read_u16()?;
        let mut cover_h = cur.read_u16()?;
        let cover_len = cur.read_u32()? as usize;
        if cover_len > cur.remaining() {
            return Err(VvbError::Truncated);
        }
        let cover_2bpp = if cover_len <= MAX_COVER_BYTES {
            let mut c = vec![0u8; cover_len];
            cur.read_exact(&mut c)?;
            c
        } else {
            cur.skip(cover_len)?;
            cover_w = 0;
            cover_h = 0;
            Vec::new()
        };

        let toc_count = cur.read_u32()? as usize;
        if toc_count > MAX_TOC_ENTRIES {
            return Err(VvbError::Corrupted);
        }
        let mut toc = Vec::with_capacity(toc_count.min(KEEP_TOC_ENTRIES));
        for i in 0..toc_count {
            let t = cur.read_str(512)?;
            let bi = cur.read_u32()?;
            if i < KEEP_TOC_ENTRIES {
                toc.push(TocEntry {
                    title: t,
                    block_index: bi,
                });
            }
        }

        let block_count = cur.read_u32()? as usize;
        if block_count > MAX_BLOCKS {
            return Err(VvbError::Corrupted);
        }
        let (checkpoint_shift, block_checkpoints) = if is_sparse {
            let shift = cur.read_u8()?;
            let cp_count = cur.read_u32()? as usize;
            if shift > 24
                || cp_count > MAX_VVB_CHECKPOINTS
                || cp_count.saturating_mul(4) > cur.remaining()
            {
                return Err(VvbError::Corrupted);
            }
            let mut cps = Vec::with_capacity(cp_count);
            for _ in 0..cp_count {
                cps.push(cur.read_u32()?);
            }
            (shift, cps)
        } else {
            if block_count.saturating_mul(4) > cur.remaining() {
                return Err(VvbError::Corrupted);
            }
            let mut shift = 0u8;
            while shift < 24 && block_count.div_ceil(1usize << shift) > MAX_VVB_CHECKPOINTS {
                shift += 1;
            }
            let mask = (1usize << shift) - 1;
            let mut cps = Vec::with_capacity(block_count.div_ceil(1usize << shift));
            for i in 0..block_count {
                let off = cur.read_u32()?;
                if (i & mask) == 0 {
                    cps.push(off);
                }
            }
            (shift, cps)
        };

        Ok(Self {
            meta: BookMetadata {
                id_hex,
                title,
                author,
                language,
                source_format,
                word_count,
                cover_w,
                cover_h,
                cover_2bpp,
            },
            toc,
            block_count,
            checkpoint_shift,
            block_checkpoints,
            block_stream_base: stream_base as u32,
            content_blake3,
        })
    }

    pub fn block_count(&self) -> usize {
        self.block_count
    }

    fn resolve_block_abs<S: StorageBackend>(
        &self,
        storage: &S,
        path: &str,
        block_idx: usize,
    ) -> Result<u64, VvbError> {
        if block_idx >= self.block_count {
            return Err(VvbError::Truncated);
        }
        let cp_idx = block_idx >> self.checkpoint_shift;
        let cp_rel = *self.block_checkpoints.get(cp_idx).ok_or(VvbError::Truncated)?;
        let mut abs = self.block_stream_base as u64 + cp_rel as u64;
        let stride = 1usize << self.checkpoint_shift;
        let skip = block_idx & (stride - 1);
        for _ in 0..skip {
            let mut len_buf = [0u8; 4];
            if storage.read_range(path, abs, &mut len_buf).map_err(|_| VvbError::Truncated)? < 4 {
                return Err(VvbError::Truncated);
            }
            let len = u32::from_le_bytes(len_buf) as u64;
            if len > 256 * 1024 {
                return Err(VvbError::Corrupted);
            }
            abs += 4 + len;
        }
        Ok(abs)
    }

    pub fn read_block<S: StorageBackend>(&self, storage: &S, path: &str, block_idx: usize) -> Result<Block, VvbError> {
        let abs = self.resolve_block_abs(storage, path, block_idx)?;
        let mut len_buf = [0u8; 4];
        if storage.read_range(path, abs, &mut len_buf).map_err(|_| VvbError::Truncated)? < 4 {
            return Err(VvbError::Truncated);
        }
        let len = u32::from_le_bytes(len_buf) as usize;
        if len > 256 * 1024 {
            return Err(VvbError::Corrupted);
        }
        let mut buf = vec![0u8; len];
        if storage
            .read_range(path, abs + 4, &mut buf)
            .map_err(|_| VvbError::Truncated)?
            < len
        {
            return Err(VvbError::Truncated);
        }
        decode_block(&buf)
    }

    /// Sequentially reads `count` consecutive blocks starting at `start_idx` using a single checkpoint walk.
    pub fn read_blocks_range<S: StorageBackend>(
        &self,
        storage: &S,
        path: &str,
        start_idx: usize,
        count: usize,
    ) -> Result<Vec<Block>, VvbError> {
        if start_idx >= self.block_count || count == 0 {
            return Ok(Vec::new());
        }
        let n = count.min(self.block_count - start_idx);
        let mut abs = self.resolve_block_abs(storage, path, start_idx)?;
        let mut blocks = Vec::with_capacity(n);
        let mut scratch = Vec::new();
        let mut win = [0u8; 4096];
        let mut w_pos = 0usize;
        let mut w_len = 0usize;

        for _ in 0..n {
            let mut len_buf = [0u8; 4];
            let mut got = 0usize;
            while got < 4 {
                if w_pos < w_len {
                    let take = (w_len - w_pos).min(4 - got);
                    len_buf[got..got + take].copy_from_slice(&win[w_pos..w_pos + take]);
                    w_pos += take;
                    abs += take as u64;
                    got += take;
                } else {
                    w_pos = 0;
                    w_len = storage.read_range(path, abs, &mut win).map_err(|_| VvbError::Truncated)?;
                    if w_len == 0 {
                        return Err(VvbError::Truncated);
                    }
                }
            }
            let len = u32::from_le_bytes(len_buf) as usize;
            if len > 256 * 1024 {
                return Err(VvbError::Corrupted);
            }
            scratch.resize(len, 0);
            let mut pgot = 0usize;
            if w_pos < w_len {
                let take = (w_len - w_pos).min(len);
                scratch[..take].copy_from_slice(&win[w_pos..w_pos + take]);
                w_pos += take;
                abs += take as u64;
                pgot += take;
            }
            if pgot < len {
                let rem = len - pgot;
                if rem < win.len() / 2 {
                    while pgot < len {
                        w_len = storage.read_range(path, abs, &mut win).map_err(|_| VvbError::Truncated)?;
                        if w_len == 0 {
                            return Err(VvbError::Truncated);
                        }
                        let take = w_len.min(len - pgot);
                        scratch[pgot..pgot + take].copy_from_slice(&win[..take]);
                        w_pos = take;
                        abs += take as u64;
                        pgot += take;
                    }
                } else {
                    let read = storage
                        .read_range(path, abs, &mut scratch[pgot..])
                        .map_err(|_| VvbError::Truncated)?;
                    if read < rem {
                        return Err(VvbError::Truncated);
                    }
                    abs += rem as u64;
                    w_pos = 0;
                    w_len = 0;
                }
            }
            blocks.push(decode_block(&scratch)?);
        }
        Ok(blocks)
    }
}

pub fn intern_source_format(s: &str) -> &'static str {
    let t = s.trim();
    if t.eq_ignore_ascii_case("EPUB") {
        "EPUB"
    } else if t.eq_ignore_ascii_case("FB2") {
        "FB2"
    } else if t.eq_ignore_ascii_case("FB2.ZIP") {
        "FB2.ZIP"
    } else if t.eq_ignore_ascii_case("MOBI") {
        "MOBI"
    } else if t.eq_ignore_ascii_case("AZW3") {
        "AZW3"
    } else if t.eq_ignore_ascii_case("AZW") {
        "AZW"
    } else if t.eq_ignore_ascii_case("PRC") {
        "PRC"
    } else if t.eq_ignore_ascii_case("TXT") {
        "TXT"
    } else if t.eq_ignore_ascii_case("MD") {
        "MD"
    } else if t.eq_ignore_ascii_case("HTML") {
        "HTML"
    } else if t.eq_ignore_ascii_case("PDF") {
        "PDF"
    } else if t.eq_ignore_ascii_case("VVB") {
        "VVB"
    } else {
        "BOOK"
    }
}

pub fn intern_language(s: &str) -> &'static str {
    let primary = s
        .trim()
        .split(['-', '_'])
        .next()
        .unwrap_or("")
        .trim();
    if primary.eq_ignore_ascii_case("ru") || primary.eq_ignore_ascii_case("rus") {
        "ru"
    } else if primary.eq_ignore_ascii_case("en") || primary.eq_ignore_ascii_case("eng") {
        "en"
    } else if primary.eq_ignore_ascii_case("uk") || primary.eq_ignore_ascii_case("ukr") {
        "uk"
    } else if primary.eq_ignore_ascii_case("be") || primary.eq_ignore_ascii_case("bel") {
        "be"
    } else if primary.eq_ignore_ascii_case("de")
        || primary.eq_ignore_ascii_case("ger")
        || primary.eq_ignore_ascii_case("deu")
    {
        "de"
    } else if primary.eq_ignore_ascii_case("fr")
        || primary.eq_ignore_ascii_case("fre")
        || primary.eq_ignore_ascii_case("fra")
    {
        "fr"
    } else if primary.eq_ignore_ascii_case("es") || primary.eq_ignore_ascii_case("spa") {
        "es"
    } else if primary.eq_ignore_ascii_case("it") || primary.eq_ignore_ascii_case("ita") {
        "it"
    } else if primary.eq_ignore_ascii_case("pl") || primary.eq_ignore_ascii_case("pol") {
        "pl"
    } else if primary.eq_ignore_ascii_case("la") || primary.eq_ignore_ascii_case("lat") {
        "la"
    } else {
        "und"
    }
}

fn read_str_slice<'a>(src: &'a [u8], pos: &mut usize) -> Result<&'a str, VvbError> {
    let len = usize::try_from(read_u32(src, pos)?).map_err(|_| VvbError::Truncated)?;
    let end = pos
        .checked_add(len)
        .filter(|&e| e <= src.len())
        .ok_or(VvbError::Truncated)?;
    let s = core::str::from_utf8(&src[*pos..end]).map_err(|_| VvbError::Corrupted)?;
    *pos = end;
    Ok(s)
}

/// Library catalog persisted at `/.vvbook/catalog.vvc` on the 16 GB microSD card.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogEntry {
    pub id_hex: String,
    pub source_path: String,
    pub title: String,
    pub author: String,
    pub language: &'static str,
    pub source_format: &'static str,
    pub word_count: u32,
    pub block_count: u32,
    pub last_block: u32,
    pub last_word: u16,
    pub total_pages_cached: u32,
    pub current_page_cached: u32,
    pub bookmarked_block: Option<u32>,
}

impl CatalogEntry {
    pub fn vvb_path(&self) -> String {
        format!("/.vvbook/books/{}.vvb", self.id_hex)
    }
}

pub fn catalog_format_version(raw: &[u8]) -> Option<u16> {
    if raw.len() < 10 || raw[0..4] != VVCC_MAGIC {
        return None;
    }
    Some(u16::from_le_bytes([raw[4], raw[5]]))
}

pub fn encode_catalog(entries: &[CatalogEntry]) -> Vec<u8> {
    let exact_len = 10
        + entries
            .iter()
            .map(|e| {
                50 + e.id_hex.len()
                    + e.source_path.len()
                    + e.title.len()
                    + e.author.len()
                    + e.language.len()
                    + e.source_format.len()
            })
            .sum::<usize>();
    let mut out = Vec::with_capacity(exact_len);
    out.extend_from_slice(&VVCC_MAGIC);
    out.extend_from_slice(&3u16.to_le_bytes());
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for e in entries {
        push_str(&mut out, &e.id_hex);
        push_str(&mut out, &e.source_path);
        push_str(&mut out, &e.title);
        push_str(&mut out, &e.author);
        push_str(&mut out, e.language);
        push_str(&mut out, e.source_format);
        out.extend_from_slice(&e.word_count.to_le_bytes());
        out.extend_from_slice(&e.block_count.to_le_bytes());
        out.extend_from_slice(&e.last_block.to_le_bytes());
        out.extend_from_slice(&e.last_word.to_le_bytes());
        out.extend_from_slice(&e.total_pages_cached.to_le_bytes());
        out.extend_from_slice(&e.current_page_cached.to_le_bytes());
        let bm = e.bookmarked_block.unwrap_or(u32::MAX);
        out.extend_from_slice(&bm.to_le_bytes());
    }
    out
}

pub fn decode_catalog(raw: &[u8]) -> Result<Vec<CatalogEntry>, VvbError> {
    if raw.len() < 10 || raw[0..4] != VVCC_MAGIC {
        return Err(VvbError::BadMagic);
    }
    let ver = u16::from_le_bytes([raw[4], raw[5]]);
    if ver != 2 && ver != 3 {
        return Err(VvbError::UnsupportedVersion);
    }
    let mut pos = 6usize;
    let count = read_u32(raw, &mut pos)? as usize;
    let mut entries = Vec::with_capacity(count.min(512));
    for _ in 0..count {
        let id_hex = read_str(raw, &mut pos)?;
        let source_path = read_str(raw, &mut pos)?;
        let title = read_str(raw, &mut pos)?;
        let author = read_str(raw, &mut pos)?;
        let language = intern_language(read_str_slice(raw, &mut pos)?);
        let source_format = intern_source_format(read_str_slice(raw, &mut pos)?);
        let word_count = read_u32(raw, &mut pos)?;
        let block_count = read_u32(raw, &mut pos)?;
        let last_block = read_u32(raw, &mut pos)?;
        let last_word = read_u16(raw, &mut pos)?;
        let total_pages_cached = read_u32(raw, &mut pos)?;
        let current_page_cached = read_u32(raw, &mut pos)?;
        let bm = read_u32(raw, &mut pos)?;
        entries.push(CatalogEntry {
            id_hex,
            source_path,
            title,
            author,
            language,
            source_format,
            word_count,
            block_count,
            last_block,
            last_word,
            total_pages_cached,
            current_page_cached,
            bookmarked_block: if bm == u32::MAX { None } else { Some(bm) },
        });
    }
    Ok(entries)
}

/// Page anchor cached on SD in `/.vvbook/layout/<id>_<key>.vvl`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageAnchor {
    pub block_idx: u32,
    pub word_offset: u16,
}

pub fn encode_layout_cache(pages: &[PageAnchor]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + pages.len() * 6);
    out.extend_from_slice(&VVLC_MAGIC);
    out.extend_from_slice(&(pages.len() as u32).to_le_bytes());
    for p in pages {
        out.extend_from_slice(&p.block_idx.to_le_bytes());
        out.extend_from_slice(&p.word_offset.to_le_bytes());
    }
    out
}

pub fn decode_layout_cache(raw: &[u8]) -> Result<Vec<PageAnchor>, VvbError> {
    if raw.len() < 8 || raw[0..4] != VVLC_MAGIC {
        return Err(VvbError::BadMagic);
    }
    let count = usize::try_from(u32::from_le_bytes(raw[4..8].try_into().unwrap()))
        .map_err(|_| VvbError::Truncated)?;
    let expected = count
        .checked_mul(6)
        .and_then(|n| n.checked_add(8))
        .ok_or(VvbError::Truncated)?;
    if raw.len() != expected {
        return Err(VvbError::Truncated);
    }
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let base = 8 + i * 6;
        let block_idx = u32::from_le_bytes(raw[base..base + 4].try_into().unwrap());
        let word_offset = u16::from_le_bytes(raw[base + 4..base + 6].try_into().unwrap());
        out.push(PageAnchor {
            block_idx,
            word_offset,
        });
    }
    Ok(out)
}
