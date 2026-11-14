extern crate alloc;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use miniz_oxide::inflate::decompress_to_vec_with_limit;
use miniz_oxide::inflate::stream::{InflateState, inflate};
use miniz_oxide::{DataFormat, MZError, MZFlush, MZStatus};
use vvbook_pal::StorageBackend;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZipError {
    BadArchive,
    EntryNotFound,
    UnsupportedMethod,
    SecurityViolation,
    DecompressFailed,
}

#[derive(Clone, Debug)]
pub struct ZipEntryMeta {
    pub name: String,
    pub method: u16,
    pub comp_size: usize,
    pub uncomp_size: usize,
    pub local_header_offset: usize,
}

/// Compact 16-byte copyable reference to a ZIP entry (zero `String` heap per chapter).
#[derive(Clone, Copy, Debug)]
pub struct ZipEntryRef {
    pub method: u16,
    pub comp_size: u32,
    pub uncomp_size: u32,
    pub local_header_offset: u32,
}

impl From<&ZipEntryMeta> for ZipEntryRef {
    fn from(m: &ZipEntryMeta) -> Self {
        Self {
            method: m.method,
            comp_size: m.comp_size as u32,
            uncomp_size: m.uncomp_size as u32,
            local_header_offset: m.local_header_offset as u32,
        }
    }
}

/// Reusable ~32-40 KB DEFLATE state across all chapters of an EPUB so we never
/// reallocate an `InflateState` per chapter on constrained SRAM.
pub struct ZipInflateScratch(Box<InflateState>);

impl ZipInflateScratch {
    pub fn new() -> Self {
        Self(InflateState::new_boxed(DataFormat::Raw))
    }

    pub fn reset(&mut self) {
        self.0.reset(DataFormat::Raw);
    }
}

impl Default for ZipInflateScratch {
    fn default() -> Self {
        Self::new()
    }
}

pub struct ZipArchive<'a> {
    data: &'a [u8],
    entries: Vec<ZipEntryMeta>,
}

const MAX_ENTRIES: usize = 4096;
const MAX_ENTRY_UNCOMP: usize = 16 * 1024 * 1024; // 16 MiB cap per entry
const MAX_STREAM_ENTRY_UNCOMP: usize = 512 * 1024 * 1024; // 512 MiB cap for streamed entries
const MAX_RATIO: usize = 64; // Zip-bomb defense

fn find_eocd(tail: &[u8]) -> Option<usize> {
    if tail.len() < 22 {
        return None;
    }
    let mut i = tail.len() - 22;
    loop {
        if tail[i..i + 4] == [0x50, 0x4B, 0x05, 0x06] {
            let comment_len = u16::from_le_bytes([tail[i + 20], tail[i + 21]]) as usize;
            if i + 22 + comment_len == tail.len() {
                return Some(i);
            }
        }
        if i == 0 {
            break;
        }
        i -= 1;
    }
    None
}

fn is_binary_asset_entry(name: &str) -> bool {
    let lo = name.to_ascii_lowercase();
    lo.ends_with('/')
        || lo.ends_with(".jpg")
        || lo.ends_with(".jpeg")
        || lo.ends_with(".png")
        || lo.ends_with(".gif")
        || lo.ends_with(".webp")
        || lo.ends_with(".bmp")
        || lo.ends_with(".svg")
        || lo.ends_with(".ico")
        || lo.ends_with(".ttf")
        || lo.ends_with(".otf")
        || lo.ends_with(".woff")
        || lo.ends_with(".woff2")
        || lo.ends_with(".css")
        || lo.ends_with(".mp3")
        || lo.ends_with(".ogg")
        || lo.ends_with(".mp4")
        || lo.ends_with(".m4a")
}

fn ends_with_ascii_ci(s: &str, suffix: &str) -> bool {
    s.len() >= suffix.len()
        && s.as_bytes()[s.len() - suffix.len()..].eq_ignore_ascii_case(suffix.as_bytes())
}

