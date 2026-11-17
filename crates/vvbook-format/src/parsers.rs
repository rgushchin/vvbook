extern crate alloc;

use crate::encoding::{TextEncoding, decode_with, detect_and_decode_to_utf8, detect_encoding};
use crate::vvb::{
    Block, BookMetadata, COVER_THUMB_H, COVER_THUMB_W, MAX_VVB_CHECKPOINTS, SpanStyle, TextRun,
    TocEntry, VVBT_FOOTER_MAGIC, VVB_PREAMBLE_LEN, VvbBook, VvbIndex,
    build_vvb_sparse_trailer_preamble, encode_block, encode_vvb_meta_and_toc,
};
use crate::zip::{StorageZip, ZipArchive, ZipEntryMeta, ZipEntryRef, ZipInflateScratch};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use vvbook_pal::StorageBackend;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputFormat {
    Vvb,
    Fb2,
    Fb2Zip,
    Epub,
    Mobi,
    Markdown,
    Html,
    Txt,
}

impl InputFormat {
    pub fn from_filename(name: &str) -> Option<Self> {
        let lower = name.to_ascii_lowercase();
        if lower.ends_with(".vvb") {
            Some(Self::Vvb)
        } else if lower.ends_with(".fb2.zip") || lower.ends_with(".fbz") {
            Some(Self::Fb2Zip)
        } else if lower.ends_with(".fb2") {
            Some(Self::Fb2)
        } else if lower.ends_with(".epub") {
            Some(Self::Epub)
        } else if lower.ends_with(".mobi")
            || lower.ends_with(".prc")
            || lower.ends_with(".azw")
            || lower.ends_with(".azw3")
        {
            Some(Self::Mobi)
        } else if lower.ends_with(".md") || lower.ends_with(".markdown") {
            Some(Self::Markdown)
        } else if lower.ends_with(".html") || lower.ends_with(".htm") || lower.ends_with(".xhtml") {
            Some(Self::Html)
        } else if lower.ends_with(".txt") || lower.ends_with(".text") {
            Some(Self::Txt)
        } else {
            None
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Vvb => "VVB",
            Self::Fb2 => "FB2",
            Self::Fb2Zip => "FB2.ZIP",
            Self::Epub => "EPUB",
            Self::Mobi => "MOBI",
            Self::Markdown => "MD",
            Self::Html => "HTML",
            Self::Txt => "TXT",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    UnsupportedFormat,
    InvalidArchive,
    MalformedDocument,
    EmptyDocument,
}

pub fn ingest_document(filename: &str, raw: &[u8]) -> Result<VvbBook, ParseError> {
    let fmt = InputFormat::from_filename(filename).ok_or(ParseError::UnsupportedFormat)?;
    ingest_with_format(filename, fmt, raw)
}

pub fn ingest_with_format(filename: &str, fmt: InputFormat, raw: &[u8]) -> Result<VvbBook, ParseError> {
    let mut book = match fmt {
        InputFormat::Vvb => return VvbBook::from_bytes(raw).map_err(|_| ParseError::MalformedDocument),
        InputFormat::Fb2 => parse_fb2(raw, filename, "FB2")?,
        InputFormat::Fb2Zip => {
            let zip = ZipArchive::parse(raw).map_err(|_| ParseError::InvalidArchive)?;
            let entry = zip
                .entries()
                .iter()
                .find(|e| e.name.to_ascii_lowercase().ends_with(".fb2"))
                .or_else(|| zip.entries().first())
                .ok_or(ParseError::MalformedDocument)?
                .clone();
            let xml = zip.read_meta(&entry).map_err(|_| ParseError::InvalidArchive)?;
            parse_fb2(&xml, filename, "FB2.ZIP")?
        }
        InputFormat::Epub => parse_epub(raw, filename)?,
        InputFormat::Mobi => crate::mobi::parse_mobi_in_memory(raw, filename)?,
        InputFormat::Markdown => {
            let (_, text) = detect_and_decode_to_utf8(raw, None);
            parse_markdown(&text, filename)
        }
        InputFormat::Html => {
            let (_, text) = detect_and_decode_to_utf8(raw, None);
            parse_html(&text, filename)
        }
        InputFormat::Txt => {
            let (_, text) = detect_and_decode_to_utf8(raw, None);
            parse_plain_text(&text, filename)
        }
    };

    if book.blocks.is_empty() {
        return Err(ParseError::EmptyDocument);
    }

    // Compute stable 64-bit hex ID from BLAKE3(content)
    let digest = *blake3::hash(raw).as_bytes();
    book.content_blake3 = digest;
    book.meta.id_hex = encode_short_id(&digest[..4]);
    book.meta.word_count = count_words(&book.blocks);

    // Ensure TOC has at least one entry
    if book.toc.is_empty() {
        for (idx, b) in book.blocks.iter().enumerate() {
            if let Block::ChapterTitle { title } = b {
                book.toc.push(TocEntry {
                    title: title.clone(),
                    block_index: idx as u32,
                });
            }
        }
        if book.toc.is_empty() {
            book.toc.push(TocEntry {
                title: book.meta.title.clone(),
                block_index: 0,
            });
        }
    }

    // Mark first paragraph of every chapter for optional Drop Cap
    let mut after_chapter = false;
    for b in &mut book.blocks {
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
            Block::VerseLine { .. } => {
                after_chapter = false;
            }
            _ => {}
        }
    }

    // Generate clean editorial 2-bpp cover if none provided
    if book.meta.cover_2bpp.is_empty() {
        book.meta.cover_w = COVER_THUMB_W;
        book.meta.cover_h = COVER_THUMB_H;
        book.meta.cover_2bpp = vec![0xFFu8; (COVER_THUMB_W as usize * COVER_THUMB_H as usize) / 4];
    }

    Ok(book)
}

fn encode_short_id(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0F) as usize] as char);
    }
    s
}

pub(crate) fn title_from_filename(filename: &str) -> String {
    let base = filename.rsplit(['/', '\\']).next().unwrap_or(filename);
    let trimmed = base
        .strip_suffix(".fb2.zip")
        .or_else(|| base.strip_suffix(".FB2.ZIP"))
        .unwrap_or_else(|| match base.rfind('.') {
            Some(idx) if idx > 0 => &base[..idx],
            _ => base,
        });
    trimmed.replace(['_', '-'], " ")
}

fn count_words(blocks: &[Block]) -> u32 {
    let mut words = 0u32;
    for b in blocks {
        match b {
            Block::ChapterTitle { title } | Block::Heading { text: title, .. } | Block::VerseLine { text: title } => {
                words += title.split_whitespace().count() as u32;
            }
            Block::Paragraph { runs, .. } | Block::BlockQuote { runs } | Block::ListItem { runs, .. } => {
                for r in runs {
                    words += r.text.split_whitespace().count() as u32;
                }
            }
            Block::CodeBlock { text } => words += text.split_whitespace().count() as u32,
            Block::SceneBreak => {}
        }
    }
    words
}

pub(crate) fn has_cyrillic(s: &str) -> bool {
    s.chars().any(|c| matches!(c, '\u{0400}'..='\u{04FF}'))
}

pub fn upgrade_typography(raw: &str, prefer_cyrillic: bool) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    let mut prev_space = true;
    let mut quote_open = true;
    let mut cur = chars.next();
    let mut next1 = chars.next();
    let mut next2 = chars.next();
    while let Some(c) = cur {
        if c == '-' && next1 == Some('-') {
            out.push('—');
            prev_space = false;
            if next2 == Some('-') {
                cur = chars.next();
                next1 = chars.next();
                next2 = chars.next();
            } else {
                cur = next2;
                next1 = chars.next();
                next2 = chars.next();
            }
            continue;
        }
        if c == '.' && next1 == Some('.') && next2 == Some('.') {
            out.push('…');
            prev_space = false;
            cur = chars.next();
            next1 = chars.next();
            next2 = chars.next();
            continue;
        }
        if c == '"' {
            if prefer_cyrillic {
                if prev_space || quote_open {
                    out.push('«');
                    quote_open = false;
                } else {
                    out.push('»');
                    quote_open = true;
                }
            } else if prev_space || quote_open {
                out.push('“');
                quote_open = false;
            } else {
                out.push('”');
                quote_open = true;
            }
            prev_space = false;
            cur = next1;
            next1 = next2;
            next2 = chars.next();
            continue;
        }
        out.push(c);
        prev_space = c.is_whitespace() || matches!(c, '(' | '[' | '—');
        cur = next1;
        next1 = next2;
        next2 = chars.next();
    }
    out
}

pub fn decode_xml_entities(input: &str) -> String {
    if !input.contains('&') {
        return String::from(input);
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        if let Some(semi) = after.find(';')
            && semi <= 12 {
                let ent = &after[..semi];
                let ch = match ent {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    "nbsp" => Some('\u{00A0}'),
                    "laquo" => Some('«'),
                    "raquo" => Some('»'),
                    "bdquo" => Some('„'),
                    "ldquo" => Some('“'),
                    "rdquo" => Some('”'),
                    "lsquo" => Some('‘'),
                    "rsquo" => Some('’'),
                    "mdash" => Some('—'),
                    "ndash" => Some('–'),
                    "hellip" => Some('…'),
                    "numero" => Some('№'),
                    "copy" => Some('©'),
                    "sect" => Some('§'),
                    "deg" => Some('°'),
                    _ if ent.starts_with("#x") || ent.starts_with("#X") => {
                        u32::from_str_radix(&ent[2..], 16).ok().and_then(char::from_u32)
                    }
                    _ if ent.starts_with('#') => ent[1..].parse::<u32>().ok().and_then(char::from_u32),
                    _ => None,
                };
                if let Some(c) = ch {
                    out.push(c);
                    rest = &after[semi + 1..];
                    continue;
                }
            }
        out.push('&');
        rest = after;
    }
    out.push_str(rest);
    out
}

fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    for ch in s.chars() {
        if ch == '\n' || ch == '\r' || ch == '\t' || ch == ' ' {
            if !prev_ws && !out.is_empty() {
                out.push(' ');
            }
            prev_ws = true;
        } else {
            out.push(ch);
            prev_ws = false;
        }
    }
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

fn push_run(runs: &mut Vec<TextRun>, style: SpanStyle, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = runs.last_mut()
        && last.style == style {
            last.text.push_str(text);
            return;
        }
    runs.push(TextRun {
        style,
        text: String::from(text),
    });
}

fn runs_trimmed(runs: Vec<TextRun>) -> Vec<TextRun> {
    let mut cleaned = Vec::new();
    for mut r in runs {
        if cleaned.is_empty() {
            r.text = String::from(r.text.trim_start());
        }
        if !r.text.is_empty() {
            cleaned.push(r);
        }
    }
    if let Some(last) = cleaned.last_mut() {
        last.text = String::from(last.text.trim_end());
        if last.text.is_empty() {
            cleaned.pop();
        }
    }
    cleaned
}

fn is_dialogue_runs(runs: &[TextRun]) -> bool {
    runs.first()
        .map(|r| {
            let t = r.text.trim_start();
            t.starts_with('—') || t.starts_with("-\u{00A0}") || t.starts_with("- ")
        })
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// 1. FictionBook 2 (.fb2 / .fb2.zip) parser
// ---------------------------------------------------------------------------

pub fn parse_fb2(raw: &[u8], filename: &str, fmt_label: &str) -> Result<VvbBook, ParseError> {
    let (_, xml) = detect_and_decode_to_utf8(raw, None);

    // Metadata from <description><title-info>
    let title = extract_xml_tag_text(&xml, "book-title")
        .map(|s| collapse_ws(&decode_xml_entities(&s)))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| title_from_filename(filename));

    let author = extract_fb2_author(&xml);
    let lang = extract_xml_tag_text(&xml, "lang")
        .map(|s| collapse_ws(&s))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            if has_cyrillic(&title) {
                String::from("ru")
            } else {
                String::from("en")
            }
        });

    let mut blocks = Vec::new();
    let mut toc = Vec::new();

    // Parse primary <body> (ignore <body name="notes"> or place after)
    let body_slice = find_primary_fb2_body(&xml).unwrap_or(&xml);
    parse_xml_flow(body_slice, &mut blocks, &mut toc, true);

    if blocks.is_empty() {
        return Err(ParseError::EmptyDocument);
    }

    Ok(VvbBook {
        meta: BookMetadata {
            id_hex: String::new(),
            title,
            author,
            language: lang,
            source_format: String::from(fmt_label),
            word_count: 0,
            cover_w: COVER_THUMB_W,
            cover_h: COVER_THUMB_H,
            cover_2bpp: Vec::new(),
        },
        toc,
        blocks,
        content_blake3: [0u8; 32],
    })
}

