extern crate alloc;

use alloc::vec::Vec;

#[inline]
fn is_cyrillic_char(c: char) -> bool {
    matches!(c as u32, 0x0400..=0x04FF)
}

#[inline]
fn to_lower_cyr_en(c: char) -> char {
    match c {
        'А'..='Я' => char::from_u32(c as u32 + 32).unwrap_or(c),
        'Ё' => 'ё',
        'A'..='Z' => (c as u8 + 32) as char,
        _ => c,
    }
}

#[inline]
fn is_cyr_vowel(c: char) -> bool {
    matches!(
        to_lower_cyr_en(c),
        'а' | 'е' | 'ё' | 'и' | 'о' | 'у' | 'ы' | 'э' | 'ю' | 'я'
    )
}

#[inline]
fn is_cyr_sign_or_j(c: char) -> bool {
    matches!(to_lower_cyr_en(c), 'ь' | 'ъ' | 'й')
}

#[inline]
fn is_en_vowel(c: char) -> bool {
    matches!(to_lower_cyr_en(c), 'a' | 'e' | 'i' | 'o' | 'u' | 'y')
}

#[inline]
fn is_word_letter(c: char) -> bool {
    c.is_alphabetic()
}

/// Extract core alphabetic span `(start_byte, end_byte)` stripping outer punctuation.
fn core_letter_span(word: &str) -> Option<(usize, usize)> {
    let mut start = None;
    let mut end = 0usize;
    for (idx, ch) in word.char_indices() {
        if is_word_letter(ch) {
            if start.is_none() {
                start = Some(idx);
            }
            end = idx + ch.len_utf8();
        } else if start.is_some() && ch != '-' && ch != '‐' {
            // Stop before trailing punctuation like commas or guillemets
            if word[idx..].chars().all(|c| !is_word_letter(c)) {
                break;
            }
        }
    }
    start.map(|s| (s, end))
}

/// Zero-alloc stack-based hyphenation writing up to 16 split byte offsets into `out`.
/// Never leaves < 2 letters on either side, never orphans Russian `ь`/`ъ`/`й`,
/// and ensures both sides contain at least one vowel.
pub fn hyphen_points_into(word: &str, out: &mut [usize; 16]) -> usize {
    let Some((core_s, core_e)) = core_letter_span(word) else {
        return 0;
    };
    let core = &word[core_s..core_e];
    let mut chars = [(0usize, '\0'); 48];
    let mut n = 0usize;
    for (idx, ch) in core.char_indices() {
        if n >= chars.len() {
            return 0;
        }
        chars[n] = (idx, ch);
        n += 1;
    }
    if n < 5 {
        // Explicit existing hyphen inside compound words like "кое-что" is handled by tokenizer
        return 0;
    }

    let cyr_count = chars[..n].iter().filter(|(_, c)| is_cyrillic_char(*c)).count();
    if cyr_count >= n / 2 {
        hyphenate_cyrillic_into(core_s, &chars[..n], out)
    } else if chars[..n].iter().all(|(_, c)| c.is_ascii_alphabetic()) {
        hyphenate_english_into(core_s, &chars[..n], out)
    } else {
        0
    }
}

/// Returns valid hyphenation byte offsets inside `word` (never leaves < 2 letters on either side,
/// never orphans Russian `ь`/`ъ`/`й`, and ensures both sides contain at least one vowel).
pub fn hyphen_points(word: &str) -> Vec<usize> {
    let mut buf = [0usize; 16];
    let n = hyphen_points_into(word, &mut buf);
    buf[..n].to_vec()
}

#[inline]
fn push_hyp(out: &mut [usize; 16], count: &mut usize, val: usize) {
    if *count < out.len() {
        out[*count] = val;
        *count += 1;
    }
}

fn hyphenate_cyrillic_into(base: usize, chars: &[(usize, char)], out: &mut [usize; 16]) -> usize {
    let n = chars.len();
    let mut vowels_before = [0u8; 49];
    for (k, &(_, ch)) in chars.iter().enumerate() {
        vowels_before[k + 1] = vowels_before[k] + u8::from(is_cyr_vowel(ch));
    }
    let total_vowels = vowels_before[n];
    if total_vowels < 2 {
        return 0;
    }

    let mut out_len = 0usize;
    // Iterate candidate split index i (left = chars[..i], right = chars[i..])
    let mut i = 2usize;
    while i <= n - 2 {
        let left_v = vowels_before[i];
        let right_v = total_vowels - left_v;
        if left_v == 0 || right_v == 0 {
            i += 1;
            continue;
        }
        let prev = chars[i - 1].1;
        let curr = chars[i].1;

        // Rule 1: Never split before ь, ъ, й, or before non-letters
        if is_cyr_sign_or_j(curr) || !is_word_letter(curr) || !is_word_letter(prev) {
            i += 1;
            continue;
        }

        // Rule 2: Split right AFTER ь, ъ, й (e.g., силь-ный, май-ский, подъ-езд)
        if is_cyr_sign_or_j(prev) {
            push_hyp(out, &mut out_len, base + chars[i].0);
            i += 2;
            continue;
        }

        // Rule 3: Vowel followed by single consonant + vowel -> break before consonant (мо-ло-ко)
        if is_cyr_vowel(prev) {
            if is_cyr_vowel(curr) {
                // Between two vowels: те-атр
                push_hyp(out, &mut out_len, base + chars[i].0);
                i += 2;
                continue;
            }
            if i + 1 < n && is_cyr_vowel(chars[i + 1].1) {
                push_hyp(out, &mut out_len, base + chars[i].0);
                i += 2;
                continue;
            }
            // Vowel followed by consonant cluster: split after the FIRST consonant
            // unless next is ь/ъ/й (which stays glued to first consonant).
            if i + 2 < n && !is_cyr_vowel(chars[i + 1].1) {
                let c2 = chars[i + 1].1;
                if is_cyr_sign_or_j(c2) {
                    i += 1;
                    continue;
                }
                // Keep resonant+obstruent split cleanly after first consonant (кар-та, солн-це)
                push_hyp(out, &mut out_len, base + chars[i + 1].0);
                i += 2;
                continue;
            }
        } else if !is_cyr_vowel(curr) {
            // Double identical consonants (ван-на, рус-ский) or cluster break
            if to_lower_cyr_en(prev) == to_lower_cyr_en(curr) {
                push_hyp(out, &mut out_len, base + chars[i].0);
                i += 2;
                continue;
            }
        }

        i += 1;
    }
    out_len
}