fn is_text_book_entry(name: &str) -> bool {
    ends_with_ascii_ci(name, ".xhtml")
        || ends_with_ascii_ci(name, ".html")
        || ends_with_ascii_ci(name, ".htm")
        || ends_with_ascii_ci(name, ".fb2")
        || ends_with_ascii_ci(name, ".txt")
        || ends_with_ascii_ci(name, ".md")
}

fn is_essential_zip_meta(name: &str) -> bool {
    let clean = name.trim_start_matches("./");
    clean.eq_ignore_ascii_case("META-INF/container.xml")
        || ends_with_ascii_ci(clean, ".opf")
        || ends_with_ascii_ci(clean, ".ncx")
        || ends_with_ascii_ci(clean, ".fb2")
}

fn validate_cd_record(
    rec: &[u8],
    pos: usize,
    archive_len: usize,
) -> Result<(ZipEntryMeta, usize), ZipError> {
    if pos + 46 > rec.len() || rec[pos..pos + 4] != [0x50, 0x4B, 0x01, 0x02] {
        return Err(ZipError::BadArchive);
    }
    let method = u16::from_le_bytes([rec[pos + 10], rec[pos + 11]]);
    let comp_size = u32::from_le_bytes(rec[pos + 20..pos + 24].try_into().unwrap()) as usize;
    let uncomp_size = u32::from_le_bytes(rec[pos + 24..pos + 28].try_into().unwrap()) as usize;
    let name_len = u16::from_le_bytes([rec[pos + 28], rec[pos + 29]]) as usize;
    let extra_len = u16::from_le_bytes([rec[pos + 30], rec[pos + 31]]) as usize;
    let comment_len = u16::from_le_bytes([rec[pos + 32], rec[pos + 33]]) as usize;
    let lfh_off = u32::from_le_bytes(rec[pos + 42..pos + 46].try_into().unwrap()) as usize;

    let name_end = pos + 46 + name_len;
    let next_pos = name_end + extra_len + comment_len;
    if next_pos > rec.len() {
        return Err(ZipError::BadArchive);
    }
    let raw_name = &rec[pos + 46..name_end];
    let name = String::from_utf8_lossy(raw_name);

    if name.starts_with('/') || name.starts_with('\\') || name.split(['/', '\\']).any(|seg| seg == "..") {
        return Err(ZipError::SecurityViolation);
    }
    if uncomp_size > MAX_ENTRY_UNCOMP || lfh_off >= archive_len {
        return Err(ZipError::SecurityViolation);
    }
    if method == 8 && comp_size > 0 && uncomp_size > comp_size.saturating_mul(MAX_RATIO).max(65536) {
        return Err(ZipError::SecurityViolation);
    }

    Ok((
        ZipEntryMeta {
            name: name.into_owned(),
            method,
            comp_size,
            uncomp_size,
            local_header_offset: lfh_off,
        },
        next_pos,
    ))
}

fn parse_cd_entries(cd: &[u8], total_entries: usize, archive_len: usize) -> Result<Vec<ZipEntryMeta>, ZipError> {
    let mut entries = Vec::with_capacity(total_entries.min(256));
    let mut pos = 0usize;
    for _ in 0..total_entries {
        let (meta, next_pos) = validate_cd_record(cd, pos, archive_len)?;
        entries.push(meta);
        pos = next_pos;
    }
    Ok(entries)
}

impl<'a> ZipArchive<'a> {
    pub fn parse(data: &'a [u8]) -> Result<Self, ZipError> {
        if data.len() < 22 {
            return Err(ZipError::BadArchive);
        }
        let search_start = data.len().saturating_sub(65_557);
        let rel_eocd = find_eocd(&data[search_start..]).ok_or(ZipError::BadArchive)?;
        let eocd = search_start + rel_eocd;
        let total_entries = u16::from_le_bytes([data[eocd + 10], data[eocd + 11]]) as usize;
        let cd_size = u32::from_le_bytes(data[eocd + 12..eocd + 16].try_into().unwrap()) as usize;
        let cd_offset = u32::from_le_bytes(data[eocd + 16..eocd + 20].try_into().unwrap()) as usize;

        if total_entries > MAX_ENTRIES || cd_offset.checked_add(cd_size).is_none_or(|end| end > data.len()) {
            return Err(ZipError::SecurityViolation);
        }

        let entries = parse_cd_entries(&data[cd_offset..cd_offset + cd_size], total_entries, data.len())?;
        Ok(Self { data, entries })
    }