fn find_primary_fb2_body(xml: &str) -> Option<&str> {
    let mut pos = 0usize;
    while let Some(rel) = xml[pos..].find("<body") {
        let start = pos + rel;
        let tag_end = xml[start..].find('>')? + start;
        let open_tag = &xml[start..=tag_end];
        let is_notes = open_tag.contains("notes");
        if let Some(close_rel) = xml[tag_end + 1..].find("</body>") {
            let body_inner = &xml[tag_end + 1..tag_end + 1 + close_rel];
            if !is_notes {
                return Some(body_inner);
            }
            pos = tag_end + 1 + close_rel + 7;
        } else {
            break;
        }
    }
    None
}

fn extract_fb2_author(xml: &str) -> String {
    let scope = extract_xml_tag_raw(xml, "title-info").unwrap_or(xml);
    let author_xml = extract_xml_tag_raw(scope, "author").unwrap_or("");
    let first = extract_xml_tag_text(author_xml, "first-name").unwrap_or_default();
    let middle = extract_xml_tag_text(author_xml, "middle-name").unwrap_or_default();
    let last = extract_xml_tag_text(author_xml, "last-name").unwrap_or_default();
    let parts: Vec<&str> = [first.trim(), middle.trim(), last.trim()]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() {
        String::from("Unknown Author")
    } else {
        decode_xml_entities(&parts.join(" "))
    }
}

fn extract_xml_tag_raw<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}");
    let start = xml.find(&open)?;
    let gt = xml[start..].find('>')? + start;
    let close = format!("</{tag}>");
    let end = xml[gt + 1..].find(&close)? + gt + 1;
    Some(&xml[gt + 1..end])
}

pub(crate) fn extract_xml_tag_text(xml: &str, tag: &str) -> Option<String> {
    let raw = extract_xml_tag_raw(xml, tag)?;
    let mut out = String::new();
    let mut in_tag = false;
    for ch in raw.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    Some(collapse_ws(&decode_xml_entities(&out)))
}

// ---------------------------------------------------------------------------
// 2. Shared lightweight XML/XHTML SAX flow parser (FB2 + EPUB + HTML)
// ---------------------------------------------------------------------------

fn parse_xml_flow(src: &str, blocks: &mut Vec<Block>, toc: &mut Vec<TocEntry>, fb2_mode: bool) {
    let mut skip_depth = 0usize;
    parse_xml_flow_with_state(src, blocks, toc, fb2_mode, &mut skip_depth);
}

fn parse_xml_flow_with_state(
    src: &str,
    blocks: &mut Vec<Block>,
    toc: &mut Vec<TocEntry>,
    fb2_mode: bool,
    skip_depth_state: &mut usize,
) {
    let mut pos = 0usize;
    let mut in_title = false;
    let mut in_subtitle = false;
    let mut in_p = false;
    let mut in_v = false;
    let mut in_epigraph = false;
    let mut in_cite = false;
    let mut in_li = false;
    let mut in_pre = false;
    let mut skip_depth = *skip_depth_state;
    let mut bold_depth = 0usize;
    let mut italic_depth = 0usize;
    let mut code_depth = 0usize;
    let mut heading_level = 0u8;

    let mut cur_text = String::new();
    let mut cur_runs: Vec<TextRun> = Vec::new();
    let mut cur_runs_bytes = 0usize;

    let flush_p = |runs: &mut Vec<TextRun>,
                   run_bytes: &mut usize,
                   blocks: &mut Vec<Block>,
                   quote: bool,
                   li: bool| {
        *run_bytes = 0;
        let cleaned = runs_trimmed(core::mem::take(runs));
        if cleaned.is_empty() {
            return;
        }
        let total: usize = cleaned.iter().map(|r| r.text.len()).sum();
        let emit_one = |chunk: Vec<TextRun>, first: bool, out: &mut Vec<Block>| {
            if chunk.is_empty() {
                return;
            }
            if li {
                out.push(Block::ListItem {
                    bullet: if first { String::from("•") } else { String::new() },
                    runs: chunk,
                });
            } else if quote {
                out.push(Block::BlockQuote { runs: chunk });
            } else {
                let dlg = is_dialogue_runs(&chunk);
                out.push(Block::Paragraph {
                    runs: chunk,
                    first_of_chapter: false,
                    is_dialogue: dlg,
                });
            }
        };
        if total <= 1800 {
            emit_one(cleaned, true, blocks);
        } else {
            let mut first = true;
            split_runs_into(cleaned, 1800, |chunk| {
                emit_one(chunk, first, blocks);
                first = false;
            });
        }
    };

    while pos < src.len() {
        let rest = &src[pos..];
        if let Some(lt) = rest.find('<') {
            if lt > 0 && skip_depth == 0 {
                let raw_txt = &rest[..lt];
                let decoded_owned;
                let decoded: &str = if raw_txt.contains('&') {
                    decoded_owned = decode_xml_entities(raw_txt);
                    &decoded_owned
                } else {
                    raw_txt
                };
                let chunk = if in_pre {
                    String::from(decoded)
                } else {
                    normalize_inline_ws(decoded)
                };
                if !chunk.is_empty() {
                    if in_title || in_subtitle || heading_level > 0 || in_v {
                        cur_text.push_str(&chunk);
                    } else if in_p || in_li || in_epigraph || in_cite {
                        let style = current_style(bold_depth, italic_depth, code_depth);
                        cur_runs_bytes += chunk.len();
                        push_run(&mut cur_runs, style, &chunk);
                        if cur_runs_bytes >= 1800 {
                            flush_p(
                                &mut cur_runs,
                                &mut cur_runs_bytes,
                                blocks,
                                in_epigraph || in_cite,
                                in_li,
                            );
                        }
                    } else if !chunk.trim().is_empty() {
                        let style = current_style(bold_depth, italic_depth, code_depth);
                        cur_runs_bytes += chunk.len();
                        push_run(&mut cur_runs, style, &chunk);
                        in_p = true;
                        if cur_runs_bytes >= 1800 {
                            flush_p(
                                &mut cur_runs,
                                &mut cur_runs_bytes,
                                blocks,
                                in_epigraph || in_cite,
                                in_li,
                            );
                        }
                    }
                }
            }
            pos += lt;
            // Comment or CDATA
            if src[pos..].starts_with("<!--")
                && let Some(end) = src[pos..].find("-->") {
                    pos += end + 3;
                    continue;
                }
            let Some(gt) = src[pos..].find('>') else {
                break;
            };
            let tag_inner = src[pos + 1..pos + gt].trim();
            pos += gt + 1;
            if tag_inner.is_empty() || tag_inner.starts_with('?') || tag_inner.starts_with('!') {
                continue;
            }
            let self_closing = tag_inner.ends_with('/');
            let is_close = tag_inner.starts_with('/');
            let name_raw = tag_inner
                .trim_start_matches('/')
                .trim_end_matches('/')
                .split(|c: char| c.is_whitespace())
                .next()
                .unwrap_or("");
            // Strip XML namespace prefix if any (e.g., xhtml:p -> p)
            let name = name_raw
                .rsplit(':')
                .next()
                .unwrap_or(name_raw)
                .to_ascii_lowercase();

            if !is_close {
                match name.as_str() {
                    "script" | "style" | "head" | "nav" | "annotation" | "binary" | "svg" | "math" => {
                        if !self_closing {
                            skip_depth += 1;
                        }
                    }
                    "title" if fb2_mode && skip_depth == 0 => {
                        in_title = true;
                        cur_text.clear();
                    }
                    "subtitle" if skip_depth == 0 => {
                        in_subtitle = true;
                        cur_text.clear();
                    }
                    "h1" | "h2" | "h3" if skip_depth == 0 => {
                        flush_p(
                            &mut cur_runs,
                            &mut cur_runs_bytes,
                            blocks,
                            in_epigraph || in_cite,
                            in_li,
                        );
                        in_p = false;
                        heading_level = match name.as_str() {
                            "h1" => 1,
                            "h2" => 2,
                            _ => 3,
                        };
                        cur_text.clear();
                    }
                    "p" | "div" if skip_depth == 0 => {
                        if in_title {
                            if !cur_text.is_empty() && !cur_text.ends_with(' ') {
                                cur_text.push(' ');
                            }
                        } else {
                            flush_p(
                                &mut cur_runs,
                                &mut cur_runs_bytes,
                                blocks,
                                in_epigraph || in_cite,
                                in_li,
                            );
                            in_p = true;
                        }
                    }
                    "v" if skip_depth == 0 => {
                        in_v = true;
                        cur_text.clear();
                    }
                    "epigraph" | "blockquote" | "cite" if skip_depth == 0 => {
                        flush_p(&mut cur_runs, &mut cur_runs_bytes, blocks, false, false);
                        in_p = false;
                        if name == "epigraph" {
                            in_epigraph = true;
                        } else {
                            in_cite = true;
                        }
                    }
                    "li" if skip_depth == 0 => {
                        flush_p(&mut cur_runs, &mut cur_runs_bytes, blocks, false, false);
                        in_li = true;
                    }
                    "pre" if skip_depth == 0 => {
                        flush_p(&mut cur_runs, &mut cur_runs_bytes, blocks, false, false);
                        in_pre = true;
                        cur_text.clear();
                    }
                    "strong" | "b" if skip_depth == 0 => bold_depth += 1,
                    "emphasis" | "em" | "i" if skip_depth == 0 => italic_depth += 1,
                    "code" if skip_depth == 0 => code_depth += 1,
                    "empty-line" | "hr" | "mbp:pagebreak" | "pagebreak" if skip_depth == 0 => {
                        flush_p(
                            &mut cur_runs,
                            &mut cur_runs_bytes,
                            blocks,
                            in_epigraph || in_cite,
                            in_li,
                        );
                        in_p = false;
                        blocks.push(Block::SceneBreak);
                    }
                    "br" if skip_depth == 0 && (in_p || in_v || !cur_runs.is_empty()) => {
                        if in_v {
                            let line = collapse_ws(&cur_text);
                            if !line.is_empty() {
                                blocks.push(Block::VerseLine { text: line });
                            }
                            cur_text.clear();
                        } else if cur_runs_bytes >= 600 {
                            flush_p(
                                &mut cur_runs,
                                &mut cur_runs_bytes,
                                blocks,
                                in_epigraph || in_cite,
                                in_li,
                            );
                        } else {
                            cur_runs_bytes += 1;
                            push_run(&mut cur_runs, current_style(bold_depth, italic_depth, code_depth), " ");
                        }
                    }
                    _ => {}
                }
            }
            if is_close || self_closing {
                match name.as_str() {
                    "script" | "style" | "head" | "nav" | "annotation" | "binary" | "svg" | "math" => {
                        skip_depth = skip_depth.saturating_sub(1);
                    }
                    "title" if fb2_mode && in_title => {
                        in_title = false;
                        let t = collapse_ws(&cur_text);
                        if !t.is_empty() {
                            toc.push(TocEntry {
                                title: t.clone(),
                                block_index: blocks.len() as u32,
                            });
                            blocks.push(Block::ChapterTitle { title: t });
                        }
                        cur_text.clear();
                    }
                    "subtitle" if in_subtitle => {
                        in_subtitle = false;
                        let t = collapse_ws(&cur_text);
                        if !t.is_empty() {
                            blocks.push(Block::Heading { level: 2, text: t });
                        }
                        cur_text.clear();
                    }
                    "h1" | "h2" | "h3" if heading_level > 0 => {
                        let lvl = heading_level;
                        heading_level = 0;
                        let t = collapse_ws(&cur_text);
                        if !t.is_empty() {
                            if lvl == 1 {
                                toc.push(TocEntry {
                                    title: t.clone(),
                                    block_index: blocks.len() as u32,
                                });
                                blocks.push(Block::ChapterTitle { title: t });
                            } else {
                                blocks.push(Block::Heading { level: lvl, text: t });
                            }
                        }
                        cur_text.clear();
                    }
                    "p" | "div" if !in_title => {
                        flush_p(
                            &mut cur_runs,
                            &mut cur_runs_bytes,
                            blocks,
                            in_epigraph || in_cite,
                            in_li,
                        );
                        in_p = false;
                    }
                    "v" => {
                        in_v = false;
                        let line = collapse_ws(&cur_text);
                        if !line.is_empty() {
                            blocks.push(Block::VerseLine { text: line });
                        }
                        cur_text.clear();
                    }
                    "epigraph" => {
                        flush_p(&mut cur_runs, &mut cur_runs_bytes, blocks, true, false);
                        in_epigraph = false;
                        in_p = false;
                    }
                    "blockquote" | "cite" => {
                        flush_p(&mut cur_runs, &mut cur_runs_bytes, blocks, true, false);
                        in_cite = false;
                        in_p = false;
                    }
                    "li" => {
                        flush_p(&mut cur_runs, &mut cur_runs_bytes, blocks, false, true);
                        in_li = false;
                    }
                    "pre" => {
                        in_pre = false;
                        if !cur_text.trim().is_empty() {
                            blocks.push(Block::CodeBlock {
                                text: cur_text.trim().into(),
                            });
                        }
                        cur_text.clear();
                    }
                    "strong" | "b" => bold_depth = bold_depth.saturating_sub(1),
                    "emphasis" | "em" | "i" => italic_depth = italic_depth.saturating_sub(1),
                    "code" => code_depth = code_depth.saturating_sub(1),
                    _ => {}
                }
            }
        } else {
            if skip_depth == 0 {
                let decoded = decode_xml_entities(rest);
                let chunk = normalize_inline_ws(&decoded);
                if !chunk.trim().is_empty() {
                    push_run(&mut cur_runs, current_style(bold_depth, italic_depth, code_depth), &chunk);
                }
            }
            break;
        }
    }
    flush_p(
        &mut cur_runs,
        &mut cur_runs_bytes,
        blocks,
        in_epigraph || in_cite,
        in_li,
    );
    *skip_depth_state = skip_depth;
}