fn hyphenate_english_into(base: usize, chars: &[(usize, char)], out: &mut [usize; 16]) -> usize {
    let n = chars.len();
    let mut lower = [0u8; 48];
    let mut vowels_before = [0u8; 49];
    for (k, &(_, ch)) in chars.iter().enumerate() {
        lower[k] = to_lower_cyr_en(ch) as u8;
        vowels_before[k + 1] = vowels_before[k] + u8::from(is_en_vowel(ch));
    }
    let lower_slice = &lower[..n];
    let total_v = vowels_before[n];
    if total_v < 2 {
        return 0;
    }

    let prefixes = [
        "under", "inter", "trans", "over", "dis", "mis", "pre", "pro", "sub", "un", "re", "in",
        "im",
    ];
    let suffixes = [
        "tion", "sion", "ment", "ness", "able", "ible", "less", "ing", "ful", "ous", "est",
    ];

    let mut cands = [0usize; 24];
    let mut cand_len = 0usize;
    let push_cand = |cands: &mut [usize; 24], cand_len: &mut usize, v: usize| {
        if *cand_len < cands.len() {
            cands[*cand_len] = v;
            *cand_len += 1;
        }
    };

    for p in prefixes {
        let pb = p.as_bytes();
        if lower_slice.starts_with(pb) && n - pb.len() >= 3 && vowels_before[pb.len()] >= 1 {
            push_cand(&mut cands, &mut cand_len, pb.len());
        }
    }
    for s in suffixes {
        let sb = s.as_bytes();
        if lower_slice.ends_with(sb)
            && n - sb.len() >= 3
            && total_v - vowels_before[n - sb.len()] >= 1
        {
            push_cand(&mut cands, &mut cand_len, n - sb.len());
        }
    }

    // Standard VC-CV & V-CV syllable boundaries
    for (i, &vb) in vowels_before.iter().enumerate().take(n - 1).skip(2) {
        if vb == 0 || total_v - vb == 0 {
            continue;
        }
        let prev = lower_slice[i - 1];
        let curr = lower_slice[i];
        let prev_v = is_en_vowel(prev as char);
        let curr_v = is_en_vowel(curr as char);

        // Protect digraphs
        let digraph = matches!(
            [prev, curr],
            [b'c', b'h']
                | [b's', b'h']
                | [b't', b'h']
                | [b'p', b'h']
                | [b'w', b'h']
                | [b'q', b'u']
                | [b'c', b'k']
        );
        if digraph {
            continue;
        }

        let vccv = !prev_v && !curr_v && i >= 2 && is_en_vowel(lower_slice[i - 2] as char);
        let vcv = prev_v && !curr_v && i + 1 < n && is_en_vowel(lower_slice[i + 1] as char) && i >= 3;
        if vccv || vcv {
            push_cand(&mut cands, &mut cand_len, i);
        }
    }

    // Insertion-sort stack candidates
    for i in 1..cand_len {
        let key = cands[i];
        let mut j = i;
        while j > 0 && cands[j - 1] > key {
            cands[j] = cands[j - 1];
            j -= 1;
        }
        cands[j] = key;
    }

    // Maintain at least 2 characters gap between splits
    let mut out_len = 0usize;
    let mut last_idx = 0usize;
    for &idx in &cands[..cand_len] {
        if idx >= 2 && n - idx >= 2 && (last_idx == 0 || idx >= last_idx + 2) {
            push_hyp(out, &mut out_len, base + chars[idx].0);
            last_idx = idx;
        }
    }
    out_len
}

/// Non-breaking 1-letter words in Russian & English that must cling to the NEXT word
/// rather than stranding at the end of a line.
pub fn must_glue_to_next(word: &str) -> bool {
    let trimmed = word.trim_start_matches(['«', '„', '“', '"', '(', '—']);
    matches!(
        trimmed,
        "в" | "В"
            | "к"
            | "К"
            | "с"
            | "С"
            | "у"
            | "У"
            | "о"
            | "О"
            | "и"
            | "И"
            | "а"
            | "А"
            | "я"
            | "Я"
            | "a"
            | "A"
            | "I"
            | "—"
    )
}