    pub fn entries(&self) -> &[ZipEntryMeta] {
        &self.entries
    }

    pub fn read_entry(&self, name: &str) -> Result<Vec<u8>, ZipError> {
        let meta = self
            .entries
            .iter()
            .find(|e| e.name == name)
            .ok_or(ZipError::EntryNotFound)?;
        self.read_meta(meta)
    }

    pub fn read_meta(&self, meta: &ZipEntryMeta) -> Result<Vec<u8>, ZipError> {
        let off = meta.local_header_offset;
        if off + 30 > self.data.len() || self.data[off..off + 4] != [0x50, 0x4B, 0x03, 0x04] {
            return Err(ZipError::BadArchive);
        }
        let nlen = u16::from_le_bytes([self.data[off + 26], self.data[off + 27]]) as usize;
        let xlen = u16::from_le_bytes([self.data[off + 28], self.data[off + 29]]) as usize;
        let payload_start = off + 30 + nlen + xlen;
        let payload_end = payload_start.checked_add(meta.comp_size).ok_or(ZipError::BadArchive)?;
        if payload_end > self.data.len() {
            return Err(ZipError::BadArchive);
        }
        let slice = &self.data[payload_start..payload_end];
        match meta.method {
            0 => Ok(slice.to_vec()),
            8 => {
                let cap = meta.uncomp_size.max(meta.comp_size * MAX_RATIO).min(MAX_ENTRY_UNCOMP);
                let out = decompress_to_vec_with_limit(slice, cap)
                    .map_err(|_| ZipError::DecompressFailed)?;
                Ok(out)
            }
            _ => Err(ZipError::UnsupportedMethod),
        }
    }
}

/// Low-RAM streaming ZIP reader over `StorageBackend::read_range` (never loads entire archive or CD).
pub struct StorageZip {
    entries: Vec<ZipEntryMeta>,
    file_size: u64,
    cd_offset: u64,
    cd_size: usize,
    total_entries: usize,
    text_uncomp_bytes: u64,
}