fn split_runs_into(runs: Vec<TextRun>, max_bytes: usize, mut emit: impl FnMut(Vec<TextRun>)) {
    let mut cur: Vec<TextRun> = Vec::new();
    let mut cur_bytes = 0usize;
    for r in runs {
        let style = r.style;
        let mut rem = r.text.as_str();
        while !rem.is_empty() {
            let room = max_bytes.saturating_sub(cur_bytes);
            if room == 0 {
                let emitted = runs_trimmed(core::mem::take(&mut cur));
                if !emitted.is_empty() {
                    emit(emitted);
                }
                cur_bytes = 0;
                continue;
            }
            if rem.len() <= room {
                cur_bytes += rem.len();
                push_run(&mut cur, style, rem);
                break;
            }
            let safe = floor_char_boundary(rem, room);
            let mut cut = rem[..safe].rfind(' ').map(|p| p + 1).unwrap_or(safe);
            if cut == 0 {
                if !cur.is_empty() {
                    let emitted = runs_trimmed(core::mem::take(&mut cur));
                    if !emitted.is_empty() {
                        emit(emitted);
                    }
                    cur_bytes = 0;
                    continue;
                }
                cut = rem[..safe]
                    .rfind(' ')
                    .map(|p| p + 1)
                    .unwrap_or_else(|| if safe > 0 { safe } else { ceil_char_boundary(rem, 1) });
            }
            let head = rem[..cut].trim_end();
            if !head.is_empty() {
                push_run(&mut cur, style, head);
            }
            let emitted = runs_trimmed(core::mem::take(&mut cur));
            if !emitted.is_empty() {
                emit(emitted);
            }
            cur_bytes = 0;
            rem = rem[cut..].trim_start();
        }
    }
    let emitted = runs_trimmed(cur);
    if !emitted.is_empty() {
        emit(emitted);
    }
}

pub(crate) fn split_oversize_blocks(blocks: &mut Vec<Block>) {
    const MAX_BLOCK_BYTES: usize = 1800;
    let has_big = blocks.iter().any(|b| match b {
        Block::Paragraph { runs, .. } | Block::BlockQuote { runs } | Block::ListItem { runs, .. } => {
            runs.iter().map(|r| r.text.len()).sum::<usize>() > MAX_BLOCK_BYTES
        }
        _ => false,
    });
    if !has_big {
        return;
    }
    let old = core::mem::take(blocks);
    blocks.reserve(old.len());
    for b in old {
        match b {
            Block::Paragraph {
                runs,
                first_of_chapter,
                is_dialogue,
            } => {
                let total: usize = runs.iter().map(|r| r.text.len()).sum();
                if total <= MAX_BLOCK_BYTES {
                    blocks.push(Block::Paragraph {
                        runs,
                        first_of_chapter,
                        is_dialogue,
                    });
                } else {
                    let mut first = true;
                    split_runs_into(runs, MAX_BLOCK_BYTES, |chunk| {
                        blocks.push(Block::Paragraph {
                            runs: chunk,
                            first_of_chapter: first && first_of_chapter,
                            is_dialogue: first && is_dialogue,
                        });
                        first = false;
                    });
                }
            }
            Block::BlockQuote { runs } => {
                let total: usize = runs.iter().map(|r| r.text.len()).sum();
                if total <= MAX_BLOCK_BYTES {
                    blocks.push(Block::BlockQuote { runs });
                } else {
                    split_runs_into(runs, MAX_BLOCK_BYTES, |chunk| {
                        blocks.push(Block::BlockQuote { runs: chunk });
                    });
                }
            }
            Block::ListItem { bullet, runs } => {
                let total: usize = runs.iter().map(|r| r.text.len()).sum();
                if total <= MAX_BLOCK_BYTES {
                    blocks.push(Block::ListItem { bullet, runs });
                } else {
                    let mut first = true;
                    split_runs_into(runs, MAX_BLOCK_BYTES, |chunk| {
                        blocks.push(Block::ListItem {
                            bullet: if first { bullet.clone() } else { String::new() },
                            runs: chunk,
                        });
                        first = false;
                    });
                }
            }
            other => blocks.push(other),
        }
    }
}

fn current_style(bold: usize, italic: usize, code: usize) -> SpanStyle {
    if code > 0 {
        SpanStyle::Code
    } else if bold > 0 && italic > 0 {
        SpanStyle::BoldItalic
    } else if bold > 0 {
        SpanStyle::Bold
    } else if italic > 0 {
        SpanStyle::Italic
    } else {
        SpanStyle::Regular
    }
}

fn normalize_inline_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    for ch in s.chars() {
        if ch == '\n' || ch == '\r' || ch == '\t' || ch == ' ' {
            if !prev_ws {
                out.push(' ');
            }
            prev_ws = true;
        } else {
            out.push(ch);
            prev_ws = false;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 3. EPUB 2 & 3 parser
// ---------------------------------------------------------------------------

fn parse_epub_opf_meta(
    opf_xml: &str,
    opf_dir: &str,
    filename: &str,
) -> (String, String, String, Vec<String>) {
    let title = extract_xml_tag_text(opf_xml, "dc:title")
        .or_else(|| extract_xml_tag_text(opf_xml, "title"))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| title_from_filename(filename));
    let author = extract_xml_tag_text(opf_xml, "dc:creator")
        .or_else(|| extract_xml_tag_text(opf_xml, "creator"))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| String::from("Unknown Author"));
    let language = extract_xml_tag_text(opf_xml, "dc:language")
        .or_else(|| extract_xml_tag_text(opf_xml, "language"))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            if has_cyrillic(&title) {
                String::from("ru")
            } else {
                String::from("en")
            }
        });

    let mut manifest: Vec<(String, String)> = Vec::new();
    let mut scan = opf_xml;
    while let Some(pos) = scan.find("<item") {
        scan = &scan[pos..];
        let Some(gt) = scan.find('>') else { break };
        let tag = &scan[..=gt];
        if !tag.starts_with("<itemref")
            && !tag.starts_with("<items")
            && let (Some(id), Some(href)) = (extract_attr(tag, "id"), extract_attr(tag, "href"))
        {
            manifest.push((id, href));
        }
        scan = &scan[gt + 1..];
    }

    let mut spine_hrefs = Vec::new();
    let mut sp = opf_xml;
    while let Some(pos) = sp.find("<itemref") {
        sp = &sp[pos..];
        let Some(gt) = sp.find('>') else { break };
        let tag = &sp[..=gt];
        if let Some(idref) = extract_attr(tag, "idref")
            && let Some((_, href)) = manifest.iter().find(|(id, _)| id == &idref)
        {
            spine_hrefs.push(format!("{opf_dir}{href}"));
        }
        sp = &sp[gt + 1..];
    }

    (title, author, language, spine_hrefs)
}

fn classify_epub_encryption_xml_drm(xml: &str) -> bool {
    let lower = xml.to_ascii_lowercase();
    if lower.contains("readium.org/2014/01/lcp") || lower.contains("ns.adobe.com/adept") {
        return true;
    }
    let mut rest = lower.as_str();
    while let Some(start) = rest.find("<encrypteddata") {
        let after = &rest[start..];
        let block = match after.find("</encrypteddata>") {
            Some(end) => {
                rest = &after[end + "</encrypteddata>".len()..];
                &after[..end]
            }
            None => {
                rest = "";
                after
            }
        };
        if block.contains("idpf.org/2008/embedding")
            || block.contains("ns.adobe.com/pdf/enc#rc")
            || block.contains("www.w3.org/2008/embedding")
        {
            continue;
        }
        if let Some(uri_pos) = block.find("uri=") {
            let tail = &block[uri_pos + 4..];
            let q = tail.as_bytes().first().copied().unwrap_or(b'"');
            if (q == b'"' || q == b'\'')
                && let Some(end_q) = tail[1..].find(q as char)
            {
                let uri = tail[1..1 + end_q].split('#').next().unwrap_or("");
                if uri.ends_with(".xhtml")
                    || uri.ends_with(".html")
                    || uri.ends_with(".htm")
                    || uri.ends_with(".xml")
                    || uri.ends_with(".opf")
                    || uri.ends_with(".ncx")
                {
                    return true;
                }
            }
        }
    }
    false
}

pub(crate) fn is_epub_drm_in_memory(zip: &ZipArchive<'_>) -> bool {
    for entry in zip.entries() {
        let clean = entry.name.trim_start_matches("./");
        if clean.eq_ignore_ascii_case("META-INF/rights.xml")
            || clean.eq_ignore_ascii_case("META-INF/license.lcpl")
            || clean.eq_ignore_ascii_case("META-INF/sinf.xml")
        {
            return true;
        }
    }
    for entry in zip.entries() {
        let clean = entry.name.trim_start_matches("./");
        if clean.eq_ignore_ascii_case("META-INF/encryption.xml")
            && let Ok(enc_bytes) = zip.read_meta(entry)
        {
            let (_, xml) = detect_and_decode_to_utf8(&enc_bytes, None);
            if classify_epub_encryption_xml_drm(&xml) {
                return true;
            }
        }
    }
    false
}

pub(crate) fn is_epub_drm_storage<S: StorageBackend>(
    storage: &S,
    zip: &StorageZip,
    path: &str,
    scratch: Option<&mut ZipInflateScratch>,
) -> bool {
    if zip.find_entry("META-INF/rights.xml").is_some()
        || zip.find_entry("META-INF/license.lcpl").is_some()
        || zip.find_entry("META-INF/sinf.xml").is_some()
    {
        return true;
    }
    if let Some(enc_meta) = zip.find_entry("META-INF/encryption.xml")
        && let Ok(enc_raw) =
            zip.read_entry_prefix_with_scratch(storage, path, enc_meta, 8192, scratch)
    {
        let (_, xml) = detect_and_decode_to_utf8(&enc_raw, None);
        if classify_epub_encryption_xml_drm(&xml) {
            return true;
        }
    }
    false
}

pub fn parse_epub(raw: &[u8], filename: &str) -> Result<VvbBook, ParseError> {
    let zip = ZipArchive::parse(raw).map_err(|_| ParseError::InvalidArchive)?;
    if is_epub_drm_in_memory(&zip) {
        return Err(ParseError::UnsupportedFormat);
    }
    let container = zip
        .read_entry("META-INF/container.xml")
        .map_err(|_| ParseError::MalformedDocument)?;
    let (_, container_xml) = detect_and_decode_to_utf8(&container, None);
    let opf_path = extract_attr(&container_xml, "full-path").ok_or(ParseError::MalformedDocument)?;
    let opf_bytes = zip.read_entry(&opf_path).map_err(|_| ParseError::MalformedDocument)?;
    let (_, opf_xml) = detect_and_decode_to_utf8(&opf_bytes, None);

    let opf_dir = match opf_path.rfind('/') {
        Some(idx) => &opf_path[..=idx],
        None => "",
    };

    let (title, author, language, spine_hrefs) = parse_epub_opf_meta(&opf_xml, opf_dir, filename);
    if spine_hrefs.is_empty() {
        return Err(ParseError::MalformedDocument);
    }

    let mut blocks = Vec::new();
    let mut toc = Vec::new();
    for href in spine_hrefs {
        let clean_href = normalize_zip_href(href.split('#').next().unwrap_or(&href));
        if let Ok(xhtml_bytes) = zip.read_entry(&clean_href) {
            let (_, xhtml) = detect_and_decode_to_utf8(&xhtml_bytes, None);
            let body = extract_xml_tag_raw(&xhtml, "body").unwrap_or(&xhtml);
            parse_xml_flow(body, &mut blocks, &mut toc, false);
        }
    }

    if blocks.is_empty() {
        return Err(ParseError::EmptyDocument);
    }

    Ok(VvbBook {
        meta: BookMetadata {
            id_hex: String::new(),
            title,
            author,
            language,
            source_format: String::from("EPUB"),
            word_count: 0,
            cover_w: COVER_THUMB_W,
            cover_h: COVER_THUMB_H,
            cover_2bpp: Vec::new(),
        },
        toc,
        blocks,
        content_blake3: [0u8; 32],
    })
}

