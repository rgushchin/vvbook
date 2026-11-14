extern crate alloc;

use alloc::string::String;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextEncoding {
    Utf8,
    Windows1251,
    Koi8R,
    Koi8U,
    Cp866,
    Iso8859_1,
}

// Windows-1251 upper 128 bytes (0x80..=0xFF) -> Unicode char
#[rustfmt::skip]
const CP1251: [char; 128] = [
    '\u{0402}','\u{0403}','\u{201A}','\u{0453}','\u{201E}','\u{2026}','\u{2020}','\u{2021}',
    '\u{20AC}','\u{2030}','\u{0409}','\u{2039}','\u{040A}','\u{040C}','\u{040B}','\u{040F}',
    '\u{0452}','\u{2018}','\u{2019}','\u{201C}','\u{201D}','\u{2022}','\u{2013}','\u{2014}',
    ' ',       '\u{2122}','\u{0459}','\u{203A}','\u{045A}','\u{045C}','\u{045B}','\u{045F}',
    '\u{00A0}','\u{040E}','\u{045E}','\u{0408}','\u{00A4}','\u{0490}','\u{00A6}','\u{00A7}',
    '\u{0401}','\u{00A9}','\u{0404}','\u{00AB}','\u{00AC}','\u{00AD}','\u{00AE}','\u{0407}',
    '\u{00B0}','\u{00B1}','\u{0406}','\u{0456}','\u{0491}','\u{00B5}','\u{00B6}','\u{00B7}',
    '\u{0451}','\u{2116}','\u{0454}','\u{00BB}','\u{0458}','\u{0405}','\u{0455}','\u{0457}',
    'А','Б','В','Г','Д','Е','Ж','З','И','Й','К','Л','М','Н','О','П',
    'Р','С','Т','У','Ф','Х','Ц','Ч','Ш','Щ','Ъ','Ы','Ь','Э','Ю','Я',
    'а','б','в','г','д','е','ж','з','и','й','к','л','м','н','о','п',
    'р','с','т','у','ф','х','ц','ч','ш','щ','ъ','ы','ь','э','ю','я',
];

// KOI8-R upper 128 bytes (0x80..=0xFF) -> Unicode char (lowercase at 0xC0..0xDF, uppercase at 0xE0..0xFF)
#[rustfmt::skip]
const KOI8_R: [char; 128] = [
    '─','│','┌','┐','└','┘','├','┤','┬','┴','┼','▀','▄','█','▌','▐',
    '░','▒','▓','⌠','■','∙','√','≈','≤','≥','\u{00A0}','⌡','°','²','·','÷',
    '═','║','╒','ё','╓','╔','╕','╖','╗','╘','╙','╚','╛','╜','╝','╞',
    '╟','╠','╡','Ё','╢','╣','╤','╥','╦','╧','╨','╩','╪','╫','╬','©',
    'ю','а','б','ц','д','е','ф','г','х','и','й','к','л','м','н','о',
    'п','я','р','с','т','у','ж','в','ь','ы','з','ш','э','щ','ч','ъ',
    'Ю','А','Б','Ц','Д','Е','Ф','Г','Х','И','Й','К','Л','М','Н','О',
    'П','Я','Р','С','Т','У','Ж','В','Ь','Ы','З','Ш','Э','Щ','Ч','Ъ',
];

// CP866 upper 128 bytes (0x80..=0xFF)
#[rustfmt::skip]
const CP866: [char; 128] = [
    'А','Б','В','Г','Д','Е','Ж','З','И','Й','К','Л','М','Н','О','П',
    'Р','С','Т','У','Ф','Х','Ц','Ч','Ш','Щ','Ъ','Ы','Ь','Э','Ю','Я',
    'а','б','в','г','д','е','ж','з','и','й','к','л','м','н','о','п',
    '░','▒','▓','│','┤','╡','╢','╖','╕','╣','║','╗','╝','╜','╛','┐',
    '└','┴','┬','├','─','┼','╞','╟','╚','╔','╩','╦','╠','═','╬','╧',
    '╨','╤','╥','╙','╘','╒','╓','╫','╪','┘','┌','█','▄','▌','▐','▀',
    'р','с','т','у','ф','х','ц','ч','ш','щ','ъ','ы','ь','э','ю','я',
    'Ё','ё','Є','є','Ї','ї','Ў','ў','°','∙','·','√','№','¤','■','\u{00A0}',
];

pub fn parse_encoding_label(label: &str) -> Option<TextEncoding> {
    let lower = label.trim().to_ascii_lowercase();
    match lower.as_str() {
        "utf-8" | "utf8" => Some(TextEncoding::Utf8),
        "windows-1251" | "cp1251" | "win-1251" | "x-cp1251" => Some(TextEncoding::Windows1251),
        "koi8-r" | "koi8r" => Some(TextEncoding::Koi8R),
        "koi8-u" | "koi8u" => Some(TextEncoding::Koi8U),
        "cp866" | "ibm866" | "866" => Some(TextEncoding::Cp866),
        "iso-8859-1" | "latin1" | "latin-1" => Some(TextEncoding::Iso8859_1),
        _ => None,
    }
}

#[inline]
fn map_upper_byte(b: u8, enc: TextEncoding) -> char {
    match enc {
        TextEncoding::Windows1251 => CP1251[(b - 0x80) as usize],
        TextEncoding::Koi8R => KOI8_R[(b - 0x80) as usize],
        TextEncoding::Koi8U => match b {
            0xA4 => 'є',
            0xA6 => 'і',
            0xA7 => 'ї',
            0xAD => 'ґ',
            0xB4 => 'Є',
            0xB6 => 'І',
            0xB7 => 'Ї',
            0xBD => 'Ґ',
            _ => KOI8_R[(b - 0x80) as usize],
        },
        TextEncoding::Cp866 => CP866[(b - 0x80) as usize],
        TextEncoding::Iso8859_1 => b as char,
        TextEncoding::Utf8 => b as char,
    }
}