fn stream_cd_records<S: StorageBackend, F>(
    storage: &S,
    path: &str,
    file_size: u64,
    cd_offset: u64,
    cd_size: usize,
    total_entries: usize,
    mut on_record: F,
) -> Result<(), ZipError>
where
    F: FnMut(&str, ZipEntryRef) -> Result<(), ZipError>,
{
    let fsize_usize = usize::try_from(file_size).unwrap_or(usize::MAX);
    let mut carry = Vec::with_capacity(4608);
    let mut chunk = [0u8; 4096];
    let mut cd_pos = 0usize;
    let mut pos = 0usize;

    for _ in 0..total_entries {
        while carry.len().saturating_sub(pos) < 46 {
            if pos > 0 {
                carry.drain(..pos);
                pos = 0;
            }
            if cd_pos >= cd_size {
                return Err(ZipError::BadArchive);
            }
            let want = (cd_size - cd_pos).min(chunk.len());
            let n = storage
                .read_range(path, cd_offset + cd_pos as u64, &mut chunk[..want])
                .map_err(|_| ZipError::BadArchive)?;
            if n == 0 {
                return Err(ZipError::BadArchive);
            }
            cd_pos += n;
            carry.extend_from_slice(&chunk[..n]);
        }

        if carry[pos..pos + 4] != [0x50, 0x4B, 0x01, 0x02] {
            return Err(ZipError::BadArchive);
        }
        let name_len = u16::from_le_bytes([carry[pos + 28], carry[pos + 29]]) as usize;
        let extra_len = u16::from_le_bytes([carry[pos + 30], carry[pos + 31]]) as usize;
        let comment_len = u16::from_le_bytes([carry[pos + 32], carry[pos + 33]]) as usize;
        let rec_len = 46 + name_len + extra_len + comment_len;
        if rec_len > 16_384 {
            return Err(ZipError::SecurityViolation);
        }

        while carry.len().saturating_sub(pos) < rec_len {
            if pos > 0 {
                carry.drain(..pos);
                pos = 0;
            }
            if cd_pos >= cd_size {
                return Err(ZipError::BadArchive);
            }
            let want = (cd_size - cd_pos).min(chunk.len());
            let n = storage
                .read_range(path, cd_offset + cd_pos as u64, &mut chunk[..want])
                .map_err(|_| ZipError::BadArchive)?;
            if n == 0 {
                return Err(ZipError::BadArchive);
            }
            cd_pos += n;
            carry.extend_from_slice(&chunk[..n]);
        }

        let method = u16::from_le_bytes([carry[pos + 10], carry[pos + 11]]);
        let comp_size = u32::from_le_bytes(carry[pos + 20..pos + 24].try_into().unwrap()) as usize;
        let uncomp_size = u32::from_le_bytes(carry[pos + 24..pos + 28].try_into().unwrap()) as usize;
        let lfh_off = u32::from_le_bytes(carry[pos + 42..pos + 46].try_into().unwrap()) as usize;
        let name_end = pos + 46 + name_len;
        let next_pos = name_end + extra_len + comment_len;
        if next_pos > carry.len() {
            return Err(ZipError::BadArchive);
        }
        let name = String::from_utf8_lossy(&carry[pos + 46..name_end]);
        if name.starts_with('/') || name.starts_with('\\') || name.split(['/', '\\']).any(|seg| seg == "..") {
            return Err(ZipError::SecurityViolation);
        }
        if uncomp_size > MAX_STREAM_ENTRY_UNCOMP || lfh_off >= fsize_usize {
            return Err(ZipError::SecurityViolation);
        }
        let entry_ref = ZipEntryRef {
            method,
            comp_size: comp_size as u32,
            uncomp_size: uncomp_size as u32,
            local_header_offset: lfh_off as u32,
        };
        on_record(name.as_ref(), entry_ref)?;
        pos = next_pos;
    }
    Ok(())
}