pub(crate) fn extract_attr(tag: &str, attr: &str) -> Option<String> {
    for quote in ['"', '\''] {
        let pat = format!("{attr}={quote}");
        let mut search_from = 0usize;
        while let Some(rel) = tag[search_from..].find(&pat) {
            let pos = search_from + rel;
            let boundary_ok = pos == 0 || tag.as_bytes()[pos - 1].is_ascii_whitespace();
            if boundary_ok {
                let rest = &tag[pos + pat.len()..];
                if let Some(end) = rest.find(quote) {
                    return Some(decode_xml_entities(&rest[..end]));
                }
            }
            search_from = pos + pat.len();
        }
    }
    None
}

pub(crate) fn percent_decode_href(href: &str) -> String {
    if !href.contains('%') {
        return String::from(href);
    }
    let bytes = href.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex_val = |b: u8| -> Option<u8> {
                match b {
                    b'0'..=b'9' => Some(b - b'0'),
                    b'a'..=b'f' => Some(10 + b - b'a'),
                    b'A'..=b'F' => Some(10 + b - b'A'),
                    _ => None,
                }
            };
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub(crate) fn fnv1a64_ascii_ci(s: &str) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for &b in s.as_bytes() {
        h ^= b.to_ascii_lowercase() as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

pub(crate) fn opf_tag_local_name(tag: &str) -> &str {
    let inner = tag.trim_start_matches('<').trim_start_matches('/');
    let raw = inner
        .split(|c: char| c.is_ascii_whitespace() || c == '/' || c == '>')
        .next()
        .unwrap_or("");
    raw.rsplit(':').next().unwrap_or(raw)
}

// ---------------------------------------------------------------------------
// 4. HTML parser
// ---------------------------------------------------------------------------

pub fn parse_html(html: &str, filename: &str) -> VvbBook {
    let title = extract_xml_tag_text(html, "title")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| title_from_filename(filename));
    let body = extract_xml_tag_raw(html, "article")
        .or_else(|| extract_xml_tag_raw(html, "main"))
        .or_else(|| extract_xml_tag_raw(html, "body"))
        .unwrap_or(html);
    let mut blocks = Vec::new();
    let mut toc = Vec::new();
    parse_xml_flow(body, &mut blocks, &mut toc, false);
    let lang = if has_cyrillic(&title) { "ru" } else { "en" };
    VvbBook {
        meta: BookMetadata {
            id_hex: String::new(),
            title,
            author: String::from("Web Article"),
            language: String::from(lang),
            source_format: String::from("HTML"),
            word_count: 0,
            cover_w: COVER_THUMB_W,
            cover_h: COVER_THUMB_H,
            cover_2bpp: Vec::new(),
        },
        toc,
        blocks,
        content_blake3: [0u8; 32],
    }
}

// ---------------------------------------------------------------------------
// 5. Markdown parser
// ---------------------------------------------------------------------------

pub fn parse_markdown(md: &str, filename: &str) -> VvbBook {
    let mut title = None;
    let mut author = None;
    let mut blocks = Vec::new();
    let mut toc = Vec::new();

    let mut para_buf = String::new();
    let mut in_code = false;
    let mut code_buf = String::new();
    let cyr = has_cyrillic(md);

    let flush_para = |buf: &mut String, blocks: &mut Vec<Block>| {
        let trimmed = buf.trim();
        if !trimmed.is_empty() {
            let runs = parse_md_inline(trimmed, cyr);
            let dlg = is_dialogue_runs(&runs);
            blocks.push(Block::Paragraph {
                runs,
                first_of_chapter: false,
                is_dialogue: dlg,
            });
        }
        buf.clear();
    };

    for raw_line in md.lines() {
        let line = raw_line.trim_end_matches('\r');
        if line.trim_start().starts_with("```") {
            flush_para(&mut para_buf, &mut blocks);
            if in_code {
                blocks.push(Block::CodeBlock {
                    text: core::mem::take(&mut code_buf),
                });
                in_code = false;
            } else {
                in_code = true;
            }
            continue;
        }
        if in_code {
            if !code_buf.is_empty() {
                code_buf.push('\n');
            }
            code_buf.push_str(line);
            continue;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            flush_para(&mut para_buf, &mut blocks);
            continue;
        }
        if trimmed == "---" || trimmed == "***" || trimmed == "* * *" {
            flush_para(&mut para_buf, &mut blocks);
            blocks.push(Block::SceneBreak);
            continue;
        }
        if let Some(h) = trimmed.strip_prefix("# ") {
            flush_para(&mut para_buf, &mut blocks);
            let t = upgrade_typography(h.trim(), cyr);
            if title.is_none() {
                title = Some(t.clone());
            }
            toc.push(TocEntry {
                title: t.clone(),
                block_index: blocks.len() as u32,
            });
            blocks.push(Block::ChapterTitle { title: t });
            continue;
        }
        if let Some(h) = trimmed.strip_prefix("## ") {
            flush_para(&mut para_buf, &mut blocks);
            let t = upgrade_typography(h.trim(), cyr);
            toc.push(TocEntry {
                title: t.clone(),
                block_index: blocks.len() as u32,
            });
            blocks.push(Block::Heading { level: 2, text: t });
            continue;
        }
        if let Some(h) = trimmed.strip_prefix("### ") {
            flush_para(&mut para_buf, &mut blocks);
            blocks.push(Block::Heading {
                level: 3,
                text: upgrade_typography(h.trim(), cyr),
            });
            continue;
        }
        if let Some(q) = trimmed.strip_prefix("> ") {
            flush_para(&mut para_buf, &mut blocks);
            if q.to_ascii_lowercase().starts_with("author:") && author.is_none() {
                author = Some(String::from(q[7..].trim()));
            } else {
                blocks.push(Block::BlockQuote {
                    runs: parse_md_inline(q.trim(), cyr),
                });
            }
            continue;
        }
        if let Some(li) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")) {
            flush_para(&mut para_buf, &mut blocks);
            blocks.push(Block::ListItem {
                bullet: String::from("•"),
                runs: parse_md_inline(li.trim(), cyr),
            });
            continue;
        }
        if !para_buf.is_empty() {
            para_buf.push(' ');
        }
        para_buf.push_str(trimmed);
    }
    flush_para(&mut para_buf, &mut blocks);

    VvbBook {
        meta: BookMetadata {
            id_hex: String::new(),
            title: title.unwrap_or_else(|| title_from_filename(filename)),
            author: author.unwrap_or_else(|| String::from("Markdown Folio")),
            language: String::from(if cyr { "ru" } else { "en" }),
            source_format: String::from("MD"),
            word_count: 0,
            cover_w: COVER_THUMB_W,
            cover_h: COVER_THUMB_H,
            cover_2bpp: Vec::new(),
        },
        toc,
        blocks,
        content_blake3: [0u8; 32],
    }
}

fn parse_md_inline(s: &str, cyr: bool) -> Vec<TextRun> {
    let mut runs = Vec::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0usize;
    let mut buf = String::new();

    let flush = |b: &mut String, runs: &mut Vec<TextRun>, style: SpanStyle| {
        if !b.is_empty() {
            let up = upgrade_typography(b, cyr);
            push_run(runs, style, &up);
            b.clear();
        }
    };

    while i < chars.len() {
        if chars[i] == '`'
            && let Some(end) = chars[i + 1..].iter().position(|&c| c == '`') {
                flush(&mut buf, &mut runs, SpanStyle::Regular);
                let inner: String = chars[i + 1..i + 1 + end].iter().collect();
                push_run(&mut runs, SpanStyle::Code, &inner);
                i += end + 2;
                continue;
            }
        if i + 1 < chars.len() && chars[i] == '*' && chars[i + 1] == '*'
            && let Some(end) = chars[i + 2..].windows(2).position(|w| w == ['*', '*']) {
                flush(&mut buf, &mut runs, SpanStyle::Regular);
                let inner: String = chars[i + 2..i + 2 + end].iter().collect();
                push_run(&mut runs, SpanStyle::Bold, &upgrade_typography(&inner, cyr));
                i += end + 4;
                continue;
            }
        if chars[i] == '*' || chars[i] == '_' {
            let marker = chars[i];
            if let Some(end) = chars[i + 1..].iter().position(|&c| c == marker)
                && end > 0 {
                    flush(&mut buf, &mut runs, SpanStyle::Regular);
                    let inner: String = chars[i + 1..i + 1 + end].iter().collect();
                    push_run(&mut runs, SpanStyle::Italic, &upgrade_typography(&inner, cyr));
                    i += end + 2;
                    continue;
                }
        }
        buf.push(chars[i]);
        i += 1;
    }
    flush(&mut buf, &mut runs, SpanStyle::Regular);
    runs
}

// ---------------------------------------------------------------------------
// 6. Smart Plain Text (.txt) parser (Russian/English Gutenberg & prose aware)
// ---------------------------------------------------------------------------

pub fn parse_plain_text(txt: &str, filename: &str) -> VvbBook {
    let cyr = has_cyrillic(txt);
    let mut blocks = Vec::new();
    let mut toc = Vec::new();
    let mut para_buf = String::new();
    let mut detected_title: Option<String> = None;

    let flush_para = |buf: &mut String, blocks: &mut Vec<Block>| {
        let trimmed = buf.trim();
        if trimmed.is_empty() {
            buf.clear();
            return;
        }
        let up = upgrade_typography(trimmed, cyr);
        let dlg = up.starts_with('—') || up.starts_with("- ");
        blocks.push(Block::Paragraph {
            runs: vec![TextRun {
                style: SpanStyle::Regular,
                text: up,
            }],
            first_of_chapter: false,
            is_dialogue: dlg,
        });
        buf.clear();
    };

    for raw_line in txt.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            flush_para(&mut para_buf, &mut blocks);
            continue;
        }
        if line == "***" || line == "* * *" || line == "---" {
            flush_para(&mut para_buf, &mut blocks);
            blocks.push(Block::SceneBreak);
            continue;
        }
        if is_chapter_heading_line(line) {
            flush_para(&mut para_buf, &mut blocks);
            let t = upgrade_typography(line, cyr);
            if detected_title.is_none() && toc.is_empty() {
                // Keep filename or first non-chapter title line
            }
            toc.push(TocEntry {
                title: t.clone(),
                block_index: blocks.len() as u32,
            });
            blocks.push(Block::ChapterTitle { title: t });
            continue;
        }
        // Preserve Russian direct speech paragraph breaks (`— ` or `- ` at line start)
        if para_buf.is_empty() && detected_title.is_none() && blocks.is_empty() && line.chars().count() < 64 {
            detected_title = Some(upgrade_typography(line, cyr));
            toc.push(TocEntry {
                title: detected_title.clone().unwrap(),
                block_index: 0,
            });
            blocks.push(Block::ChapterTitle {
                title: detected_title.clone().unwrap(),
            });
            continue;
        }
        if (line.starts_with("— ") || line.starts_with("- ")) && !para_buf.is_empty() {
            flush_para(&mut para_buf, &mut blocks);
        }
        if !para_buf.is_empty() {
            para_buf.push(' ');
        }
        para_buf.push_str(line);
    }
    flush_para(&mut para_buf, &mut blocks);

    VvbBook {
        meta: BookMetadata {
            id_hex: String::new(),
            title: detected_title.unwrap_or_else(|| title_from_filename(filename)),
            author: String::from(if cyr { "Текст" } else { "Plain Text" }),
            language: String::from(if cyr { "ru" } else { "en" }),
            source_format: String::from("TXT"),
            word_count: 0,
            cover_w: COVER_THUMB_W,
            cover_h: COVER_THUMB_H,
            cover_2bpp: Vec::new(),
        },
        toc,
        blocks,
        content_blake3: [0u8; 32],
    }
}

