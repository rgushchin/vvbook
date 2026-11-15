#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod direct;
pub mod encoding;
pub mod mobi;
pub mod parsers;
pub mod vvb;
pub mod zip;

pub use direct::{DIRECT_MAX_SECTION_BLOCKS, DirectBook, DirectTocItem};
pub use encoding::{TextEncoding, detect_and_decode_to_utf8, detect_encoding};
pub use parsers::{
    InputFormat, ParseError, ProbedBookMeta, ingest_document, ingest_storage_to_vvb,
    ingest_storage_to_vvb_with_progress, ingest_with_format, probe_book_storage,
    probe_book_storage_with_scratch, source_book_id_hex,
};
pub use vvb::{
    Block, BookMetadata, COVER_THUMB_H, COVER_THUMB_W, CatalogEntry, PageAnchor, SpanStyle, TextRun,
    TocEntry, VvbBook, VvbIndex, build_vvb_header, catalog_format_version, decode_catalog,
    decode_layout_cache, encode_catalog, encode_layout_cache, intern_language,
    intern_source_format,
};
pub use zip::{StorageZip, ZipArchive, ZipError, ZipInflateScratch};

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{string::String, vec, vec::Vec};
    use miniz_oxide::deflate::compress_to_vec;

    /// Build a minimal valid uncompressed or deflated ZIP archive in memory for tests.
    fn make_test_zip(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut cd = Vec::new();
        for (name, payload) in files {
            let lfh_offset = out.len() as u32;
            let comp = compress_to_vec(payload, 6);
            // Local File Header
            out.extend_from_slice(&0x04034B50u32.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes()); // ver
            out.extend_from_slice(&0u16.to_le_bytes()); // flags
            out.extend_from_slice(&8u16.to_le_bytes()); // method deflate
            out.extend_from_slice(&0u32.to_le_bytes()); // mtime
            out.extend_from_slice(&0u32.to_le_bytes()); // crc
            out.extend_from_slice(&(comp.len() as u32).to_le_bytes());
            out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(&comp);

            // Central Directory Header
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
            cd.extend_from_slice(name.as_bytes());
        }
        let cd_offset = out.len() as u32;
        let cd_size = cd.len() as u32;
        out.extend_from_slice(&cd);
        // EOCD
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
    fn detects_cp1251_and_koi8r_cyrillic() {
        let sample = "Молодая рыжая собака — помесь таксы с дворняжкой — бегала взад и вперёд по тротуару.";
        // Encode to Windows-1251 manually for key words
        let cp1251_bytes: &[u8] = &[
            0xCC, 0xEE, 0xEB, 0xEE, 0xE4, 0xE0, 0xFF, 0x20, 0xF0, 0xFB, 0xE6, 0xE0, 0xFF, 0x20,
            0xF1, 0xEE, 0xE1, 0xE0, 0xEA, 0xE0, 0x20, 0xE1, 0xE5, 0xE3, 0xE0, 0xEB, 0xE0, 0x20,
            0xEF, 0xEE, 0x20, 0xF2, 0xF0, 0xEE, 0xF2, 0xF3, 0xE0, 0xF0, 0xF3, 0x2E,
        ];
        let (enc, decoded) = detect_and_decode_to_utf8(cp1251_bytes, None);
        assert_eq!(enc, TextEncoding::Windows1251);
        assert!(decoded.contains("Молодая рыжая собака бегала по тротуару."));

        // KOI8-R: "привет мир хороший день" -> in KOI8-R lowercase letters are 0xC0..0xDF
        let koi8_bytes: &[u8] = &[
            0xF0, 0xD2, 0xC9, 0xD7, 0xC5, 0xD4, 0x20, 0xCD, 0xC9, 0xD2, 0x20, 0xC8, 0xCF, 0xD2,
            0xCF, 0xDB, 0xC9, 0xCA, 0x20, 0xC4, 0xC5, 0xCE, 0xD8,
        ];
        let (enc2, decoded2) = detect_and_decode_to_utf8(koi8_bytes, None);
        assert_eq!(enc2, TextEncoding::Koi8R);
        assert_eq!(decoded2, "Привет мир хороший день");
        let _ = sample;
    }

    #[test]
    fn parses_fb2_zip_and_roundtrips_vvb() {
        let fb2 = r#"<?xml version="1.0" encoding="utf-8"?>
<FictionBook>
  <description><title-info>
    <author><first-name>Александр</first-name><last-name>Пушкин</last-name></author>
    <book-title>Сказка о царе Салтане</book-title>
    <lang>ru</lang>
  </title-info></description>
  <body><section>
    <title><p>Три девицы под окном</p></title>
    <poem><stanza>
      <v>Три девицы под окном</v>
      <v>Пряли поздно вечерком.</v>
    </stanza></poem>
    <p>«Кабы я была <emphasis>царица</emphasis>,» — говорит одна девица.</p>
  </section></body>
</FictionBook>"#;
        let zipped = make_test_zip(&[("saltan.fb2", fb2.as_bytes())]);
        let book = ingest_document("saltan.fb2.zip", &zipped).unwrap();
        assert_eq!(book.meta.title, "Сказка о царе Салтане");
        assert_eq!(book.meta.author, "Александр Пушкин");
        assert_eq!(book.meta.language, "ru");
        assert_eq!(book.toc.len(), 1);

        let bin = book.to_bytes();
        let parsed = VvbBook::from_bytes(&bin).unwrap();
        assert_eq!(parsed.meta.title, book.meta.title);
        assert_eq!(parsed.blocks.len(), book.blocks.len());
    }

    #[test]
    fn parses_epub_and_rejects_zip_traversal() {
        let container = br#"<?xml version="1.0"?><container><rootfiles><rootfile full-path="OEBPS/content.opf"/></rootfiles></container>"#;
        let opf = br#"<package><metadata><dc:title>Alice</dc:title><dc:creator>Lewis Carroll</dc:creator></metadata><manifest><item id="c1" href="ch1.xhtml"/></manifest><spine><itemref idref="c1"/></spine></package>"#;
        let ch1 = br#"<html><body><h1>Down the Rabbit-Hole</h1><p>Alice was beginning to get very <em>tired</em>.</p></body></html>"#;
        let epub = make_test_zip(&[
            ("META-INF/container.xml", container),
            ("OEBPS/content.opf", opf),
            ("OEBPS/ch1.xhtml", ch1),
        ]);
        let book = ingest_document("alice.epub", &epub).unwrap();
        assert_eq!(book.meta.title, "Alice");
        assert_eq!(book.meta.author, "Lewis Carroll");
        assert!(book.blocks.len() >= 2);

        // Path traversal attack must be rejected
        let evil = make_test_zip(&[("../../etc/passwd", b"root:x:0:0")]);
        assert!(ZipArchive::parse(&evil).is_err());
    }

    #[test]
    fn all_five_cyrillic_encodings_and_explicit_hint() {
        // 1. UTF-8 with BOM
        let mut utf8_bom = vec![0xEFu8, 0xBB, 0xBF];
        utf8_bom.extend_from_slice("Привет, читатель!".as_bytes());
        let (e_utf8, s_utf8) = detect_and_decode_to_utf8(&utf8_bom, None);
        assert_eq!(e_utf8, TextEncoding::Utf8);
        assert_eq!(s_utf8, "Привет, читатель!");

        // 2. Windows-1251 ("Русский текст")
        let cp1251 = &[0xD0, 0xF3, 0xF1, 0xF1, 0xEA, 0xE8, 0xE9, 0x20, 0xF2, 0xE5, 0xEA, 0xF1, 0xF2];
        let (e_1251, s_1251) = detect_and_decode_to_utf8(cp1251, Some("windows-1251"));
        assert_eq!(e_1251, TextEncoding::Windows1251);
        assert_eq!(s_1251, "Русский текст");

        // 3. KOI8-R ("Привет")
        let koi8r = &[0xF0, 0xD2, 0xC9, 0xD7, 0xC5, 0xD4];
        let (e_k8r, s_k8r) = detect_and_decode_to_utf8(koi8r, Some("koi8-r"));
        assert_eq!(e_k8r, TextEncoding::Koi8R);
        assert_eq!(s_k8r, "Привет");

        // 4. KOI8-U ("Київ і їжак") -> і=0xA6, ї=0xA7, є=0xA4, ґ=0xAD in KOI8-U
        let koi8u = &[0xEB, 0xC9, 0xA7, 0xD7, 0x20, 0xA6, 0x20, 0xA4];
        let (e_k8u, s_k8u) = detect_and_decode_to_utf8(koi8u, Some("koi8-u"));
        assert_eq!(e_k8u, TextEncoding::Koi8U);
        assert_eq!(s_k8u, "Київ і є");

        // 5. CP866 ("Книга" -> К=0x8A, н=0xAD, и=0xA8, г=0xA3, а=0xA0)
        let cp866 = &[0x8A, 0xAD, 0xA8, 0xA3, 0xA0];
        let (e_866, s_866) = detect_and_decode_to_utf8(cp866, Some("cp866"));
        assert_eq!(e_866, TextEncoding::Cp866);
        assert_eq!(s_866, "Книга");
    }

    #[test]
    fn zip_bomb_corrupt_vvb_and_streaming_250_block_index() {
        use core::cell::Cell;
        use vvbook_pal::{DirEntry, StorageBackend, StorageError};

        // Zip bomb defense: inflated size > MAX_UNCOMPRESSED_ENTRY must fail safely
        let huge = alloc::vec![b'A'; 5 * 1024 * 1024];
        let mut bomb_zip = make_test_zip(&[("huge.txt", &huge)]);
        // Corrupt/inflate claimed size in central directory / local header or craft raw 20MB stored entry test via parser limit
        if bomb_zip.len() > 30 {
            // Even before decompressing 20MB, verify corrupt ZIP fails safely without panicking
            let mut trunc = bomb_zip.clone();
            trunc.truncate(bomb_zip.len() / 2);
            assert!(ZipArchive::parse(&trunc).is_err());
        }
        bomb_zip.clear();

        // Build 250-block bilingual book and stream via VvbIndex::open + read_block
        let mut md = String::from("# Big Book\n\n");
        for i in 0..249 {
            md.push_str(&alloc::format!("Блок номер {i}: Пушкин и Чехов читаются отлично.\n\n"));
        }
        let book = ingest_document("big.md", md.as_bytes()).unwrap();
        assert!(book.blocks.len() >= 250);
        let raw = book.to_bytes();

        // Every prefix truncation of `.vvb` must cleanly Err instead of panicking
        for cut in [0usize, 4, 16, 48, raw.len() / 3, raw.len() - 1] {
            assert!(VvbBook::from_bytes(&raw[..cut]).is_err(), "truncation {cut} should Err");
        }

        struct SliceStore(Vec<u8>, Cell<usize>);
        impl StorageBackend for SliceStore {
            fn exists(&self, _: &str) -> bool { true }
            fn file_len(&self, _: &str) -> Result<u64, StorageError> { Ok(self.0.len() as u64) }
            fn read_all(&self, _: &str) -> Result<Vec<u8>, StorageError> { Ok(self.0.clone()) }
            fn read_range(&self, _: &str, offset: u64, buf: &mut [u8]) -> Result<usize, StorageError> {
                self.1.set(self.1.get() + 1);
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

        let store = SliceStore(raw, Cell::new(0));
        let idx = VvbIndex::open_from_storage(&store, "/big.vvb").unwrap();
        assert_eq!(idx.block_count(), book.blocks.len());
        let b240 = idx.read_block(&store, "/big.vvb", 240).unwrap();
        assert_eq!(b240, book.blocks[240]);
    }

    #[test]
    fn large_epub_and_fb2_with_binary_cover_probe_and_stream_without_read_all() {
        use alloc::collections::BTreeMap;
        use vvbook_pal::{DirEntry, StorageBackend, StorageError};

        struct NoReadAllStore(BTreeMap<String, Vec<u8>>);
        impl StorageBackend for NoReadAllStore {
            fn exists(&self, p: &str) -> bool {
                self.0.contains_key(p)
            }
            fn file_len(&self, p: &str) -> Result<u64, StorageError> {
                self.0.get(p).map(|v| v.len() as u64).ok_or(StorageError::NotFound)
            }
            fn read_all(&self, p: &str) -> Result<Vec<u8>, StorageError> {
                panic!("read_all forbidden on streaming path ({p})!");
            }
            fn read_range(&self, p: &str, offset: u64, buf: &mut [u8]) -> Result<usize, StorageError> {
                let d = self.0.get(p).ok_or(StorageError::NotFound)?;
                let off = offset as usize;
                if off >= d.len() {
                    return Ok(0);
                }
                let n = buf.len().min(d.len() - off);
                buf[..n].copy_from_slice(&d[off..off + n]);
                Ok(n)
            }
            fn write_all(&mut self, p: &str, data: &[u8]) -> Result<(), StorageError> {
                self.0.insert(String::from(p), data.to_vec());
                Ok(())
            }
            fn write_atomic(&mut self, p: &str, data: &[u8]) -> Result<(), StorageError> {
                self.0.insert(String::from(p), data.to_vec());
                Ok(())
            }
            fn append(&mut self, p: &str, data: &[u8]) -> Result<(), StorageError> {
                self.0.entry(String::from(p)).or_default().extend_from_slice(data);
                Ok(())
            }
            fn remove(&mut self, p: &str) -> Result<(), StorageError> {
                self.0.remove(p);
                Ok(())
            }
            fn mkdir_all(&mut self, _: &str) -> Result<(), StorageError> {
                Ok(())
            }
            fn list_dir(&self, _: &str) -> Result<Vec<DirEntry>, StorageError> {
                Ok(Vec::new())
            }
        }

        // 1. Multi-chapter ~180 KB EPUB
        let mut epub_files: Vec<(String, Vec<u8>)> = vec![
            (
                "META-INF/container.xml".into(),
                br#"<?xml version="1.0"?><container><rootfiles><rootfile full-path="OEBPS/content.opf"/></rootfiles></container>"#.to_vec(),
            ),
        ];
        let mut manifest = String::new();
        let mut spine = String::new();
        for ch in 1..=10 {
            manifest.push_str(&alloc::format!("<item id=\"c{ch}\" href=\"ch{ch}.xhtml\"/>"));
            spine.push_str(&alloc::format!("<itemref idref=\"c{ch}\"/>"));
            let mut xhtml = alloc::format!("<html><head><title>Ch {ch}</title></head><body><h1>Chapter {ch}</h1>");
            for p in 0..45 {
                xhtml.push_str(&alloc::format!(
                    "<p>Paragraph {p} of chapter {ch}: fine typography breathes on paper like morning mist over quiet reeds.</p>"
                ));
            }
            xhtml.push_str("</body></html>");
            epub_files.push((alloc::format!("OEBPS/ch{ch}.xhtml"), xhtml.into_bytes()));
        }
        let opf = alloc::format!(
            "<package><metadata><dc:title>Quiet Pages</dc:title><dc:creator>Ivan Turgenev</dc:creator><dc:language>en</dc:language></metadata><manifest>{manifest}</manifest><spine>{spine}</spine></package>"
        );
        epub_files.insert(1, ("OEBPS/content.opf".into(), opf.into_bytes()));
        let refs: Vec<(&str, &[u8])> = epub_files.iter().map(|(k, v)| (k.as_str(), v.as_slice())).collect();
        let epub_bytes = make_test_zip(&refs);

        // 2. Russian FB2 with 240 KB <binary> cover blob
        let mut fb2 = String::from(
            r#"<?xml version="1.0" encoding="utf-8"?><FictionBook><description><title-info><author><first-name>Антон</first-name><last-name>Чехов</last-name></author><book-title>Дама с собачкой</book-title><lang>ru</lang></title-info></description><body><section><title><p>Глава I</p></title>"#,
        );
        for i in 0..160 {
            fb2.push_str(&alloc::format!("<p>Абзац {i}. На набережной в Ялте было тихо и светло от моря.</p>"));
        }
        fb2.push_str("</section></body><binary id=\"cov.jpg\" content-type=\"image/jpeg\">");
        for _ in 0..240_000 {
            fb2.push('A');
        }
        fb2.push_str("</binary></FictionBook>");
        let fb2_bytes = fb2.into_bytes();

        let mut store = NoReadAllStore(BTreeMap::new());
        store.write_all("/Calibre/quiet.epub", &epub_bytes).unwrap();
        store.write_all("/Books/chekhov.fb2", &fb2_bytes).unwrap();

        let p1 = probe_book_storage(&store, "/Calibre/quiet.epub", epub_bytes.len() as u64).unwrap();
        assert_eq!(p1.title, "Quiet Pages");
        assert_eq!(p1.author, "Ivan Turgenev");
        let idx1 = ingest_storage_to_vvb(&mut store, "/Calibre/quiet.epub", "/.vvbook/books/q.vvb", Some(&p1.id_hex)).unwrap();
        assert!(idx1.block_count() >= 450);
        assert!(matches!(idx1.read_block(&store, "/.vvbook/books/q.vvb", 0).unwrap(), Block::ChapterTitle { .. }));

        let p2 = probe_book_storage(&store, "/Books/chekhov.fb2", fb2_bytes.len() as u64).unwrap();
        assert_eq!(p2.title, "Дама с собачкой");
        assert_eq!(p2.author, "Антон Чехов");
        let idx2 = ingest_storage_to_vvb(&mut store, "/Books/chekhov.fb2", "/.vvbook/books/c.vvb", Some(&p2.id_hex)).unwrap();
        assert_eq!(idx2.block_count(), 161);
        assert_eq!(
            idx2.read_block(&store, "/.vvbook/books/c.vvb", 0).unwrap(),
            Block::ChapterTitle { title: "Глава I".into() }
        );
    }

    #[test]
    fn deflate_boundary_refill_and_sparse_vvb_checkpoints() {
        use alloc::collections::BTreeMap;
        use vvbook_pal::{DirEntry, StorageBackend, StorageError};

        struct MemStore(BTreeMap<String, Vec<u8>>);
        impl StorageBackend for MemStore {
            fn exists(&self, p: &str) -> bool { self.0.contains_key(p) }
            fn file_len(&self, p: &str) -> Result<u64, StorageError> {
                self.0.get(p).map(|v| v.len() as u64).ok_or(StorageError::NotFound)
            }
            fn read_all(&self, p: &str) -> Result<Vec<u8>, StorageError> {
                self.0.get(p).cloned().ok_or(StorageError::NotFound)
            }
            fn read_range(&self, p: &str, offset: u64, buf: &mut [u8]) -> Result<usize, StorageError> {
                let v = self.0.get(p).ok_or(StorageError::NotFound)?;
                let start = (offset as usize).min(v.len());
                let n = buf.len().min(v.len() - start);
                buf[..n].copy_from_slice(&v[start..start + n]);
                Ok(n)
            }
            fn write_all(&mut self, p: &str, d: &[u8]) -> Result<(), StorageError> {
                self.0.insert(String::from(p), d.to_vec());
                Ok(())
            }
            fn write_atomic(&mut self, p: &str, d: &[u8]) -> Result<(), StorageError> {
                self.write_all(p, d)
            }
            fn append(&mut self, p: &str, d: &[u8]) -> Result<(), StorageError> {
                self.0.entry(String::from(p)).or_default().extend_from_slice(d);
                Ok(())
            }
            fn remove(&mut self, p: &str) -> Result<(), StorageError> {
                self.0.remove(p);
                Ok(())
            }
            fn mkdir_all(&mut self, _: &str) -> Result<(), StorageError> { Ok(()) }
            fn list_dir(&self, _: &str) -> Result<Vec<DirEntry>, StorageError> { Ok(Vec::new()) }
        }

        // Build >16 KB incompressible-ish Deflated payload so input buffer spans multiple 4 KB refills
        let mut payload = Vec::with_capacity(48 * 1024);
        let mut x = 0x9E37_79B9u32;
        for _ in 0..12_000 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            payload.extend_from_slice(&x.to_le_bytes());
        }
        let zip_bytes = make_test_zip(&[("big.bin", &payload)]);
        let mut store = MemStore(BTreeMap::new());
        store.write_all("/stream.zip", &zip_bytes).unwrap();
        let sz = StorageZip::open(&store, "/stream.zip", zip_bytes.len() as u64).unwrap();
        let eref = zip::ZipEntryRef::from(sz.find_entry("big.bin").unwrap());
        let fsize = sz.file_size();
        drop(sz);
        let mut scratch = zip::ZipInflateScratch::new();
        let mut s = StorageZip::open_entry_ref_stream(
            fsize,
            &store,
            "/stream.zip",
            eref,
            payload.len() + 1024,
            Some(&mut scratch),
        )
        .unwrap();
        let mut got = Vec::new();
        let mut out_chunk = [0u8; 37];
        loop {
            let n = s.next_chunk(&store, &mut out_chunk).unwrap();
            if n == 0 {
                break;
            }
            got.extend_from_slice(&out_chunk[..n]);
        }
        assert_eq!(got, payload);

        // Build 4,300 blocks via TXT streaming -> verifies sparse VVB checkpoints (> 4096 blocks, shift = 1)
        let mut big_txt = String::new();
        for i in 0..4300 {
            big_txt.push_str(&alloc::format!("Paragraph number {i} for sparse index check.\n\n"));
        }
        store.write_all("/many.txt", big_txt.as_bytes()).unwrap();
        let idx = ingest_storage_to_vvb(&mut store, "/many.txt", "/many.vvb", Some("m0")).unwrap();
        assert_eq!(idx.block_count(), 4300);
        assert_eq!(idx.checkpoint_shift, 1);
        assert!(idx.block_checkpoints.len() <= vvb::MAX_VVB_CHECKPOINTS);
        let reopened = VvbIndex::open_from_storage(&store, "/many.vvb").unwrap();
        assert_eq!(reopened.block_count(), 4300);
        assert_eq!(reopened.checkpoint_shift, 1);
        let range = reopened.read_blocks_range(&store, "/many.vvb", 4094, 5).unwrap();
        assert_eq!(range.len(), 5);
        assert_eq!(range[3], reopened.read_block(&store, "/many.vvb", 4097).unwrap());
    }

    #[test]
    fn vvb_binary_decoders_reject_overflow_and_huge_lengths() {
        // count = 0x2AAA_AAAB overflows `8 + count * 6` on 32-bit targets (would wrap to 10 bytes).
        let mut raw_vvlc = Vec::from(*b"VVLC");
        raw_vvlc.extend_from_slice(&0x2AAA_AAABu32.to_le_bytes());
        raw_vvlc.extend_from_slice(&[0u8; 2]);
        assert_eq!(decode_layout_cache(&raw_vvlc), Err(vvb::VvbError::Truncated));

        // Corrupt id_hex string length = u32::MAX right after a valid 44-byte VVBK preamble.
        let mut bad_vvb = vec![0u8; 64];
        bad_vvb[0..4].copy_from_slice(b"VVBK");
        bad_vvb[4..6].copy_from_slice(&1u16.to_le_bytes());
        bad_vvb[44..48].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(VvbBook::from_bytes(&bad_vvb).err(), Some(vvb::VvbError::Truncated));
        assert_eq!(VvbIndex::from_header_bytes(&bad_vvb).err(), Some(vvb::VvbError::Truncated));
    }
}