impl StorageZip {
    pub fn open<S: StorageBackend>(storage: &S, path: &str, file_size: u64) -> Result<Self, ZipError> {
        const MAX_STORAGE_ENTRIES: usize = 32_768;
        const MAX_STORAGE_CD_BYTES: usize = 2 * 1024 * 1024;
        const MAX_KEPT_ENTRIES: usize = 16;

        if file_size < 22 {
            return Err(ZipError::BadArchive);
        }

        // Zero-heap backwards EOCD search across up to 65,557 trailing bytes
        let earliest = file_size.saturating_sub(65_557);
        let mut end = file_size;
        let mut eocd_hdr = None;
        let mut tail_win = [0u8; 1046];
        while end > earliest {
            let win_start = end.saturating_sub(1045).max(earliest);
            let want = (end - win_start) as usize;
            let n = storage
                .read_range(path, win_start, &mut tail_win[..want])
                .map_err(|_| ZipError::BadArchive)?;
            if n < 22 {
                break;
            }
            let mut i = n - 22;
            loop {
                if tail_win[i..i + 4] == [0x50, 0x4B, 0x05, 0x06] {
                    let comment_len = u16::from_le_bytes([tail_win[i + 20], tail_win[i + 21]]) as u64;
                    if win_start + i as u64 + 22 + comment_len == file_size {
                        let total_entries =
                            u16::from_le_bytes([tail_win[i + 10], tail_win[i + 11]]) as usize;
                        let cd_size = u32::from_le_bytes(
                            tail_win[i + 12..i + 16].try_into().unwrap(),
                        ) as usize;
                        let cd_offset = u32::from_le_bytes(
                            tail_win[i + 16..i + 20].try_into().unwrap(),
                        ) as usize;
                        eocd_hdr = Some((total_entries, cd_size, cd_offset));
                        break;
                    }
                }
                if i == 0 {
                    break;
                }
                i -= 1;
            }
            if eocd_hdr.is_some() || win_start == earliest {
                break;
            }
            end = win_start + 21;
        }

        let (total_entries, cd_size, cd_offset) = eocd_hdr.ok_or(ZipError::BadArchive)?;
        if total_entries > MAX_STORAGE_ENTRIES
            || cd_size > MAX_STORAGE_CD_BYTES
            || (cd_offset as u64)
                .checked_add(cd_size as u64)
                .is_none_or(|end| end > file_size)
        {
            return Err(ZipError::SecurityViolation);
        }

        let mut entries = Vec::new();
        let mut text_uncomp_bytes = 0u64;
        stream_cd_records(
            storage,
            path,
            file_size,
            cd_offset as u64,
            cd_size,
            total_entries,
            |name, entry_ref| {
                if is_text_book_entry(name) {
                    text_uncomp_bytes = text_uncomp_bytes.saturating_add(entry_ref.uncomp_size as u64);
                }
                let essential = is_essential_zip_meta(name);
                if essential || (entries.len() < MAX_KEPT_ENTRIES && !is_binary_asset_entry(name)) {
                    entries.push(ZipEntryMeta {
                        name: String::from(name),
                        method: entry_ref.method,
                        comp_size: entry_ref.comp_size as usize,
                        uncomp_size: entry_ref.uncomp_size as usize,
                        local_header_offset: entry_ref.local_header_offset as usize,
                    });
                }
                Ok(())
            },
        )?;

        Ok(Self {
            entries,
            file_size,
            cd_offset: cd_offset as u64,
            cd_size,
            total_entries,
            text_uncomp_bytes,
        })
    }

    pub fn file_size(&self) -> u64 {
        self.file_size
    }

    /// Re-stream the Central Directory without keeping `String` names in heap, invoking `on_entry` for each record.
    pub fn scan_cd_entries<S: StorageBackend, F>(
        &self,
        storage: &S,
        path: &str,
        on_entry: F,
    ) -> Result<(), ZipError>
    where
        F: FnMut(&str, ZipEntryRef) -> Result<(), ZipError>,
    {
        stream_cd_records(
            storage,
            path,
            self.file_size,
            self.cd_offset,
            self.cd_size,
            self.total_entries,
            on_entry,
        )
    }

    pub fn text_uncomp_bytes(&self) -> u64 {
        self.text_uncomp_bytes
    }

    pub fn entries(&self) -> &[ZipEntryMeta] {
        &self.entries
    }

    pub fn find_entry(&self, name: &str) -> Option<&ZipEntryMeta> {
        let clean = name.trim_start_matches("./");
        self.entries
            .iter()
            .find(|e| e.name == clean || e.name == name)
            .or_else(|| {
                self.entries
                    .iter()
                    .find(|e| e.name.eq_ignore_ascii_case(clean) || e.name.eq_ignore_ascii_case(name))
            })
    }