fn is_chapter_heading_line(line: &str) -> bool {
    if line.chars().count() > 64 {
        return false;
    }
    let lower = line.to_lowercase();
    lower.starts_with("chapter ")
        || lower.starts_with("part ")
        || lower.starts_with("глава ")
        || lower.starts_with("часть ")
        || lower.starts_with("пролог")
        || lower.starts_with("эпилог")
}

// ---------------------------------------------------------------------------
// 7. Fast Metadata Probing & Low-RAM Streaming Ingestion (ESP32-C3 safe!)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbedBookMeta {
    pub id_hex: String,
    pub title: String,
    pub author: String,
    pub language: String,
    pub source_format: String,
    pub est_words: u32,
    pub est_blocks: u32,
}

pub fn source_book_id_hex(path: &str, file_size: u64) -> String {
    let mut h = blake3::Hasher::new();
    h.update(path.as_bytes());
    h.update(b":");
    h.update(&file_size.to_le_bytes());
    encode_short_id(&h.finalize().as_bytes()[..4])
}

pub(crate) fn normalize_zip_href(raw_href: &str) -> String {
    let mut dec = Vec::with_capacity(raw_href.len());
    let b = raw_href.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(hi), Some(lo)) = (hex_val(b[i + 1]), hex_val(b[i + 2]))
        {
            dec.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        dec.push(b[i]);
        i += 1;
    }
    let s = String::from_utf8_lossy(&dec);
    let mut parts: Vec<&str> = Vec::new();
    for seg in s.split(['/', '\\']) {
        match seg {
            "" | "." => {}
            ".." => {
                let _ = parts.pop();
            }
            p => parts.push(p),
        }
    }
    parts.join("/")
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

pub(crate) fn trim_utf8_complete_prefix(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }
    let len = bytes.len();
    // Walk back at most 3 trailing continuation bytes if they form an incomplete multi-byte UTF-8 sequence.
    let mut tail = 0usize;
    while tail < 3 && tail < len && (bytes[len - 1 - tail] & 0xC0) == 0x80 {
        tail += 1;
    }
    let idx = len - tail;
    if idx > 0 {
        let lead = bytes[idx - 1];
        let needed = if (lead & 0x80) == 0 {
            1
        } else if (lead & 0xE0) == 0xC0 {
            2
        } else if (lead & 0xF0) == 0xE0 {
            3
        } else if (lead & 0xF8) == 0xF0 {
            4
        } else {
            1
        };
        if len - (idx - 1) < needed {
            return idx - 1;
        }
    }
    len
}

pub(crate) fn read_storage_prefix<S: StorageBackend>(storage: &S, path: &str, cap: usize) -> Result<Vec<u8>, ParseError> {
    let mut out = vec![0u8; cap];
    let mut got = 0usize;
    while got < cap {
        let n = storage
            .read_range(path, got as u64, &mut out[got..])
            .map_err(|_| ParseError::MalformedDocument)?;
        if n == 0 {
            break;
        }
        got += n;
    }
    out.truncate(got);
    Ok(out)
}

pub(crate) fn fb2_meta_from_head(head_bytes: &[u8], filename: &str) -> (String, String, String) {
    let cut = trim_utf8_complete_prefix(head_bytes);
    let (_, xml) = detect_and_decode_to_utf8(&head_bytes[..cut], None);
    let title = extract_xml_tag_text(&xml, "book-title")
        .map(|s| collapse_ws(&decode_xml_entities(&s)))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| title_from_filename(filename));
    let author = extract_fb2_author(&xml);
    let language = extract_xml_tag_text(&xml, "lang")
        .map(|s| collapse_ws(&s))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            if has_cyrillic(&title) {
                String::from("ru")
            } else {
                String::from("en")
            }
        });
    (title, author, language)
}

pub(crate) fn parse_epub_opf_dc_meta(opf_xml: &str, filename: &str) -> (String, String, String) {
    let title = extract_xml_tag_text(opf_xml, "dc:title")
        .or_else(|| extract_xml_tag_text(opf_xml, "title"))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| title_from_filename(filename));
    let author = extract_xml_tag_text(opf_xml, "dc:creator")
        .or_else(|| extract_xml_tag_text(opf_xml, "creator"))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| String::from("Unknown Author"));
    let language = extract_xml_tag_text(opf_xml, "dc:language")
        .or_else(|| extract_xml_tag_text(opf_xml, "language"))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            if has_cyrillic(&title) {
                String::from("ru")
            } else {
                String::from("en")
            }
        });
    (title, author, language)
}

pub(crate) fn is_spine_text_href(href: &str) -> bool {
    let clean = href.split('#').next().unwrap_or(href);
    let lo = clean.to_ascii_lowercase();
    lo.ends_with(".xhtml") || lo.ends_with(".html") || lo.ends_with(".htm") || lo.ends_with(".xml")
}

fn stream_epub_opf_spine<S: StorageBackend>(
    storage: &S,
    zip: &StorageZip,
    path: &str,
    opf_meta: &ZipEntryMeta,
    opf_dir: &str,
) -> Result<Vec<String>, ParseError> {
    let mut stream = zip
        .open_stream(storage, path, opf_meta, 512 * 1024)
        .map_err(|_| ParseError::InvalidArchive)?;
    let mut carry: Vec<u8> = Vec::new();
    let mut text = String::new();
    let mut chunk = [0u8; 2048];
    let mut manifest: Vec<(String, String)> = Vec::new();
    let mut manifest_sorted = false;
    let mut spine_hrefs: Vec<String> = Vec::new();
    let mut enc: Option<TextEncoding> = None;

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
                        && let (Some(id), Some(href)) = (extract_attr(tag, "id"), extract_attr(tag, "href"))
                        && is_spine_text_href(&href)
                    {
                        manifest.push((id, href));
                        manifest_sorted = false;
                    } else if lname.eq_ignore_ascii_case("itemref")
                        && let Some(idref) = extract_attr(tag, "idref")
                    {
                        if !manifest_sorted {
                            manifest.sort_by(|a, b| a.0.cmp(&b.0));
                            manifest_sorted = true;
                        }
                        if let Ok(idx) = manifest.binary_search_by(|(id, _)| id.cmp(&idref)) {
                            let href = &manifest[idx].1;
                            let no_frag = href.split('#').next().unwrap_or(href);
                            let decoded = percent_decode_href(no_frag);
                            spine_hrefs.push(normalize_zip_href(&format!("{opf_dir}{decoded}")));
                        }
                    }
                }
            }
            text.drain(..=gt);
        }

        if eof {
            break;
        }
    }
    Ok(spine_hrefs)
}

pub fn probe_book_storage<S: StorageBackend>(
    storage: &S,
    path: &str,
    file_size: u64,
) -> Result<ProbedBookMeta, ParseError> {
    probe_book_storage_with_scratch(storage, path, file_size, None)
}

pub fn probe_book_storage_with_scratch<S: StorageBackend>(
    storage: &S,
    path: &str,
    file_size: u64,
    mut ext_scratch: Option<&mut ZipInflateScratch>,
) -> Result<ProbedBookMeta, ParseError> {
    let fmt = InputFormat::from_filename(path).ok_or(ParseError::UnsupportedFormat)?;
    if file_size == 0 {
        return Err(ParseError::EmptyDocument);
    }

    match fmt {
        InputFormat::Vvb => {
            let idx = VvbIndex::open_from_storage(storage, path)
                .map_err(|_| ParseError::MalformedDocument)?;
            let est_blocks = idx.block_count() as u32;
            Ok(ProbedBookMeta {
                id_hex: idx.meta.id_hex,
                title: idx.meta.title,
                author: idx.meta.author,
                language: idx.meta.language,
                source_format: String::from("VVB"),
                est_words: idx.meta.word_count,
                est_blocks,
            })
        }
        InputFormat::Epub => {
            let zip = StorageZip::open(storage, path, file_size).map_err(|_| ParseError::InvalidArchive)?;
            let html_bytes = zip.text_uncomp_bytes();
            let c_meta = zip
                .find_entry("META-INF/container.xml")
                .ok_or(ParseError::MalformedDocument)?
                .clone();
            let mut owned_scratch = if ext_scratch.is_none() {
                Some(ZipInflateScratch::new())
            } else {
                None
            };
            let scratch = match ext_scratch.as_deref_mut() {
                Some(sc) => {
                    sc.reset();
                    sc
                }
                None => owned_scratch.as_mut().expect("owned scratch initialized"),
            };
            if is_epub_drm_storage(storage, &zip, path, Some(scratch)) {
                return Err(ParseError::UnsupportedFormat);
            }
            scratch.reset();
            let opf_path = {
                let c_raw = zip
                    .read_entry_prefix_with_scratch(storage, path, &c_meta, 4096, Some(scratch))
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
            let (title, author, language, spine_targets, _) = crate::direct::stream_epub_opf_compact(
                storage,
                file_size,
                path,
                &opf_meta,
                opf_dir,
                scratch,
            )?;
            let first_hash = spine_targets
                .iter()
                .min_by_key(|&&(_, idx)| idx)
                .map(|&(h, _)| h)
                .ok_or(ParseError::MalformedDocument)?;
            let mut first_ref: Option<ZipEntryRef> = None;
            zip.scan_cd_entries(storage, path, |name, entry_ref| {
                if first_ref.is_none() {
                    let clean = name.trim_start_matches("./");
                    if crate::direct::hash_canonical_zip_href("", clean) == first_hash {
                        first_ref = Some(entry_ref);
                    }
                }
                Ok(())
            })
            .map_err(|_| ParseError::InvalidArchive)?;
            drop(zip);
            let first_entry_ref = first_ref.ok_or(ParseError::MalformedDocument)?;
            scratch.reset();
            let mut st = StorageZip::open_entry_ref_stream(
                file_size,
                storage,
                path,
                first_entry_ref,
                512,
                Some(scratch),
            )
            .map_err(|_| ParseError::InvalidArchive)?;
            let mut sample = [0u8; 512];
            let mut sample_len = 0usize;
            while sample_len < sample.len() {
                let n = st
                    .next_chunk(storage, &mut sample[sample_len..])
                    .map_err(|_| ParseError::InvalidArchive)?;
                if n == 0 {
                    break;
                }
                sample_len += n;
            }
            drop(st);
            drop(owned_scratch);
            if sample_len == 0 || sample[..sample_len].contains(&0) {
                return Err(ParseError::MalformedDocument);
            }
            let est_words = ((html_bytes / 12).max(120)).min(u32::MAX as u64) as u32;
            Ok(ProbedBookMeta {
                id_hex: source_book_id_hex(path, file_size),
                title,
                author,
                language,
                source_format: String::from("EPUB"),
                est_words,
                est_blocks: (est_words / 24).max(1),
            })
        }
        InputFormat::Fb2 => {
            if file_size < 64 {
                return Err(ParseError::MalformedDocument);
            }
            let head = read_storage_prefix(storage, path, 8 * 1024)?;
            if head.contains(&0) {
                return Err(ParseError::MalformedDocument);
            }
            let cut = trim_utf8_complete_prefix(&head);
            let (_, head_str) = detect_and_decode_to_utf8(&head[..cut], None);
            if !head_str.to_ascii_lowercase().contains("<fictionbook") {
                return Err(ParseError::MalformedDocument);
            }
            drop(head_str);
            let (title, author, language) = fb2_meta_from_head(&head, path);
            let est_words = ((file_size / 14).max(80)).min(u32::MAX as u64) as u32;
            Ok(ProbedBookMeta {
                id_hex: source_book_id_hex(path, file_size),
                title,
                author,
                language,
                source_format: String::from("FB2"),
                est_words,
                est_blocks: (est_words / 24).max(1),
            })
        }
        InputFormat::Fb2Zip => {
            let zip = StorageZip::open(storage, path, file_size).map_err(|_| ParseError::InvalidArchive)?;
            let fb2_entry = zip
                .entries()
                .iter()
                .find(|e| e.name.to_ascii_lowercase().ends_with(".fb2"))
                .or_else(|| zip.entries().first())
                .ok_or(ParseError::MalformedDocument)?
                .clone();
            let uncomp_sz = fb2_entry.uncomp_size;
            let mut owned_scratch = if fb2_entry.method == 8 && ext_scratch.is_none() {
                Some(ZipInflateScratch::new())
            } else {
                None
            };
            let scratch_opt = if fb2_entry.method == 8 {
                match ext_scratch {
                    Some(sc) => {
                        sc.reset();
                        Some(sc)
                    }
                    None => owned_scratch.as_mut(),
                }
            } else {
                None
            };
            let head = zip
                .read_entry_prefix_with_scratch(storage, path, &fb2_entry, 6 * 1024, scratch_opt)
                .map_err(|_| ParseError::InvalidArchive)?;
            drop(owned_scratch);
            drop(zip);
            if head.is_empty() || head.contains(&0) {
                return Err(ParseError::MalformedDocument);
            }
            let cut = trim_utf8_complete_prefix(&head);
            let (_, head_str) = detect_and_decode_to_utf8(&head[..cut], None);
            let lower = head_str.to_ascii_lowercase();
            if !lower.contains("<fictionbook") && !lower.contains("<?xml") {
                return Err(ParseError::MalformedDocument);
            }
            drop(head_str);
            let (title, author, language) = fb2_meta_from_head(&head, path);
            let est_words = ((uncomp_sz as u64 / 14).max(80)).min(u32::MAX as u64) as u32;
            Ok(ProbedBookMeta {
                id_hex: source_book_id_hex(path, file_size),
                title,
                author,
                language,
                source_format: String::from("FB2.ZIP"),
                est_words,
                est_blocks: (est_words / 24).max(1),
            })
        }
        InputFormat::Mobi => {
            let hdr = crate::mobi::probe_mobi_header(storage, path, file_size)?;
            if hdr.encryption_type != 0
                || !matches!(hdr.compression, 1 | 2)
                || hdr.text_record_count == 0
                || hdr.flow0_end == 0
            {
                return Err(ParseError::UnsupportedFormat);
            }
            let est_words = ((hdr.flow0_end as u64 / 12).max(80)).min(u32::MAX as u64) as u32;
            Ok(ProbedBookMeta {
                id_hex: source_book_id_hex(path, file_size),
                title: hdr.title,
                author: hdr.author,
                language: hdr.language,
                source_format: String::from(hdr.source_format),
                est_words,
                est_blocks: (est_words / 24).max(1),
            })
        }
        InputFormat::Html => {
            if file_size < 64 {
                return Err(ParseError::MalformedDocument);
            }
            let head = read_storage_prefix(storage, path, 8 * 1024)?;
            if head.is_empty() || head.contains(&0) {
                return Err(ParseError::MalformedDocument);
            }
            let cut = trim_utf8_complete_prefix(&head);
            let (_, txt) = detect_and_decode_to_utf8(&head[..cut], None);
            let lower = txt.to_ascii_lowercase();
            if !["<html", "<body", "<p>", "<p ", "<div", "<h1", "<h2", "<article", "<section"]
                .iter()
                .any(|tag| lower.contains(tag))
            {
                return Err(ParseError::MalformedDocument);
            }
            let title = extract_xml_tag_text(&txt, "title")
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| title_from_filename(path));
            let lang = if has_cyrillic(&title) || has_cyrillic(&txt) {
                "ru"
            } else {
                "en"
            };
            let est_words = ((file_size / 12).max(50)).min(u32::MAX as u64) as u32;
            Ok(ProbedBookMeta {
                id_hex: source_book_id_hex(path, file_size),
                title,
                author: String::from("Web Article"),
                language: String::from(lang),
                source_format: String::from("HTML"),
                est_words,
                est_blocks: (est_words / 24).max(1),
            })
        }
        InputFormat::Markdown | InputFormat::Txt => {
            if file_size < 24 {
                return Err(ParseError::MalformedDocument);
            }
            let head = read_storage_prefix(storage, path, 4 * 1024)?;
            if head.is_empty() || head.contains(&0) {
                return Err(ParseError::MalformedDocument);
            }
            let bad_ctrl = head
                .iter()
                .filter(|&&b| b < 0x20 && !matches!(b, b'\t' | b'\n' | b'\r' | 0x0C))
                .count();
            if bad_ctrl > head.len() / 50 {
                return Err(ParseError::MalformedDocument);
            }
            let cut = trim_utf8_complete_prefix(&head);
            let (_, txt) = detect_and_decode_to_utf8(&head[..cut], None);
            if txt.chars().filter(|c| c.is_alphabetic()).count() < 8 {
                return Err(ParseError::MalformedDocument);
            }
            let title = txt
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .and_then(|l| {
                    if fmt == InputFormat::Markdown {
                        Some(l.trim_start_matches('#').trim().to_string())
                    } else if l.chars().count() < 64 {
                        Some(l.to_string())
                    } else {
                        None
                    }
                })
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| title_from_filename(path));
            let cyr = has_cyrillic(&txt);
            let author = match (fmt, cyr) {
                (InputFormat::Markdown, _) => "Markdown Folio",
                (_, true) => "Текст",
                (_, false) => "Plain Text",
            };
            let est_words = ((file_size / 8).max(40)).min(u32::MAX as u64) as u32;
            Ok(ProbedBookMeta {
                id_hex: source_book_id_hex(path, file_size),
                title,
                author: String::from(author),
                language: String::from(if cyr { "ru" } else { "en" }),
                source_format: String::from(fmt.label()),
                est_words,
                est_blocks: (est_words / 24).max(1),
            })
        }
    }
}