pub fn decode_with(raw: &[u8], enc: TextEncoding) -> String {
    let raw = raw.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(raw);
    if enc == TextEncoding::Utf8 {
        return String::from_utf8_lossy(raw).into_owned();
    }
    let cap: usize = raw
        .iter()
        .map(|&b| if b < 0x80 { 1 } else { map_upper_byte(b, enc).len_utf8() })
        .sum();
    let mut out = String::with_capacity(cap);
    for &b in raw {
        if b < 0x80 {
            out.push(b as char);
        } else {
            out.push(map_upper_byte(b, enc));
        }
    }
    out
}

/// Sniff XML `<?xml ... encoding="..."?>` or HTML `<meta charset="...">` in first 512 bytes.
pub fn sniff_declared_encoding(raw: &[u8]) -> Option<TextEncoding> {
    let head = &raw[..raw.len().min(768)];
    // Ascii-safe lowercase scan
    let mut ascii = String::with_capacity(head.len());
    for &b in head {
        ascii.push(if b.is_ascii() { (b as char).to_ascii_lowercase() } else { ' ' });
    }
    for key in ["encoding=\"", "encoding='", "charset=\"", "charset='", "charset="] {
        if let Some(pos) = ascii.find(key) {
            let rest = &ascii[pos + key.len()..];
            let end = rest
                .find(['"', '\'', ';', ' ', '>', '\r', '\n'])
                .unwrap_or(rest.len());
            if let Some(enc) = parse_encoding_label(&rest[..end]) {
                return Some(enc);
            }
        }
    }
    None
}

/// Score decoded text for natural Cyrillic prose (CP1251 vs KOI8-R vs CP866 vs Latin1).
fn score_cyrillic_readability(bytes: &[u8], enc: TextEncoding) -> i64 {
    let sample = &bytes[..bytes.len().min(8192)];
    let mut score = 0i64;
    let mut prev_lower = false;
    let mut prev_upper = false;
    for &b in sample {
        if b < 0x80 {
            prev_lower = false;
            prev_upper = false;
            continue;
        }
        let ch = match enc {
            TextEncoding::Windows1251 => CP1251[(b - 0x80) as usize],
            TextEncoding::Koi8R => KOI8_R[(b - 0x80) as usize],
            TextEncoding::Cp866 => CP866[(b - 0x80) as usize],
            _ => b as char,
        };
        let is_lower = matches!(ch, 'а'..='я' | 'ё' | 'є' | 'і' | 'ї' | 'ґ' | 'ў');
        let is_upper = matches!(ch, 'А'..='Я' | 'Ё' | 'Є' | 'І' | 'Ї' | 'Ґ' | 'Ў');

        if is_lower {
            score += 4;
            if prev_lower || prev_upper {
                score += 6; // natural word casing (Titlecase or lowercase run)
            }
            // Top Russian letter frequency bonus (о, е, а, и, н, т, с, р, в, л)
            if matches!(ch, 'о' | 'е' | 'а' | 'и' | 'н' | 'т' | 'с' | 'р' | 'в' | 'л') {
                score += 5;
            }
            // Rare in mid-word positions penalty if very common
            if matches!(ch, 'ъ' | 'э' | 'ф') {
                score -= 1;
            }
        } else if is_upper {
            score += 1;
            if prev_lower {
                // lowercase immediately followed by UPPERCASE inside a word -> strong sign of wrong codepage (e.g., KOI8 vs 1251!)
                score -= 18;
            }
        } else if matches!(ch, '«' | '»' | '—' | '–' | '…' | '№') {
            score += 6;
        } else if matches!(ch, '─'..='╬') {
            // Box-drawing characters in prose -> almost certainly wrong codepage (KOI8-R vs CP1251/CP866)
            score -= 20;
        } else {
            score -= 3;
        }

        prev_lower = is_lower;
        prev_upper = is_upper;
    }
    score
}

pub fn detect_encoding(raw: &[u8], hint: Option<&str>) -> TextEncoding {
    if raw.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return TextEncoding::Utf8;
    }
    if let Some(h) = hint.and_then(parse_encoding_label)
        && (h != TextEncoding::Utf8 || core::str::from_utf8(raw).is_ok()) {
            return h;
        }
    if let Some(declared) = sniff_declared_encoding(raw)
        && (declared != TextEncoding::Utf8 || core::str::from_utf8(raw).is_ok()) {
            return declared;
        }
    if core::str::from_utf8(raw).is_ok() {
        return TextEncoding::Utf8;
    }

    // Score legacy 8-bit Cyrillic encodings
    let candidates = [
        TextEncoding::Windows1251,
        TextEncoding::Koi8R,
        TextEncoding::Cp866,
    ];
    let mut best = TextEncoding::Windows1251;
    let mut best_score = i64::MIN;
    for enc in candidates {
        let s = score_cyrillic_readability(raw, enc);
        if s > best_score {
            best_score = s;
            best = enc;
        }
    }
    if best_score <= 0 {
        TextEncoding::Iso8859_1
    } else {
        best
    }
}

pub fn detect_and_decode_to_utf8(raw: &[u8], hint: Option<&str>) -> (TextEncoding, String) {
    let enc = detect_encoding(raw, hint);
    (enc, decode_with(raw, enc))
}