    pub fn open_entry_ref_stream<'a, S: StorageBackend>(
        file_size: u64,
        storage: &S,
        path: &str,
        meta: ZipEntryRef,
        max_uncomp: usize,
        scratch: Option<&'a mut ZipInflateScratch>,
    ) -> Result<ZipEntryStream<'a>, ZipError> {
        let off = meta.local_header_offset as u64;
        if off + 30 > file_size {
            return Err(ZipError::BadArchive);
        }
        let mut lfh = [0u8; 30];
        if storage.read_range(path, off, &mut lfh).map_err(|_| ZipError::BadArchive)? < 30 {
            return Err(ZipError::BadArchive);
        }
        if lfh[0..4] != [0x50, 0x4B, 0x03, 0x04] {
            return Err(ZipError::BadArchive);
        }
        let nlen = u16::from_le_bytes([lfh[26], lfh[27]]) as u64;
        let xlen = u16::from_le_bytes([lfh[28], lfh[29]]) as u64;
        let cur_off = off + 30 + nlen + xlen;
        let comp_end = cur_off
            .checked_add(meta.comp_size as u64)
            .ok_or(ZipError::BadArchive)?;
        if comp_end > file_size {
            return Err(ZipError::BadArchive);
        }
        let inflate_state = match meta.method {
            0 => StreamInflate::None,
            8 => match scratch {
                Some(s) => {
                    s.reset();
                    StreamInflate::Borrowed(s)
                }
                None => StreamInflate::Owned(InflateState::new_boxed(DataFormat::Raw)),
            },
            _ => return Err(ZipError::UnsupportedMethod),
        };
        Ok(ZipEntryStream {
            path: String::from(path),
            method: meta.method,
            cur_off,
            comp_rem: meta.comp_size as usize,
            produced: 0,
            max_uncomp,
            in_buf: vec![0u8; 2048],
            in_pos: 0,
            in_len: 0,
            inflate_state,
            finished: false,
        })
    }

    pub fn open_stream<S: StorageBackend>(
        &self,
        storage: &S,
        path: &str,
        meta: &ZipEntryMeta,
        max_uncomp: usize,
    ) -> Result<ZipEntryStream<'static>, ZipError> {
        Self::open_entry_ref_stream(
            self.file_size,
            storage,
            path,
            ZipEntryRef::from(meta),
            max_uncomp,
            None,
        )
    }

    pub fn read_entry_prefix<S: StorageBackend>(
        &self,
        storage: &S,
        path: &str,
        meta: &ZipEntryMeta,
        max_bytes: usize,
    ) -> Result<Vec<u8>, ZipError> {
        self.read_entry_prefix_with_scratch(storage, path, meta, max_bytes, None)
    }

    pub fn read_entry_prefix_with_scratch<S: StorageBackend>(
        &self,
        storage: &S,
        path: &str,
        meta: &ZipEntryMeta,
        max_bytes: usize,
        scratch: Option<&mut ZipInflateScratch>,
    ) -> Result<Vec<u8>, ZipError> {
        let mut stream = Self::open_entry_ref_stream(
            self.file_size,
            storage,
            path,
            ZipEntryRef::from(meta),
            max_bytes,
            scratch,
        )?;
        let cap = meta.uncomp_size.min(max_bytes).min(8192);
        let mut out = Vec::with_capacity(cap);
        let mut chunk = [0u8; 1024];
        while out.len() < max_bytes {
            let want = chunk.len().min(max_bytes - out.len());
            let n = stream.next_chunk(storage, &mut chunk[..want])?;
            if n == 0 {
                break;
            }
            out.extend_from_slice(&chunk[..n]);
        }
        Ok(out)
    }
}

enum StreamInflate<'a> {
    None,
    Owned(Box<InflateState>),
    Borrowed(&'a mut ZipInflateScratch),
}

/// Incremental pull-based reader for a single `StorageZip` entry (`< 40 KB` heap).
pub struct ZipEntryStream<'a> {
    path: String,
    method: u16,
    cur_off: u64,
    comp_rem: usize,
    produced: usize,
    max_uncomp: usize,
    in_buf: Vec<u8>,
    in_pos: usize,
    in_len: usize,
    inflate_state: StreamInflate<'a>,
    finished: bool,
}