pub(crate) trait BlockConsumer {
    fn wants_more(&self) -> bool {
        true
    }
    fn push_blocks(&mut self, blocks: Vec<Block>, local_toc: Vec<TocEntry>) -> Result<(), ParseError>;
}

const DIRECT_COLLECTOR_MAX_TEXT_BYTES: usize = 15 * 1024;

fn block_text_bytes(b: &Block) -> usize {
    match b {
        Block::ChapterTitle { title }
        | Block::Heading { text: title, .. }
        | Block::VerseLine { text: title }
        | Block::CodeBlock { text: title } => title.len() + 32,
        Block::Paragraph { runs, .. } | Block::BlockQuote { runs } | Block::ListItem { runs, .. } => {
            32 + runs.iter().map(|r| r.text.len() + 24).sum::<usize>()
        }
        Block::SceneBreak => 16,
    }
}

pub(crate) struct VecBlockCollector {
    pub(crate) blocks: Vec<Block>,
    pub(crate) toc: Vec<TocEntry>,
    pub(crate) max_blocks: usize,
    pub(crate) max_text_bytes: usize,
    pub(crate) cur_text_bytes: usize,
    cur_slice: usize,
    cur_slice_blocks: usize,
    target_slice: usize,
    pub(crate) truncated: bool,
}

impl VecBlockCollector {
    pub(crate) fn new(max_blocks: usize) -> Self {
        Self::with_slice(max_blocks, 0)
    }

    pub(crate) fn with_slice(max_blocks: usize, target_slice: usize) -> Self {
        Self {
            blocks: Vec::new(),
            toc: Vec::new(),
            max_blocks,
            max_text_bytes: DIRECT_COLLECTOR_MAX_TEXT_BYTES,
            cur_text_bytes: 0,
            cur_slice: 0,
            cur_slice_blocks: 0,
            target_slice,
            truncated: false,
        }
    }
}

impl BlockConsumer for VecBlockCollector {
    fn wants_more(&self) -> bool {
        !self.truncated
            && (self.cur_slice < self.target_slice
                || (self.blocks.len() < self.max_blocks
                    && self.cur_text_bytes < self.max_text_bytes))
    }

    fn push_blocks(&mut self, mut blocks: Vec<Block>, local_toc: Vec<TocEntry>) -> Result<(), ParseError> {
        split_oversize_blocks(&mut blocks);
        if self.truncated {
            return Ok(());
        }
        let base_idx = self.blocks.len() as u32;
        if self.cur_slice == self.target_slice {
            for t in local_toc {
                if self.toc.len() < 128
                    && (t.block_index as usize) < self.max_blocks.saturating_sub(base_idx as usize)
                {
                    self.toc.push(TocEntry {
                        title: t.title,
                        block_index: base_idx + t.block_index,
                    });
                }
            }
        }
        for b in blocks {
            let cost = block_text_bytes(&b);
            let slice_full = self.cur_slice_blocks >= self.max_blocks
                || (self.cur_slice_blocks > 0
                    && self.cur_text_bytes.saturating_add(cost) > self.max_text_bytes);
            if slice_full {
                if self.cur_slice >= self.target_slice {
                    self.truncated = true;
                    break;
                }
                self.cur_slice += 1;
                self.cur_slice_blocks = 0;
                self.cur_text_bytes = 0;
            }
            self.cur_slice_blocks += 1;
            self.cur_text_bytes = self.cur_text_bytes.saturating_add(cost);
            if self.cur_slice == self.target_slice {
                self.blocks.push(b);
            }
        }
        Ok(())
    }
}

impl<S: StorageBackend> BlockConsumer for StreamSink<'_, S> {
    fn wants_more(&self) -> bool {
        self.block_count < 262_144
    }

    fn push_blocks(&mut self, blocks: Vec<Block>, local_toc: Vec<TocEntry>) -> Result<(), ParseError> {
        StreamSink::push_blocks(self, blocks, local_toc)
    }
}

struct StreamSink<'a, S: StorageBackend> {
    storage: &'a mut S,
    tmp_path: &'a str,
    block_count: usize,
    checkpoint_shift: u8,
    checkpoints: Vec<u32>,
    toc: Vec<TocEntry>,
    stream_len: u32,
    word_count: u32,
    after_chapter: bool,
    pending_bytes: Vec<u8>,
}

impl<'a, S: StorageBackend> StreamSink<'a, S> {
    fn new(storage: &'a mut S, tmp_path: &'a str, preamble: &[u8]) -> Self {
        let mut pending_bytes = Vec::with_capacity(16384);
        pending_bytes.extend_from_slice(preamble);
        Self {
            storage,
            tmp_path,
            block_count: 0,
            checkpoint_shift: 0,
            checkpoints: Vec::new(),
            toc: Vec::new(),
            stream_len: 0,
            word_count: 0,
            after_chapter: false,
            pending_bytes,
        }
    }

    fn flush_bytes(&mut self) -> Result<(), ParseError> {
        if !self.pending_bytes.is_empty() {
            self.storage
                .append(self.tmp_path, &self.pending_bytes)
                .map_err(|_| ParseError::MalformedDocument)?;
            self.pending_bytes.clear();
        }
        Ok(())
    }

    fn record_checkpoint(&mut self, block_idx: usize, offset: u32) {
        let mask = (1usize << self.checkpoint_shift) - 1;
        if (block_idx & mask) != 0 {
            return;
        }
        if self.checkpoints.len() >= MAX_VVB_CHECKPOINTS {
            let half = self.checkpoints.len().div_ceil(2);
            for i in 0..half {
                self.checkpoints[i] = self.checkpoints[i * 2];
            }
            self.checkpoints.truncate(half);
            self.checkpoint_shift += 1;
        }
        self.checkpoints.push(offset);
    }

    fn push_blocks(&mut self, mut blocks: Vec<Block>, local_toc: Vec<TocEntry>) -> Result<(), ParseError> {
        const MAX_STREAM_BLOCKS: usize = 262_144;
        const MAX_STREAM_TOC: usize = 512;

        if self.block_count >= MAX_STREAM_BLOCKS {
            return Ok(());
        }
        let base_idx = self.block_count as u32;
        for t in local_toc {
            if self.toc.len() >= MAX_STREAM_TOC {
                break;
            }
            self.toc.push(TocEntry {
                title: t.title,
                block_index: base_idx + t.block_index,
            });
        }
        for b in &mut blocks {
            if self.block_count >= MAX_STREAM_BLOCKS {
                break;
            }
            match b {
                Block::ChapterTitle { .. } => self.after_chapter = true,
                Block::Paragraph {
                    first_of_chapter,
                    is_dialogue,
                    ..
                } => {
                    if self.after_chapter && !*is_dialogue {
                        *first_of_chapter = true;
                        self.after_chapter = false;
                    }
                }
                Block::VerseLine { .. } => self.after_chapter = false,
                _ => {}
            }
            self.word_count += block_word_count(b);
            let idx = self.block_count;
            let off = self.stream_len;
            self.record_checkpoint(idx, off);
            self.block_count += 1;
            let encoded = encode_block(b);
            let elen = encoded.len() as u32;
            self.pending_bytes.extend_from_slice(&elen.to_le_bytes());
            self.pending_bytes.extend_from_slice(&encoded);
            self.stream_len = self.stream_len.saturating_add(4 + elen);
            if self.pending_bytes.len() >= 16384 {
                self.flush_bytes()?;
            }
        }
        Ok(())
    }
}

fn block_word_count(b: &Block) -> u32 {
    match b {
        Block::ChapterTitle { title } | Block::Heading { text: title, .. } | Block::VerseLine { text: title } => {
            title.split_whitespace().count() as u32
        }
        Block::Paragraph { runs, .. } | Block::BlockQuote { runs } | Block::ListItem { runs, .. } => {
            runs.iter().map(|r| r.text.split_whitespace().count() as u32).sum()
        }
        Block::CodeBlock { text } => text.split_whitespace().count() as u32,
        Block::SceneBreak => 0,
    }
}