impl<'a> ZipEntryStream<'a> {
    fn refill_partial_in_buf<S: StorageBackend>(
        &mut self,
        storage: &S,
        stalled: &mut u8,
    ) -> Result<(), ZipError> {
        let rem = self.in_len - self.in_pos;
        if rem > 0 {
            self.in_buf.copy_within(self.in_pos..self.in_len, 0);
        }
        self.in_pos = 0;
        self.in_len = rem;
        let space = self.in_buf.len() - self.in_len;
        if space == 0 {
            return Err(ZipError::DecompressFailed);
        }
        let want = self.comp_rem.min(space);
        let n = storage
            .read_range(
                &self.path,
                self.cur_off,
                &mut self.in_buf[self.in_len..self.in_len + want],
            )
            .map_err(|_| ZipError::BadArchive)?;
        if n == 0 {
            return Err(ZipError::DecompressFailed);
        }
        self.in_len += n;
        self.cur_off += n as u64;
        self.comp_rem -= n;
        *stalled = stalled.saturating_add(1);
        if *stalled > 8 {
            return Err(ZipError::DecompressFailed);
        }
        Ok(())
    }

    pub fn next_chunk<S: StorageBackend>(
        &mut self,
        storage: &S,
        out_buf: &mut [u8],
    ) -> Result<usize, ZipError> {
        if self.finished || self.produced >= self.max_uncomp || out_buf.is_empty() {
            return Ok(0);
        }
        let max_out = out_buf.len().min(self.max_uncomp - self.produced);
        match self.method {
            0 => {
                if self.comp_rem == 0 {
                    self.finished = true;
                    return Ok(0);
                }
                let want = max_out.min(self.comp_rem);
                let n = storage
                    .read_range(&self.path, self.cur_off, &mut out_buf[..want])
                    .map_err(|_| ZipError::BadArchive)?;
                if n == 0 {
                    return Err(ZipError::BadArchive);
                }
                self.cur_off += n as u64;
                self.comp_rem -= n;
                self.produced += n;
                Ok(n)
            }
            8 => {
                let mut stalled = 0u8;
                loop {
                    if self.in_pos == self.in_len && self.comp_rem > 0 {
                        self.in_pos = 0;
                        let want = self.comp_rem.min(self.in_buf.len());
                        self.in_len = storage
                            .read_range(&self.path, self.cur_off, &mut self.in_buf[..want])
                            .map_err(|_| ZipError::BadArchive)?;
                        if self.in_len == 0 {
                            return Err(ZipError::DecompressFailed);
                        }
                        self.cur_off += self.in_len as u64;
                        self.comp_rem -= self.in_len;
                    }
                    let res = {
                        let state = match &mut self.inflate_state {
                            StreamInflate::Owned(b) => &mut **b,
                            StreamInflate::Borrowed(s) => &mut s.0,
                            StreamInflate::None => return Err(ZipError::DecompressFailed),
                        };
                        inflate(
                            state,
                            &self.in_buf[self.in_pos..self.in_len],
                            &mut out_buf[..max_out],
                            MZFlush::None,
                        )
                    };
                    self.in_pos += res.bytes_consumed;
                    match res.status {
                        Ok(MZStatus::StreamEnd) => {
                            self.finished = true;
                            self.produced += res.bytes_written;
                            return Ok(res.bytes_written);
                        }
                        Ok(_) => {
                            if res.bytes_written > 0 {
                                self.produced += res.bytes_written;
                                return Ok(res.bytes_written);
                            }
                            if res.bytes_consumed == 0 {
                                if self.comp_rem > 0 {
                                    self.refill_partial_in_buf(storage, &mut stalled)?;
                                    continue;
                                }
                                if self.in_pos >= self.in_len {
                                    self.finished = true;
                                    return Ok(0);
                                }
                                return Err(ZipError::DecompressFailed);
                            }
                            stalled = 0;
                        }
                        Err(MZError::Buf) => {
                            if res.bytes_written > 0 {
                                self.produced += res.bytes_written;
                                return Ok(res.bytes_written);
                            }
                            if self.comp_rem > 0 {
                                self.refill_partial_in_buf(storage, &mut stalled)?;
                                continue;
                            }
                            return Err(ZipError::DecompressFailed);
                        }
                        Err(_) => return Err(ZipError::DecompressFailed),
                    }
                }
            }
            _ => Err(ZipError::UnsupportedMethod),
        }
    }
}