pub(crate) fn find_ascii_nocase(haystack: &str, needle_lower: &[u8]) -> Option<usize> {
    if needle_lower.is_empty() || haystack.len() < needle_lower.len() {
        return None;
    }
    haystack
        .as_bytes()
        .windows(needle_lower.len())
        .position(|w| w.eq_ignore_ascii_case(needle_lower))
}

pub(crate) struct XmlChunkFilter {
    enc: Option<TextEncoding>,
    byte_carry: Vec<u8>,
    text_win: String,
    skipping_binary: bool,
    skipping_oversize_tag: bool,
    skip_depth: usize,
    fb2_mode: bool,
    seen_body: bool,
    pub(crate) done: bool,
}

impl XmlChunkFilter {
    pub(crate) fn new(fb2_mode: bool) -> Self {
        Self {
            enc: None,
            byte_carry: Vec::new(),
            text_win: String::new(),
            skipping_binary: false,
            skipping_oversize_tag: false,
            skip_depth: 0,
            fb2_mode,
            seen_body: !fb2_mode,
            done: false,
        }
    }

    pub(crate) fn reset_for_chapter(&mut self) {
        self.enc = None;
        self.byte_carry.clear();
        self.text_win.clear();
        self.skipping_binary = false;
        self.skipping_oversize_tag = false;
        self.skip_depth = 0;
        self.seen_body = !self.fb2_mode;
        self.done = false;
    }

    pub(crate) fn set_encoding(&mut self, enc: TextEncoding) {
        self.enc = Some(enc);
    }

    pub(crate) fn set_seen_body(&mut self, seen: bool) {
        self.seen_body = seen;
    }

    fn push_decoded_str(&mut self, decoded: &str) {
        if self.skipping_binary {
            if let Some(end_bin) = find_ascii_nocase(decoded, b"</binary>") {
                self.skipping_binary = false;
                self.text_win.push_str(&decoded[end_bin + 9..]);
            }
        } else if self.skipping_oversize_tag {
            if let Some(gt) = decoded.find('>') {
                self.skipping_oversize_tag = false;
                self.text_win.push_str(&decoded[gt + 1..]);
            }
        } else {
            self.text_win.push_str(decoded);
        }
    }

    pub(crate) fn feed_bytes<C: BlockConsumer>(
        &mut self,
        chunk: &[u8],
        sink: &mut C,
    ) -> Result<(), ParseError> {
        if self.done || !sink.wants_more() || chunk.is_empty() {
            if !sink.wants_more() {
                self.done = true;
            }
            return Ok(());
        }
        let mut owned_combined = Vec::new();
        if !self.byte_carry.is_empty() {
            owned_combined = core::mem::take(&mut self.byte_carry);
            owned_combined.extend_from_slice(chunk);
        }
        let data: &[u8] = if owned_combined.is_empty() {
            chunk
        } else {
            &owned_combined
        };

        if self.enc.is_none() && data.len() >= 64 {
            let utf8_safe = trim_utf8_complete_prefix(data);
            let probe = if utf8_safe >= 64 { &data[..utf8_safe] } else { data };
            self.enc = Some(detect_encoding(probe, None));
        }
        let enc = self.enc.unwrap_or_else(|| {
            let utf8_safe = trim_utf8_complete_prefix(data);
            detect_encoding(&data[..utf8_safe], None)
        });
        let usable = if enc == TextEncoding::Utf8 {
            trim_utf8_complete_prefix(data)
        } else {
            data.len()
        };
        if usable < data.len() {
            self.byte_carry = data[usable..].to_vec();
        }
        let valid_bytes = &data[..usable];
        if enc == TextEncoding::Utf8 {
            let raw_utf8 = valid_bytes
                .strip_prefix(&[0xEF, 0xBB, 0xBF])
                .unwrap_or(valid_bytes);
            if let Ok(s) = core::str::from_utf8(raw_utf8) {
                self.push_decoded_str(s);
            } else {
                let decoded = decode_with(valid_bytes, enc);
                self.push_decoded_str(&decoded);
            }
        } else {
            let decoded = decode_with(valid_bytes, enc);
            self.push_decoded_str(&decoded);
        }
        drop(owned_combined);

        self.drain_ready(false, sink)
    }

    fn drain_ready<C: BlockConsumer>(
        &mut self,
        eof: bool,
        sink: &mut C,
    ) -> Result<(), ParseError> {
        // Strip <binary>...</binary> blocks and oversize >3KB tags in-flight so RAM stays bounded
        loop {
            if self.skipping_oversize_tag {
                if let Some(gt) = self.text_win.find('>') {
                    self.text_win.drain(..=gt);
                    self.skipping_oversize_tag = false;
                    continue;
                }
                self.text_win.clear();
                return Ok(());
            }
            if self.skipping_binary {
                if let Some(end_bin) = find_ascii_nocase(&self.text_win, b"</binary>") {
                    self.text_win.drain(..end_bin + 9);
                    self.skipping_binary = false;
                    continue;
                }
                if self.text_win.len() > 32 {
                    let keep = self.text_win.len() - 16;
                    let safe = ceil_char_boundary(&self.text_win, keep);
                    self.text_win.drain(..safe);
                }
                break;
            } else if let Some(b_start) = find_ascii_nocase(&self.text_win, b"<binary") {
                if b_start > 0 {
                    Self::parse_slice(
                        &self.text_win[..b_start],
                        self.fb2_mode,
                        &mut self.done,
                        &mut self.skip_depth,
                        sink,
                    )?;
                    self.text_win.drain(..b_start);
                }
                if let Some(b_end) = find_ascii_nocase(&self.text_win, b"</binary>") {
                    self.text_win.drain(..b_end + 9);
                    continue;
                } else {
                    self.text_win.clear();
                    self.skipping_binary = true;
                    break;
                }
            } else {
                break;
            }
        }

        if self.fb2_mode && !self.seen_body {
            if let Some(bp) = find_ascii_nocase(&self.text_win, b"<body") {
                if let Some(gt) = self.text_win[bp..].find('>') {
                    self.text_win.drain(..bp + gt + 1);
                    self.seen_body = true;
                }
            } else {
                if self.text_win.len() > 2048 && !eof {
                    let cut = ceil_char_boundary(&self.text_win, self.text_win.len() - 64);
                    self.text_win.drain(..cut);
                }
                return Ok(());
            }
        }

        if !eof && self.text_win.len() < 2560 {
            return Ok(());
        }

        let cut = if eof {
            self.text_win.len()
        } else {
            find_xml_flush_boundary(&self.text_win)
        };
        if cut == 0 {
            if self.text_win.len() > 5 * 1024 {
                if let Some(lt) = self.text_win.rfind('<')
                    && !self.text_win[lt..].contains('>')
                    && self.text_win.len() - lt > 3 * 1024
                {
                    if lt > 0 {
                        Self::parse_slice(
                            &self.text_win[..lt],
                            self.fb2_mode,
                            &mut self.done,
                            &mut self.skip_depth,
                            sink,
                        )?;
                    }
                    self.text_win.clear();
                    if self.text_win.capacity() > 8192 {
                        self.text_win.shrink_to(4096);
                    }
                    self.skipping_oversize_tag = true;
                    return Ok(());
                }
                let safe = floor_char_boundary(&self.text_win, self.text_win.len() - 256);
                let split = self.text_win[..safe]
                    .rfind('>')
                    .map(|p| p + 1)
                    .or_else(|| self.text_win[..safe].rfind(char::is_whitespace))
                    .unwrap_or(safe);
                if split > 0 {
                    Self::parse_slice(
                        &self.text_win[..split],
                        self.fb2_mode,
                        &mut self.done,
                        &mut self.skip_depth,
                        sink,
                    )?;
                    self.text_win.drain(..split);
                }
            }
            if self.text_win.capacity() > 8192 && self.text_win.len() <= 2048 {
                self.text_win.shrink_to(4096);
            }
            return Ok(());
        }
        Self::parse_slice(
            &self.text_win[..cut],
            self.fb2_mode,
            &mut self.done,
            &mut self.skip_depth,
            sink,
        )?;
        self.text_win.drain(..cut);
        if self.text_win.capacity() > 8192 && self.text_win.len() <= 2048 {
            self.text_win.shrink_to(4096);
        }
        Ok(())
    }

    fn parse_slice<C: BlockConsumer>(
        seg: &str,
        fb2_mode: bool,
        done: &mut bool,
        skip_depth: &mut usize,
        sink: &mut C,
    ) -> Result<(), ParseError> {
        if !sink.wants_more() {
            *done = true;
            return Ok(());
        }
        let effective = if fb2_mode {
            if let Some(end_body) = find_ascii_nocase(seg, b"</body>") {
                *done = true;
                &seg[..end_body]
            } else {
                seg
            }
        } else {
            seg
        };
        let mut blocks = Vec::new();
        let mut toc = Vec::new();
        parse_xml_flow_with_state(effective, &mut blocks, &mut toc, fb2_mode, skip_depth);
        sink.push_blocks(blocks, toc)
    }

    pub(crate) fn finish<C: BlockConsumer>(&mut self, sink: &mut C) -> Result<(), ParseError> {
        if !sink.wants_more() {
            self.done = true;
            return Ok(());
        }
        if !self.byte_carry.is_empty() && !self.skipping_binary && !self.skipping_oversize_tag {
            let enc = self.enc.unwrap_or(TextEncoding::Utf8);
            self.text_win.push_str(&decode_with(&self.byte_carry, enc));
            self.byte_carry.clear();
        }
        self.drain_ready(true, sink)
    }
}

fn find_xml_flush_boundary(s: &str) -> usize {
    const SUFFIXES: [&[u8]; 13] = [
        b"</p>",
        b"</div>",
        b"</title>",
        b"</subtitle>",
        b"</v>",
        b"</h1>",
        b"</h2>",
        b"</h3>",
        b"</li>",
        b"</blockquote>",
        b"</section>",
        b"<empty-line/>",
        b"<empty-line />",
    ];
    let bytes = s.as_bytes();
    let mut i = bytes.len();
    while i > 0 {
        i -= 1;
        if bytes[i] == b'>' {
            let end = i + 1;
            for pat in SUFFIXES {
                if end >= pat.len() && bytes[end - pat.len()..end].eq_ignore_ascii_case(pat) {
                    return end;
                }
            }
        }
    }
    0
}

pub(crate) fn ceil_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

pub(crate) fn floor_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

/// Convert any book path on `StorageBackend` into a `.vvb` file with bounded `< 100 KB` peak RAM.
pub fn ingest_storage_to_vvb<S: StorageBackend>(
    storage: &mut S,
    source_path: &str,
    vvb_path: &str,
    forced_id_hex: Option<&str>,
) -> Result<VvbIndex, ParseError> {
    ingest_storage_to_vvb_with_progress(storage, source_path, vvb_path, forced_id_hex, |_, _| {})
}

/// Convert any book path on `StorageBackend` into a sparse-checkpoint `.vvb` file while reporting progress.
pub fn ingest_storage_to_vvb_with_progress<S: StorageBackend, F: FnMut(usize, usize)>(
    storage: &mut S,
    source_path: &str,
    vvb_path: &str,
    forced_id_hex: Option<&str>,
    mut on_progress: F,
) -> Result<VvbIndex, ParseError> {
    let fmt = InputFormat::from_filename(source_path).ok_or(ParseError::UnsupportedFormat)?;
    let file_size = storage
        .file_len(source_path)
        .map_err(|_| ParseError::MalformedDocument)?;
    if file_size == 0 {
        return Err(ParseError::EmptyDocument);
    }

    if fmt == InputFormat::Vvb {
        let probed = probe_book_storage(storage, source_path, file_size)?;
        let _ = probed;
        return VvbIndex::open_from_storage(storage, source_path)
            .map_err(|_| ParseError::MalformedDocument);
    }

    // Small file fast-path (< 64 KB on disk and uncompressed) avoids double temp writes.
    if file_size <= 64 * 1024 && !matches!(fmt, InputFormat::Fb2Zip | InputFormat::Epub) {
        let raw = read_storage_prefix(storage, source_path, file_size as usize)?;
        let mut book = ingest_with_format(source_path, fmt, &raw)?;
        if let Some(id) = forced_id_hex {
            book.meta.id_hex = String::from(id);
        }
        storage
            .write_atomic(vvb_path, &book.to_bytes())
            .map_err(|_| ParseError::MalformedDocument)?;
        on_progress(1, 1);
        return VvbIndex::open_from_storage(storage, vvb_path)
            .map_err(|_| ParseError::MalformedDocument);
    }

    let probed = probe_book_storage(storage, source_path, file_size)?;
    let id_hex = forced_id_hex
        .map(String::from)
        .unwrap_or_else(|| probed.id_hex.clone());
    let digest = *blake3::hash(source_path.as_bytes()).as_bytes();
    let preamble = build_vvb_sparse_trailer_preamble(digest);

    let _ = storage.remove(vvb_path);
    let mut sink = StreamSink::new(storage, vvb_path, &preamble);

    let res: Result<(), ParseError> = (|| {
        match fmt {
        InputFormat::Epub => {
            let zip = StorageZip::open(sink.storage, source_path, file_size)
                .map_err(|_| ParseError::InvalidArchive)?;
            if is_epub_drm_storage(sink.storage, &zip, source_path, None)
            {
                return Err(ParseError::UnsupportedFormat);
            }
            let file_size_u64 = zip.file_size();
            let c_raw = {
                let c_meta = zip
                    .find_entry("META-INF/container.xml")
                    .ok_or(ParseError::MalformedDocument)?;
                zip.read_entry_prefix(sink.storage, source_path, c_meta, 4096)
                    .map_err(|_| ParseError::InvalidArchive)?
            };
            let opf_path = {
                let (_, c_xml) = detect_and_decode_to_utf8(&c_raw, None);
                let raw_fp =
                    extract_attr(&c_xml, "full-path").ok_or(ParseError::MalformedDocument)?;
                normalize_zip_href(&percent_decode_href(&raw_fp))
            };
            drop(c_raw);
            let spine_hrefs = {
                let opf_meta = zip
                    .find_entry(&opf_path)
                    .ok_or(ParseError::MalformedDocument)?
                    .clone();
                let opf_dir = match opf_path.rfind('/') {
                    Some(idx) => &opf_path[..=idx],
                    None => "",
                };
                stream_epub_opf_spine(sink.storage, &zip, source_path, &opf_meta, opf_dir)?
            };
            if spine_hrefs.is_empty() {
                return Err(ParseError::MalformedDocument);
            }
            let mut hash_table: Vec<(u64, u32)> = spine_hrefs
                .iter()
                .enumerate()
                .map(|(i, h)| (fnv1a64_ascii_ci(h), i as u32))
                .collect();
            hash_table.sort_unstable();
            let mut resolved: Vec<Option<ZipEntryRef>> = vec![None; spine_hrefs.len()];
            zip.scan_cd_entries(sink.storage, source_path, |name, entry_ref| {
                let clean = name.trim_start_matches("./");
                let h = fnv1a64_ascii_ci(clean);
                let mut lo = hash_table.partition_point(|&(kh, _)| kh < h);
                while lo < hash_table.len() && hash_table[lo].0 == h {
                    let s_idx = hash_table[lo].1 as usize;
                    if spine_hrefs[s_idx].eq_ignore_ascii_case(clean) {
                        resolved[s_idx] = Some(entry_ref);
                    }
                    lo += 1;
                }
                Ok(())
            })
            .map_err(|_| ParseError::InvalidArchive)?;
            drop(hash_table);
            drop(spine_hrefs);
            drop(zip);

            let total_ch = resolved.len().max(1);
            let mut inflate_scratch = ZipInflateScratch::new();
            let mut filter = XmlChunkFilter::new(false);
            let mut chunk_buf = [0u8; 4096];
            for (ch_idx, maybe_ref) in resolved.into_iter().enumerate() {
                if let Some(entry_ref) = maybe_ref {
                    filter.reset_for_chapter();
                    let mut stream = StorageZip::open_entry_ref_stream(
                        file_size_u64,
                        sink.storage,
                        source_path,
                        entry_ref,
                        2 * 1024 * 1024,
                        Some(&mut inflate_scratch),
                    )
                    .map_err(|_| ParseError::InvalidArchive)?;
                    loop {
                        let n = stream
                            .next_chunk(sink.storage, &mut chunk_buf)
                            .map_err(|_| ParseError::InvalidArchive)?;
                        if n == 0 {
                            break;
                        }
                        filter.feed_bytes(&chunk_buf[..n], &mut sink)?;
                    }
                    filter.finish(&mut sink)?;
                }
                on_progress(ch_idx + 1, total_ch);
            }
            Ok(())
        }
        InputFormat::Fb2Zip => {
            let (file_size_u64, entry_ref) = {
                let zip = StorageZip::open(sink.storage, source_path, file_size)
                    .map_err(|_| ParseError::InvalidArchive)?;
                let fb2_entry = zip
                    .entries()
                    .iter()
                    .find(|e| e.name.to_ascii_lowercase().ends_with(".fb2"))
                    .or_else(|| zip.entries().first())
                    .ok_or(ParseError::MalformedDocument)?;
                (zip.file_size(), ZipEntryRef::from(fb2_entry))
            };
            let mut inflate_scratch = ZipInflateScratch::new();
            let mut filter = XmlChunkFilter::new(true);
            let mut stream = StorageZip::open_entry_ref_stream(
                file_size_u64,
                sink.storage,
                source_path,
                entry_ref,
                8 * 1024 * 1024,
                Some(&mut inflate_scratch),
            )
            .map_err(|_| ParseError::InvalidArchive)?;
            let mut chunk_buf = [0u8; 4096];
            while !filter.done {
                let n = stream
                    .next_chunk(sink.storage, &mut chunk_buf)
                    .map_err(|_| ParseError::InvalidArchive)?;
                if n == 0 {
                    break;
                }
                filter.feed_bytes(&chunk_buf[..n], &mut sink)?;
            }
            filter.finish(&mut sink)?;
            on_progress(1, 1);
            Ok(())
        }
        InputFormat::Fb2 | InputFormat::Html => {
            let is_fb2 = fmt == InputFormat::Fb2;
            let mut filter = XmlChunkFilter::new(is_fb2);
            let mut off = 0u64;
            let mut buf = [0u8; 4096];
            while off < file_size && !filter.done {
                let n = sink
                    .storage
                    .read_range(source_path, off, &mut buf)
                    .map_err(|_| ParseError::MalformedDocument)?;
                if n == 0 {
                    break;
                }
                off += n as u64;
                filter.feed_bytes(&buf[..n], &mut sink)?;
            }
            filter.finish(&mut sink)?;
            on_progress(1, 1);
            Ok(())
        }
        InputFormat::Markdown | InputFormat::Txt => {
            let is_md = fmt == InputFormat::Markdown;
            let mut enc: Option<TextEncoding> = None;
            let mut carry: Vec<u8> = Vec::new();
            let mut win = String::new();
            let mut off = 0u64;
            let mut buf = [0u8; 4096];
            while off < file_size {
                let n = sink
                    .storage
                    .read_range(source_path, off, &mut buf)
                    .map_err(|_| ParseError::MalformedDocument)?;
                if n == 0 {
                    break;
                }
                off += n as u64;
                carry.extend_from_slice(&buf[..n]);
                if enc.is_none() && carry.len() >= 64 {
                    let utf8_safe = trim_utf8_complete_prefix(&carry);
                    let probe = if utf8_safe >= 64 { &carry[..utf8_safe] } else { &carry[..] };
                    enc = Some(detect_encoding(probe, None));
                }
                let use_enc = enc.unwrap_or_else(|| {
                    let utf8_safe = trim_utf8_complete_prefix(&carry);
                    detect_encoding(&carry[..utf8_safe], None)
                });
                let usable = if use_enc == TextEncoding::Utf8 {
                    trim_utf8_complete_prefix(&carry)
                } else {
                    carry.len()
                };
                let chunk_str = decode_with(&carry[..usable], use_enc);
                carry = carry[usable..].to_vec();
                win.push_str(&chunk_str);
                if win.len() >= 8192 {
                    let cut_opt = win
                        .rfind("\n\n")
                        .or_else(|| win.rfind('\n'))
                        .map(|p| p + 1)
                        .or_else(|| {
                            let safe = floor_char_boundary(&win, 6144);
                            win[..safe].rfind(' ').map(|p| p + 1).or(Some(safe))
                        });
                    if let Some(cut) = cut_opt.filter(|&c| c > 0) {
                        let seg = win[..cut].to_string();
                        win.drain(..cut);
                        let mut parsed = if is_md {
                            parse_markdown(&seg, source_path)
                        } else {
                            parse_plain_text(&seg, source_path)
                        };
                        split_oversize_blocks(&mut parsed.blocks);
                        sink.push_blocks(parsed.blocks, parsed.toc)?;
                    }
                }
            }
            if !carry.is_empty() {
                win.push_str(&decode_with(&carry, enc.unwrap_or(TextEncoding::Utf8)));
            }
            if !win.trim().is_empty() {
                let mut parsed = if is_md {
                    parse_markdown(&win, source_path)
                } else {
                    parse_plain_text(&win, source_path)
                };
                split_oversize_blocks(&mut parsed.blocks);
                sink.push_blocks(parsed.blocks, parsed.toc)?;
            }
            on_progress(1, 1);
            Ok(())
        }
        InputFormat::Mobi => {
            let raw = sink
                .storage
                .read_all(source_path)
                .map_err(|_| ParseError::MalformedDocument)?;
            let book = crate::mobi::parse_mobi_in_memory(&raw, source_path)?;
            sink.push_blocks(book.blocks, book.toc)?;
            on_progress(1, 1);
            Ok(())
        }
        InputFormat::Vvb => unreachable!(),
        }?;
        sink.flush_bytes()
    })();

    if let Err(e) = res {
        let _ = sink.storage.remove(vvb_path);
        return Err(e);
    }

    let StreamSink {
        storage: st,
        block_count,
        checkpoint_shift,
        checkpoints,
        mut toc,
        stream_len,
        word_count,
        pending_bytes,
        ..
    } = sink;
    drop(pending_bytes);

    if block_count == 0 {
        let _ = st.remove(vvb_path);
        return Err(ParseError::EmptyDocument);
    }
    if toc.is_empty() {
        toc.push(TocEntry {
            title: probed.title.clone(),
            block_index: 0,
        });
    }

    let meta = BookMetadata {
        id_hex,
        title: probed.title,
        author: probed.author,
        language: probed.language,
        source_format: probed.source_format,
        word_count,
        cover_w: COVER_THUMB_W,
        cover_h: COVER_THUMB_H,
        cover_2bpp: vec![0xFFu8; (COVER_THUMB_W as usize * COVER_THUMB_H as usize) / 4],
    };

    let trailer_off = VVB_PREAMBLE_LEN.saturating_add(stream_len);
    let meta_buf = encode_vvb_meta_and_toc(&meta, &toc, block_count as u32);
    let mut out_buf = Vec::with_capacity(16384);
    let mut append_trailer_chunk = |bytes: &[u8], out: &mut Vec<u8>| -> Result<(), ParseError> {
        let mut pos = 0usize;
        while pos < bytes.len() {
            let room = 16384usize.saturating_sub(out.len()).max(1);
            let take = (bytes.len() - pos).min(room);
            out.extend_from_slice(&bytes[pos..pos + take]);
            pos += take;
            if out.len() >= 16384 {
                if st.append(vvb_path, out).is_err() {
                    return Err(ParseError::MalformedDocument);
                }
                out.clear();
            }
        }
        Ok(())
    };
    if append_trailer_chunk(&meta_buf, &mut out_buf).is_err() {
        let _ = st.remove(vvb_path);
        return Err(ParseError::MalformedDocument);
    }
    drop(meta_buf);
    if append_trailer_chunk(&[checkpoint_shift], &mut out_buf).is_err()
        || append_trailer_chunk(&(checkpoints.len() as u32).to_le_bytes(), &mut out_buf).is_err()
    {
        let _ = st.remove(vvb_path);
        return Err(ParseError::MalformedDocument);
    }
    for cp in &checkpoints {
        if append_trailer_chunk(&cp.to_le_bytes(), &mut out_buf).is_err() {
            let _ = st.remove(vvb_path);
            return Err(ParseError::MalformedDocument);
        }
    }
    if append_trailer_chunk(&trailer_off.to_le_bytes(), &mut out_buf).is_err()
        || append_trailer_chunk(&VVBT_FOOTER_MAGIC, &mut out_buf).is_err()
        || (!out_buf.is_empty() && st.append(vvb_path, &out_buf).is_err())
    {
        let _ = st.remove(vvb_path);
        return Err(ParseError::MalformedDocument);
    }

    Ok(VvbIndex {
        meta,
        toc,
        block_count,
        checkpoint_shift,
        block_checkpoints: checkpoints,
        block_stream_base: VVB_PREAMBLE_LEN,
        content_blake3: digest,
    })
}
