#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod i18n;

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
pub use i18n::{UiLang, tr};
use vvbook_format::{
    Block, CatalogEntry, DirectBook, InputFormat, PageAnchor, VvbIndex, ZipInflateScratch,
    catalog_format_version, decode_catalog, decode_layout_cache, encode_catalog,
    encode_layout_cache, ingest_storage_to_vvb_with_progress, intern_language,
    intern_source_format, probe_book_storage, probe_book_storage_with_scratch,
};
use vvbook_games::{ChessApp, GameAction, PipProgress, PocketPipApp};
use vvbook_gfx::baked_fonts;
use vvbook_gfx::canvas::{Align, Canvas, render_to_sink};
use vvbook_gfx::icons::draw_battery_capsule;
use vvbook_gfx::qr::QrCode;
use vvbook_pal::{
    BatteryStatus, Button, DateTime, DeviceSensors, DisplaySink, EntropySource, InputEvent,
    NetworkPortal, PORTRAIT_H, PORTRAIT_W, PortalStatus, PowerControl, RefreshMode, SAFE_INSETS,
    StorageBackend, Tone,
};
use vvbook_security::{AtomicJournal, VvDropAuth};
use vvbook_typeset::{
    FontFamily, FontSizePreset, MarginPreset, PageChrome, PageLayout, ReaderTypography,
    layout_page_slice, layout_page_with, paginate_blocks_slice, paginate_from_anchor, render_page,
};

const JOURNAL_BASE: &str = "/.vvbook/state";
const GAMES_JOURNAL_BASE: &str = "/.vvbook/games";
const PROGRESS_JOURNAL_BASE: &str = "/.vvbook/progress";
const CATALOG_PATH: &str = "/.vvbook/catalog.vvc";
const PREFETCH_WINDOW_BLOCKS: usize = 16;
const VVDROP_URL: &str = "http://192.168.4.1";

fn make_wifi_qr(ssid: &str, key: &str) -> Option<QrCode> {
    let payload = format!("WIFI:T:WPA;S:{ssid};P:{key};;");
    QrCode::encode(payload.as_bytes())
}
pub const DIRECT_STREAM_MIN_BYTES: u64 = 96 * 1024;
pub const DIRECT_STREAM_ZIP_MIN_BYTES: u64 = 48 * 1024;
pub const LIBRARY_PER_PAGE: usize = 8;
const DIRECT_SEC_SHIFT: u32 = 8;

/// Returns `true` when `dir` is a valid microSD book directory (excluding hidden/OS/sidecar/recovery dirs).
pub fn is_valid_library_dir(dir: &str) -> bool {
    let name = dir.trim();
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.starts_with('.')
        || name.contains('\u{FFFD}')
        || name.chars().any(|c| c < ' ' || matches!(c, '\x7f' | '/' | '\\'))
    {
        return false;
    }
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".sdr") || lower.starts_with("found.") {
        return false;
    }
    !matches!(
        lower.as_str(),
        "$recycle.bin"
            | "system volume information"
            | "lost.dir"
            | "__macosx"
            | "crosspoint"
            | "koreader"
            | "sleep"
            | "trash"
            | ".trashes"
            | "android"
            | "calnotes"
            | ".calnotes"
            | "sdr"
            | "recycled"
            | "recycler"
    )
}

/// Returns `true` when `path` points to a supported user book inside valid folders on microSD.
pub fn is_valid_library_book_path(path: &str) -> bool {
    let trimmed = path.trim();
    if trimmed.is_empty()
        || trimmed.contains('\u{FFFD}')
        || trimmed.chars().any(|c| c < ' ' || c == '\x7f')
    {
        return false;
    }
    let mut parts = trimmed.trim_start_matches('/').split('/').peekable();
    while let Some(seg) = parts.next() {
        if seg.is_empty() {
            return false;
        }
        if parts.peek().is_some() {
            if !is_valid_library_dir(seg) {
                return false;
            }
        } else {
            if seg.starts_with('.') || seg.starts_with("._") || seg.ends_with('~') {
                return false;
            }
            let lower = seg.to_ascii_lowercase();
            if matches!(
                lower.as_str(),
                "desktop.ini"
                    | "thumbs.db"
                    | "autorun.inf"
                    | "indexervolumeguid"
                    | "wpsettings.dat"
                    | "metadata.opf"
                    | "metadata_db_prefs_backup.json"
            ) || lower == "my clippings.txt"
                || (lower.starts_with("my clippings") && lower.ends_with(".txt"))
            {
                return false;
            }
            return InputFormat::from_filename(seg).is_some();
        }
    }
    false
}

fn strip_suffix_ignore_ascii_case<'a>(s: &'a str, suffix: &str) -> Option<&'a str> {
    if s.len() >= suffix.len() {
        let split = s.len() - suffix.len();
        if s.is_char_boundary(split) && s[split..].eq_ignore_ascii_case(suffix) {
            return Some(&s[..split]);
        }
    }
    None
}

fn sibling_dedup_hash_and_prio(path: &str) -> (u64, u8) {
    let trimmed = path.trim();
    let (dir, name) = match trimmed.rsplit_once('/') {
        Some((d, n)) => (d, n),
        None => ("", trimmed),
    };
    let (stem, prio) = if let Some(s) = strip_suffix_ignore_ascii_case(name, ".fb2.zip") {
        (s, 3u8)
    } else if let Some(s) = strip_suffix_ignore_ascii_case(name, ".vvb") {
        (s, 0u8)
    } else if let Some(s) = strip_suffix_ignore_ascii_case(name, ".epub") {
        (s, 1u8)
    } else if let Some(s) = strip_suffix_ignore_ascii_case(name, ".fb2") {
        (s, 2u8)
    } else if let Some(s) = strip_suffix_ignore_ascii_case(name, ".azw3") {
        (s, 4u8)
    } else if let Some(s) = strip_suffix_ignore_ascii_case(name, ".mobi") {
        (s, 5u8)
    } else if let Some(s) = strip_suffix_ignore_ascii_case(name, ".prc") {
        (s, 6u8)
    } else if let Some(s) = strip_suffix_ignore_ascii_case(name, ".azw") {
        (s, 7u8)
    } else if let Some(s) = strip_suffix_ignore_ascii_case(name, ".markdown") {
        (s, 8u8)
    } else if let Some(s) = strip_suffix_ignore_ascii_case(name, ".md") {
        (s, 8u8)
    } else if let Some(s) = strip_suffix_ignore_ascii_case(name, ".xhtml") {
        (s, 9u8)
    } else if let Some(s) = strip_suffix_ignore_ascii_case(name, ".html") {
        (s, 9u8)
    } else if let Some(s) = strip_suffix_ignore_ascii_case(name, ".htm") {
        (s, 9u8)
    } else if let Some(s) = strip_suffix_ignore_ascii_case(name, ".txt") {
        (s, 10u8)
    } else if let Some((s, _)) = name.rsplit_once('.') {
        (s, 11u8)
    } else {
        (name, 11u8)
    };
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for b in dir.bytes() {
        h ^= u64::from(b.to_ascii_lowercase());
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h ^= 0xFF;
    h = h.wrapping_mul(0x0100_0000_01b3);
    for b in stem.bytes() {
        h ^= u64::from(b.to_ascii_lowercase());
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    (h, prio)
}

fn is_unknown_author(author: &str) -> bool {
    let a = author.trim();
    a.is_empty()
        || a.eq_ignore_ascii_case("unknown")
        || a.eq_ignore_ascii_case("unknown author")
}

fn fallback_folder_author(source_path: &str) -> String {
    let dir = source_path
        .rsplit_once('/')
        .map(|(d, _)| d)
        .unwrap_or("")
        .trim_matches('/');
    let leaf = dir.rsplit('/').next().unwrap_or("").trim();
    let stripped = if let Some(idx) = leaf.rfind(" (") {
        let rest = &leaf[idx + 2..];
        if let Some(inner) = rest.strip_suffix(')')
            && !inner.is_empty()
            && inner.bytes().all(|b| b.is_ascii_digit())
        {
            leaf[..idx].trim()
        } else {
            leaf
        }
    } else {
        leaf
    };
    if stripped.is_empty() {
        String::from("MicroSD")
    } else {
        String::from(stripped)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    Home,
    Library,
    Reader,
    VvDrop,
    PocketPip,
    Chess,
    Settings,
    SleepPoster,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SleepPosterKind {
    Literary = 0,
    Cupertino = 1,
    PipStarlight = 2,
}

impl SleepPosterKind {
    pub fn from_u8(v: u8) -> Self {
        match v % 3 {
            1 => Self::Cupertino,
            2 => Self::PipStarlight,
            _ => Self::Literary,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReaderOverlay {
    None,
    AaHud,
    TocDrawer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PowerProfileStats {
    pub panel_hv_standby: bool,
    pub sd_power_save: bool,
    pub imu_powered: bool,
    pub wifi_powered: bool,
    pub pages_from_prefetch: u32,
    pub journal_flushes: u32,
}

pub struct FolioApp {
    pub screen: Screen,
    pub prev_screen_before_sleep: Screen,
    pub lang: UiLang,
    pub typo: ReaderTypography,
    pub full_scrub_interval: u8,
    pub tilt_flick_enabled: bool,
    pub auto_sleep_mins: u8,
    pub sleep_poster: SleepPosterKind,

    // Home & Library state
    pub home_cursor: usize,
    pub catalog: Vec<CatalogEntry>,
    pub active_book_idx: usize,
    pub lib_cursor: usize,
    pub library_notice: Option<String>,
    pub home_cover_cache: Option<(String, Vec<u8>)>,
    last_open_diag: Option<String>,

    // Active Reader streaming state + 16-block SRAM prefetch window + zero-write DirectBook!
    pub active_index: Option<VvbIndex>,
    pub active_direct: Option<DirectBook>,
    pub direct_section_idx: usize,
    pub direct_section_blocks: Vec<Block>,
    pub direct_avg_pages_per_sec: usize,
    pub active_book_id: Option<String>,
    pub active_typo_hash: u32,
    pub active_pages: Vec<PageAnchor>,
    pub pending_layout_anchor: Option<PageAnchor>,
    pub current_page_num: usize,
    pub current_layout: Option<PageLayout>,
    pub prefetch_cache: BTreeMap<usize, Block>,
    pub reader_overlay: ReaderOverlay,
    pub hud_cursor: usize,
    pub toc_cursor: usize,
    pub fast_turns_since_scrub: u8,
    pub unique_pages_read_in_session: u16,
    pub pending_journal_turns: u8,
    pub persistence_dirty: bool,

    // Apps & Portal
    pub vvdrop_auth: Option<VvDropAuth>,
    pub vvdrop_qr: Option<QrCode>,
    pub vvdrop_status: Option<PortalStatus>,
    pub vvdrop_client_badge_due_ms: Option<u64>,
    pub pip: PocketPipApp,
    pub chess: ChessApp,
    pub last_games_hash: u64,
    pub last_progress_hash: u64,
    pub resume_screen_code: u8,

    // Settings UI & telemetry
    pub settings_cursor: usize,
    pub journal_seq: u32,
    pub battery: BatteryStatus,
    pub clock: DateTime,
    pub last_battery_poll_ms: u64,
    pub last_activity_ms: u64,
    pub last_vvdrop_activity_ms: u64,
    pub power_stats: PowerProfileStats,
}

struct NullSink;

impl DisplaySink for NullSink {
    fn begin_frame(&mut self, _mode: RefreshMode) {}
    fn write_strip_2bpp(&mut self, _y_start: usize, _rows: usize, _packed_2bpp: &[u8]) {}
    fn end_frame(&mut self, _mode: RefreshMode) {}
}

fn draw_opening_placard_to_sink<D: DisplaySink, P: PowerControl>(
    sink: &mut D,
    power: &mut P,
    panel_hv_standby: &mut bool,
    title: &str,
    opening_label: &str,
    done: usize,
    total: usize,
) {
    if *panel_hv_standby {
        power.set_panel_hv_standby(false);
        *panel_hv_standby = false;
    }
    render_to_sink(sink, RefreshMode::FastDu, |canvas| {
        canvas.clear(Tone::Paper);
        let left = 36;
        let right = PORTRAIT_W as i32 - 36;
        let col_w = right - left;

        canvas.fill_rect(left, 304, col_w, 4, Tone::Ink);
        canvas.draw_text(
            &baked_fonts::FONT_UI_19_SEMI,
            left,
            338,
            opening_label,
            Tone::Ink,
        );
        if let Some(pct) = (done.min(total) * 100).checked_div(total) {
            let pct_s = format!("{pct}%");
            canvas.draw_text_aligned(
                &baked_fonts::FONT_MONO_20_SEMI,
                right,
                338,
                &pct_s,
                Tone::Ink,
                Align::Right,
            );
        }

        let title_lines =
            canvas.draw_text_lines(&baked_fonts::FONT_SERIF_32_BOLD, left, 382, col_w, 40, 2, title, Tone::Ink);
        let bar_y = if title_lines > 1 { 456 } else { 424 };
        canvas.stroke_rect(left, bar_y, col_w, 10, 1, Tone::Ink);
        if total > 0 && done > 0 {
            let fill_w = (col_w * done.min(total) as i32) / total as i32;
            if fill_w > 0 {
                canvas.fill_rect(left, bar_y, fill_w.min(col_w), 10, Tone::Ink);
            }
        }
        canvas.hline(left, bar_y + 32, col_w, Tone::Ink);
    });
}

impl Default for FolioApp {
    fn default() -> Self {
        Self::new()
    }
}

impl FolioApp {
    pub fn new() -> Self {
        Self {
            screen: Screen::Home,
            prev_screen_before_sleep: Screen::Home,
            lang: UiLang::En,
            typo: ReaderTypography::default(),
            full_scrub_interval: 12,
            tilt_flick_enabled: false, // Eco off by default!
            auto_sleep_mins: 10,
            sleep_poster: SleepPosterKind::Literary,
            home_cursor: 0,
            catalog: Vec::new(),
            active_book_idx: 0,
            lib_cursor: 0,
            library_notice: None,
            home_cover_cache: None,
            last_open_diag: None,
            active_index: None,
            active_direct: None,
            direct_section_idx: 0,
            direct_section_blocks: Vec::new(),
            direct_avg_pages_per_sec: 1,
            active_book_id: None,
            active_typo_hash: 0,
            active_pages: Vec::new(),
            pending_layout_anchor: None,
            current_page_num: 0,
            current_layout: None,
            prefetch_cache: BTreeMap::new(),
            reader_overlay: ReaderOverlay::None,
            hud_cursor: 0,
            toc_cursor: 0,
            fast_turns_since_scrub: 0,
            unique_pages_read_in_session: 0,
            pending_journal_turns: 0,
            persistence_dirty: false,
            vvdrop_auth: None,
            vvdrop_qr: make_wifi_qr("VVBook-Folio", "48291037"),
            vvdrop_status: None,
            vvdrop_client_badge_due_ms: None,
            pip: PocketPipApp::new(PipProgress::default()),
            chess: ChessApp::new(),
            last_games_hash: 0,
            last_progress_hash: 0,
            resume_screen_code: 0,
            settings_cursor: 0,
            journal_seq: 0,
            battery: BatteryStatus {
                percent: 96,
                millivolts: 4160,
                current_ma: -2,
                charging: false,
            },
            clock: DateTime {
                year: 2026,
                month: 11,
                day: 12,
                hour: 20,
                minute: 15,
                second: 0,
            },
            last_battery_poll_ms: 0,
            last_activity_ms: 0,
            last_vvdrop_activity_ms: 0,
            power_stats: PowerProfileStats {
                panel_hv_standby: false,
                sd_power_save: false,
                imu_powered: false,
                wifi_powered: false,
                pages_from_prefetch: 0,
                journal_flushes: 0,
            },
        }
    }

    pub fn take_last_open_diag(&mut self) -> Option<String> {
        self.last_open_diag.take()
    }

    pub fn boot<D, S, E, P, W, R>(
        &mut self,
        sink: &mut D,
        storage: &mut S,
        sensors: &mut E,
        power: &mut P,
        portal: &mut W,
        _entropy: &mut R,
    ) where
        D: DisplaySink,
        S: StorageBackend,
        E: DeviceSensors,
        P: PowerControl,
        W: NetworkPortal,
        R: EntropySource,
    {
        power.set_sd_power_save(false);
        let _ = storage.mkdir_all("/.vvbook");
        let _ = storage.mkdir_all("/.vvbook/books");
        let _ = storage.mkdir_all("/.vvbook/layout");
        let _ = storage.mkdir_all("/books");

        // Restore crash-proof AtomicJournal & Game state journal
        if let Some(data) = AtomicJournal::load(storage, JOURNAL_BASE)
            && data.len() >= 32
        {
            self.decode_journal_state(&data);
        }
        if let Some(gdata) = AtomicJournal::load(storage, GAMES_JOURNAL_BASE) {
            self.decode_games_bundle(&gdata);
        }
        self.last_games_hash = Self::hash_bytes(&self.encode_games_bundle());

        let mut had_legacy_v2 = false;
        let mut legacy_progress: Vec<CatalogEntry> = Vec::new();
        if let Ok(raw_cat) = storage.read_all(CATALOG_PATH) {
            match catalog_format_version(&raw_cat) {
                Some(3) => {
                    if let Ok(entries) = decode_catalog(&raw_cat) {
                        self.catalog = entries;
                    }
                }
                Some(2) => {
                    had_legacy_v2 = true;
                    if let Ok(entries) = decode_catalog(&raw_cat) {
                        legacy_progress = entries
                            .into_iter()
                            .filter(|e| {
                                e.last_block > 0
                                    || e.last_word > 0
                                    || e.bookmarked_block.is_some()
                                    || e.current_page_cached > 0
                            })
                            .collect();
                    }
                    self.catalog.clear();
                }
                _ => {
                    self.catalog.clear();
                }
            }
        }
        let prog_data = AtomicJournal::load(storage, PROGRESS_JOURNAL_BASE);
        if let Some(ref pdata) = prog_data {
            self.apply_progress_bundle(pdata);
        }
        self.scan_and_ingest_books(storage);
        if had_legacy_v2 {
            for c in &mut self.catalog {
                if let Some(old) = legacy_progress.iter().find(|o| {
                    o.id_hex == c.id_hex
                        || (!c.source_path.is_empty() && o.source_path == c.source_path)
                }) {
                    c.last_block = old.last_block;
                    c.last_word = old.last_word;
                    c.total_pages_cached = old.total_pages_cached;
                    c.current_page_cached = old.current_page_cached;
                    c.bookmarked_block = old.bookmarked_block;
                }
            }
            let _ = storage.write_atomic(CATALOG_PATH, &encode_catalog(&self.catalog));
        }
        if let Some(pdata) = prog_data {
            self.apply_progress_bundle(&pdata);
        } else if had_legacy_v2 {
            self.sync_progress_journal(storage);
        }
        self.last_progress_hash = Self::hash_bytes(&self.encode_progress_bundle());
        if self.active_book_idx >= self.catalog.len() {
            self.active_book_idx = 0;
        }
        self.refresh_home_cover(storage);

        self.battery = sensors.battery();
        self.clock = sensors.now();
        let up = sensors.uptime_ms();
        self.last_battery_poll_ms = up;
        self.last_activity_ms = up;

        self.screen = match self.resume_screen_code {
            1 => {
                if !self.catalog.is_empty()
                    && self.open_active_book_interactive(None::<&mut D>, storage, power)
                {
                    self.reader_overlay = ReaderOverlay::None;
                    Screen::Reader
                } else {
                    Screen::Home
                }
            }
            2 => Screen::Chess,
            3 => Screen::PocketPip,
            _ => Screen::Home,
        };

        // Strictly turn off Wi-Fi on boot; keep IMU gated unless resuming Reader with Tilt Turn.
        portal.stop();
        self.vvdrop_status = None;
        self.vvdrop_client_badge_due_ms = None;
        let want_imu = self.screen == Screen::Reader && self.tilt_flick_enabled;
        power.set_imu_flick(want_imu);
        power.set_sd_power_save(true);
        self.power_stats.wifi_powered = false;
        self.power_stats.imu_powered = want_imu;
        self.power_stats.sd_power_save = true;

        self.redraw(sink, RefreshMode::FullGc, power);
    }

    fn hash_bytes(bytes: &[u8]) -> u64 {
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        for &b in bytes {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        h
    }

    fn screen_to_resume_code(scr: Screen) -> u8 {
        match scr {
            Screen::Reader => 1,
            Screen::Chess => 2,
            Screen::PocketPip => 3,
            _ => 0,
        }
    }

    fn encode_games_bundle(&self) -> Vec<u8> {
        let chess = self.chess.encode_state();
        let pip = self.pip.encode_state();
        let chess_len = chess.len().min(u16::MAX as usize);
        let pip_len = pip.len().min(u16::MAX as usize);
        let mut out = Vec::with_capacity(9 + chess_len + pip_len);
        out.extend_from_slice(b"VVGM");
        out.push(1);
        out.extend_from_slice(&(chess_len as u16).to_le_bytes());
        out.extend_from_slice(&chess[..chess_len]);
        out.extend_from_slice(&(pip_len as u16).to_le_bytes());
        out.extend_from_slice(&pip[..pip_len]);
        out
    }

    fn decode_games_bundle(&mut self, buf: &[u8]) {
        if buf.len() < 9 || &buf[..4] != b"VVGM" || buf[4] != 1 {
            return;
        }
        let mut p = 5usize;
        let chess_len = u16::from_le_bytes([buf[p], buf[p + 1]]) as usize;
        p += 2;
        if p + chess_len + 2 > buf.len() {
            return;
        }
        let _ = self.chess.decode_state(&buf[p..p + chess_len]);
        p += chess_len;
        let pip_len = u16::from_le_bytes([buf[p], buf[p + 1]]) as usize;
        p += 2;
        if p + pip_len > buf.len() {
            return;
        }
        let _ = self.pip.decode_state(&buf[p..p + pip_len]);
    }

    fn has_persisted_progress(c: &CatalogEntry) -> bool {
        c.last_block > 0
            || c.last_word > 0
            || c.current_page_cached > 0
            || c.total_pages_cached > 0
            || c.bookmarked_block.is_some()
    }

    fn encode_progress_bundle(&self) -> Vec<u8> {
        let count = self
            .catalog
            .iter()
            .filter(|c| Self::has_persisted_progress(c))
            .count()
            .min(u16::MAX as usize);
        let mut out = Vec::with_capacity(7 + count * 26);
        out.extend_from_slice(b"VVPR");
        out.push(1);
        out.extend_from_slice(&(count as u16).to_le_bytes());
        for c in self
            .catalog
            .iter()
            .filter(|c| Self::has_persisted_progress(c))
            .take(count)
        {
            let h = Self::hash_bytes(c.id_hex.as_bytes());
            out.extend_from_slice(&h.to_le_bytes());
            out.extend_from_slice(&c.last_block.to_le_bytes());
            out.extend_from_slice(&c.last_word.to_le_bytes());
            out.extend_from_slice(&c.total_pages_cached.to_le_bytes());
            out.extend_from_slice(&c.current_page_cached.to_le_bytes());
            out.extend_from_slice(&c.bookmarked_block.unwrap_or(u32::MAX).to_le_bytes());
        }
        out
    }

    fn apply_progress_bundle(&mut self, buf: &[u8]) {
        if buf.len() < 7 || &buf[..4] != b"VVPR" || buf[4] != 1 {
            return;
        }
        let count = u16::from_le_bytes([buf[5], buf[6]]) as usize;
        let Some(end) = 7usize.checked_add(count.saturating_mul(26)) else {
            return;
        };
        if buf.len() < end {
            return;
        }
        let records = &buf[7..end];
        for c in &mut self.catalog {
            c.last_block = 0;
            c.last_word = 0;
            c.current_page_cached = 0;
            c.bookmarked_block = None;
            let h = Self::hash_bytes(c.id_hex.as_bytes());
            for rec in records.as_chunks::<26>().0 {
                let rh = u64::from_le_bytes([
                    rec[0], rec[1], rec[2], rec[3], rec[4], rec[5], rec[6], rec[7],
                ]);
                if rh == h {
                    c.last_block = u32::from_le_bytes([rec[8], rec[9], rec[10], rec[11]]);
                    c.last_word = u16::from_le_bytes([rec[12], rec[13]]);
                    let total = u32::from_le_bytes([rec[14], rec[15], rec[16], rec[17]]);
                    if total > 0 {
                        c.total_pages_cached = total;
                    }
                    c.current_page_cached =
                        u32::from_le_bytes([rec[18], rec[19], rec[20], rec[21]]);
                    let bm = u32::from_le_bytes([rec[22], rec[23], rec[24], rec[25]]);
                    c.bookmarked_block = if bm == u32::MAX { None } else { Some(bm) };
                    break;
                }
            }
        }
    }

    fn sync_progress_journal<S: StorageBackend>(&mut self, storage: &mut S) {
        let prog_blob = self.encode_progress_bundle();
        let prog_hash = Self::hash_bytes(&prog_blob);
        if prog_hash != self.last_progress_hash
            && AtomicJournal::commit(storage, PROGRESS_JOURNAL_BASE, &prog_blob).is_ok()
        {
            self.last_progress_hash = prog_hash;
        }
    }

    fn encode_journal_state(&self) -> Vec<u8> {
        let mut buf = vec![0u8; 40];
        buf[0] = self.lang as u8;
        buf[1] = self.typo.font_family as u8;
        buf[2] = self.typo.font_size as u8;
        buf[3] = self.typo.margin_preset as u8;
        buf[4] = self.typo.line_spacing_q4;
        buf[5] = (self.typo.justify as u8)
            | ((self.typo.hyphenate as u8) << 1)
            | ((self.typo.drop_cap as u8) << 2);
        buf[6] = self.full_scrub_interval;
        buf[7] = self.tilt_flick_enabled as u8;
        buf[8] = self.auto_sleep_mins;
        buf[9] = self.sleep_poster as u8;
        buf[10..14].copy_from_slice(&(self.active_book_idx as u32).to_le_bytes());
        let eff = if self.screen == Screen::SleepPoster {
            self.prev_screen_before_sleep
        } else {
            self.screen
        };
        buf[14] = Self::screen_to_resume_code(eff);
        let pip_bytes = self.pip.progress.encode();
        buf[16..32].copy_from_slice(&pip_bytes);
        buf[39] = 2; // Journal v2 (0.3.0-jobs 257-PPI typography defaults)
        buf
    }

    fn decode_journal_state(&mut self, buf: &[u8]) {
        if buf.len() < 32 {
            return;
        }
        self.lang = UiLang::from_u8(buf[0]);
        if buf.get(39) == Some(&2) {
            self.typo.font_family = match buf[1] {
                0 => FontFamily::Serif,
                2 => FontFamily::Sans,
                _ => FontFamily::School,
            };
            self.typo.font_size = match buf[2] {
                0 => FontSizePreset::Compact,
                2 => FontSizePreset::Large,
                3 => FontSizePreset::Extra,
                _ => FontSizePreset::Classic,
            };
            self.typo.margin_preset = match buf[3] {
                0 => MarginPreset::Narrow,
                2 => MarginPreset::Wide,
                _ => MarginPreset::Editorial,
            };
            if buf[4] >= 18 && buf[4] <= 28 {
                self.typo.line_spacing_q4 = buf[4];
            }
            self.typo.justify = (buf[5] & 1) != 0;
            self.typo.hyphenate = (buf[5] & 2) != 0;
            self.typo.drop_cap = (buf[5] & 4) != 0;
        }
        if buf[6] >= 4 {
            self.full_scrub_interval = buf[6];
        }
        self.tilt_flick_enabled = buf[7] != 0;
        if matches!(buf[8], 5 | 10 | 15 | 30) {
            self.auto_sleep_mins = buf[8];
        }
        self.sleep_poster = SleepPosterKind::from_u8(buf[9]);
        self.active_book_idx = u32::from_le_bytes([buf[10], buf[11], buf[12], buf[13]]) as usize;
        if buf.get(39) == Some(&2) && buf.len() >= 40 {
            self.resume_screen_code = buf[14];
        }
        self.pip.progress = PipProgress::decode(&buf[16..32]);
    }

    pub fn flush_persistence<S: StorageBackend, P: PowerControl>(
        &mut self,
        storage: &mut S,
        power: &mut P,
    ) {
        power.set_sd_power_save(false);
        self.power_stats.sd_power_save = false;

        self.sync_progress_journal(storage);

        let payload = self.encode_journal_state();
        if AtomicJournal::commit(storage, JOURNAL_BASE, &payload).is_ok() {
            self.journal_seq = self.journal_seq.wrapping_add(1);
        }
        let games_blob = self.encode_games_bundle();
        let games_hash = Self::hash_bytes(&games_blob);
        if games_hash != self.last_games_hash
            && AtomicJournal::commit(storage, GAMES_JOURNAL_BASE, &games_blob).is_ok()
        {
            self.last_games_hash = games_hash;
        }
        self.pending_journal_turns = 0;
        self.power_stats.journal_flushes += 1;

        power.set_sd_power_save(true);
        self.power_stats.sd_power_save = true;
    }

    /// Recursively scan microSD folders (up to depth 5) for user books without loading payloads into RAM.
    pub fn scan_and_ingest_books<S: StorageBackend>(&mut self, storage: &mut S) -> usize {
        const MAX_DISCOVERED: usize = 512;
        const MAX_BOOK_BYTES: u64 = 512 * 1024 * 1024;

        let mut stack: Vec<(String, u8)> = vec![(String::from("/"), 0)];
        let mut found: Vec<(String, u64)> = Vec::new();

        while let Some((dir, depth)) = stack.pop() {
            let Ok(entries) = storage.list_dir(&dir) else {
                continue;
            };
            for de in entries {
                if de.is_dir {
                    if !is_valid_library_dir(&de.name) {
                        continue;
                    }
                    let full_path = if dir == "/" {
                        format!("/{}", de.name)
                    } else {
                        format!("{dir}/{}", de.name)
                    };
                    if depth < 5 {
                        stack.push((full_path, depth + 1));
                    }
                    continue;
                }
                if de.size == 0 || de.size > MAX_BOOK_BYTES {
                    continue;
                }
                let full_path = if dir == "/" {
                    format!("/{}", de.name)
                } else {
                    format!("{dir}/{}", de.name)
                };
                if !is_valid_library_book_path(&full_path) {
                    continue;
                }
                if found.len() < MAX_DISCOVERED {
                    found.push((full_path, de.size));
                }
            }
        }
        drop(stack);
        found.shrink_to_fit();
        found.sort_by(|a, b| {
            sibling_dedup_hash_and_prio(&a.0)
                .cmp(&sibling_dedup_hash_and_prio(&b.0))
                .then_with(|| a.0.cmp(&b.0))
        });

        let mut changed = false;
        let before_len = self.catalog.len();
        self.catalog.retain(|c| {
            if c.title.trim().is_empty() || c.title.contains('\u{FFFD}') {
                return false;
            }
            if c.source_path.is_empty() {
                storage.exists(&c.vvb_path())
            } else {
                is_valid_library_book_path(&c.source_path) && storage.exists(&c.source_path)
            }
        });
        if self.catalog.len() != before_len {
            changed = true;
        }

        // Pre-reserve catalog to exact expected capacity before allocating zip_scratch so we don't fragment SRAM mid-scan.
        let mut unique_new = 0usize;
        let mut last_h: Option<u64> = None;
        for (p, _) in &found {
            let (h, _) = sibling_dedup_hash_and_prio(p);
            if last_h != Some(h) {
                unique_new += 1;
                last_h = Some(h);
            }
        }
        let target_total = (self.catalog.len() + unique_new).min(MAX_DISCOVERED);
        if target_total > self.catalog.len() {
            self.catalog.reserve_exact(target_total - self.catalog.len());
        }

        let mut by_sibling: BTreeMap<u64, (u8, usize)> = BTreeMap::new();
        for (idx, c) in self.catalog.iter().enumerate() {
            if !c.source_path.is_empty() {
                let (h, p) = sibling_dedup_hash_and_prio(&c.source_path);
                by_sibling.entry(h).or_insert((p, idx));
            }
        }

        let needs_zip = found.iter().any(|(p, _)| {
            strip_suffix_ignore_ascii_case(p, ".epub").is_some()
                || strip_suffix_ignore_ascii_case(p, ".zip").is_some()
        });
        let mut zip_scratch = if needs_zip {
            Some(ZipInflateScratch::new())
        } else {
            None
        };

        let mut added = 0usize;
        for (mut full_path, size) in found {
            if self.catalog.len() >= MAX_DISCOVERED {
                break;
            }
            if self.catalog.iter().any(|c| c.source_path == full_path) {
                continue;
            }
            let (sib_key, prio) = sibling_dedup_hash_and_prio(&full_path);
            if let Some(&(existing_prio, existing_pos)) = by_sibling.get(&sib_key) {
                let existing = &self.catalog[existing_pos];
                let has_progress = existing.last_block > 0
                    || existing.last_word > 0
                    || existing.bookmarked_block.is_some();
                if existing_prio <= prio || has_progress {
                    continue;
                }
            }

            let Ok(probed) =
                probe_book_storage_with_scratch(storage, &full_path, size, zip_scratch.as_mut())
            else {
                continue;
            };
            if let Some(existing) = self.catalog.iter_mut().find(|c| c.id_hex == probed.id_hex) {
                if existing.source_path.is_empty() {
                    full_path.shrink_to_fit();
                    existing.source_path = full_path;
                    changed = true;
                }
                continue;
            }
            let mut id_hex = probed.id_hex;
            id_hex.shrink_to_fit();
            let mut title = probed.title;
            title.shrink_to_fit();
            let mut author = probed.author;
            author.shrink_to_fit();
            full_path.shrink_to_fit();
            let entry = CatalogEntry {
                id_hex,
                title,
                author,
                language: intern_language(&probed.language),
                source_format: intern_source_format(&probed.source_format),
                word_count: probed.est_words,
                block_count: probed.est_blocks,
                last_block: 0,
                last_word: 0,
                total_pages_cached: 0,
                current_page_cached: 0,
                bookmarked_block: None,
                source_path: full_path,
            };
            if let Some(&(_, existing_pos)) = by_sibling.get(&sib_key) {
                self.catalog[existing_pos] = entry;
                by_sibling.insert(sib_key, (prio, existing_pos));
            } else {
                let idx = self.catalog.len();
                self.catalog.push(entry);
                by_sibling.insert(sib_key, (prio, idx));
            }
            added += 1;
            changed = true;
        }
        drop(zip_scratch);
        drop(by_sibling);
        self.catalog.shrink_to_fit();

        if self.active_book_idx >= self.catalog.len() {
            self.active_book_idx = 0;
        }
        if changed {
            let _ = storage.write_atomic(CATALOG_PATH, &encode_catalog(&self.catalog));
            self.sync_progress_journal(storage);
        }
        added
    }

    pub fn release_open_book_ram(&mut self) {
        self.active_index = None;
        self.active_direct = None;
        self.direct_section_idx = 0;
        self.direct_section_blocks = Vec::new();
        self.direct_avg_pages_per_sec = 1;
        self.active_book_id = None;
        self.active_pages = Vec::new();
        self.pending_layout_anchor = None;
        self.prefetch_cache.clear();
        self.current_layout = None;
        self.home_cover_cache = None;
    }

    fn sanitize_drop_filename(name: &str) -> Option<String> {
        let base = name.rsplit(['/', '\\']).next().unwrap_or("").trim();
        if base.is_empty() {
            return None;
        }
        let mut out = String::new();
        for ch in base.chars() {
            if ch.is_alphanumeric() || matches!(ch, '_' | '-' | ' ' | '.') {
                out.push(ch);
            } else if !ch.is_control() && !out.ends_with('_') {
                out.push('_');
            }
            if out.len() >= 96 {
                break;
            }
        }
        let trimmed = out.trim_start_matches('.').trim();
        if trimmed.is_empty() {
            return None;
        }
        let clean = String::from(trimmed);
        InputFormat::from_filename(&clean)?;
        Some(clean)
    }

    #[allow(clippy::result_unit_err)]
    pub fn ingest_saved_file<S: StorageBackend>(
        &mut self,
        storage: &mut S,
        saved_path: &str,
    ) -> Result<String, ()> {
        let size = storage.file_len(saved_path).map_err(|_| ())?;
        let Ok(probed) = probe_book_storage(storage, saved_path, size) else {
            let _ = storage.remove(saved_path);
            return Err(());
        };
        let title = probed.title.clone();
        if let Some(pos) = self
            .catalog
            .iter()
            .position(|c| c.id_hex == probed.id_hex || c.source_path == saved_path)
        {
            let mut sp = saved_path.to_string();
            sp.shrink_to_fit();
            self.catalog[pos].source_path = sp;
            self.active_book_idx = pos;
        } else {
            let mut id_hex = probed.id_hex;
            id_hex.shrink_to_fit();
            let mut sp = saved_path.to_string();
            sp.shrink_to_fit();
            let mut author = probed.author;
            author.shrink_to_fit();
            let mut title_s = probed.title;
            title_s.shrink_to_fit();
            let entry = CatalogEntry {
                id_hex,
                title: title_s,
                author,
                language: intern_language(&probed.language),
                source_format: intern_source_format(&probed.source_format),
                word_count: probed.est_words,
                block_count: probed.est_blocks,
                last_block: 0,
                last_word: 0,
                total_pages_cached: 0,
                current_page_cached: 0,
                bookmarked_block: None,
                source_path: sp,
            };
            self.catalog.insert(0, entry);
            self.active_book_idx = 0;
        }
        let _ = storage.write_atomic(CATALOG_PATH, &encode_catalog(&self.catalog));
        self.sync_progress_journal(storage);
        self.refresh_home_cover(storage);
        Ok(title)
    }

    #[allow(clippy::result_unit_err)]
    pub fn ingest_raw_document<S: StorageBackend>(
        &mut self,
        storage: &mut S,
        filename: &str,
        raw: &[u8],
    ) -> Result<String, ()> {
        let safe = Self::sanitize_drop_filename(filename).ok_or(())?;
        let saved_path = format!("/books/{safe}");
        let _ = storage.mkdir_all("/books");
        storage.write_atomic(&saved_path, raw).map_err(|_| ())?;
        self.ingest_saved_file(storage, &saved_path)
    }

    fn resolved_vvb_path(entry: &CatalogEntry) -> String {
        if entry.source_path.to_ascii_lowercase().ends_with(".vvb") {
            entry.source_path.clone()
        } else {
            entry.vvb_path()
        }
    }

    pub fn refresh_home_cover<S: StorageBackend>(&mut self, storage: &mut S) {
        if let Some(entry) = self.catalog.get(self.active_book_idx) {
            if let Some((ref id, _)) = self.home_cover_cache
                && id == &entry.id_hex
            {
                return;
            }
            if self.active_book_id.as_deref() == Some(entry.id_hex.as_str()) {
                if let Some(ref idx) = self.active_index {
                    self.home_cover_cache = Some((entry.id_hex.clone(), idx.meta.cover_2bpp.clone()));
                    return;
                }
                if let Some(ref d) = self.active_direct {
                    if d.meta.cover_2bpp.is_empty() {
                        self.home_cover_cache = None;
                    } else {
                        self.home_cover_cache =
                            Some((entry.id_hex.clone(), d.meta.cover_2bpp.clone()));
                    }
                    return;
                }
            }
            let vvb_path = Self::resolved_vvb_path(entry);
            if storage.exists(&vvb_path)
                && let Ok(idx) = VvbIndex::open_from_storage(storage, &vvb_path)
            {
                self.home_cover_cache = Some((idx.meta.id_hex.clone(), idx.meta.cover_2bpp));
                return;
            }
        }
        self.home_cover_cache = None;
    }

    fn interactive_refresh_mode(&mut self) -> RefreshMode {
        self.fast_turns_since_scrub += 1;
        if self.fast_turns_since_scrub >= self.full_scrub_interval {
            self.fast_turns_since_scrub = 0;
            RefreshMode::HalfScrub
        } else {
            RefreshMode::FastDu
        }
    }

    fn direct_stream_source_len<S: StorageBackend>(storage: &S, entry: &CatalogEntry) -> Option<u64> {
        if entry.source_path.is_empty() {
            return None;
        }
        let lower = entry.source_path.to_ascii_lowercase();
        if lower.ends_with(".vvb") {
            return None;
        }
        storage.file_len(&entry.source_path).ok().filter(|&n| n > 0)
    }

    pub fn book_needs_indexing<S: StorageBackend>(&self, storage: &S, book_idx: usize) -> bool {
        let Some(entry) = self.catalog.get(book_idx) else {
            return false;
        };
        let typo_hash = self.typo.layout_hash();
        if self.active_book_id.as_deref() == Some(entry.id_hex.as_str())
            && !self.active_pages.is_empty()
        {
            if self.active_direct.is_some() {
                return false;
            }
            if self.active_index.is_some() && self.active_typo_hash == typo_hash {
                return false;
            }
        }
        if Self::direct_stream_source_len(storage, entry).is_some() {
            return true;
        }
        let vvb_path = Self::resolved_vvb_path(entry);
        let layout_path = format!("/.vvbook/layout/{}_{:08x}.vvl", entry.id_hex, typo_hash);
        !(storage.exists(&vvb_path) && storage.exists(&layout_path))
    }

    fn present_opening_placard_progress<D: DisplaySink, P: PowerControl>(
        &mut self,
        sink: &mut D,
        power: &mut P,
        book_idx: usize,
        done: usize,
        total: usize,
    ) {
        let Some(entry) = self.catalog.get(book_idx).cloned() else {
            return;
        };
        let opening_lbl = tr(self.lang).opening_book;
        draw_opening_placard_to_sink(
            sink,
            power,
            &mut self.power_stats.panel_hv_standby,
            &entry.title,
            opening_lbl,
            done,
            total,
        );
    }

    fn present_opening_placard<D: DisplaySink, P: PowerControl>(
        &mut self,
        sink: &mut D,
        power: &mut P,
        book_idx: usize,
    ) {
        self.present_opening_placard_progress(sink, power, book_idx, 0, 0);
    }

    pub fn display_page_num(&self) -> usize {
        if self.active_direct.is_some() {
            self.direct_section_idx * self.direct_avg_pages_per_sec.max(1) + self.current_page_num
        } else {
            self.current_page_num
        }
    }

    fn estimated_total_pages(&self) -> usize {
        if let Some(ref d) = self.active_direct {
            let avg = self.direct_avg_pages_per_sec.max(1);
            return (d.section_count() * avg)
                .max(self.direct_section_idx * avg + self.active_pages.len())
                .max(1);
        }
        let known = self.active_pages.len().max(1);
        let Some(pending) = self.pending_layout_anchor else {
            return self.active_pages.len();
        };
        let total_blocks = self
            .active_index
            .as_ref()
            .map(|i| i.block_count())
            .unwrap_or(0);
        let done_blocks = (pending.block_idx as usize).max(1);
        if total_blocks <= done_blocks {
            return known;
        }
        ((known as u64 * total_blocks as u64) / done_blocks as u64)
            .max(known as u64) as usize
    }

    fn load_direct_section_into_ram<S: StorageBackend, P: PowerControl>(
        &mut self,
        storage: &mut S,
        power: &mut P,
        target_sec: usize,
        forward: bool,
    ) -> bool {
        self.load_direct_section_into_ram_with_scratch(storage, power, target_sec, forward, None)
    }

    fn load_direct_section_into_ram_with_scratch<S: StorageBackend, P: PowerControl>(
        &mut self,
        storage: &mut S,
        power: &mut P,
        target_sec: usize,
        forward: bool,
        mut scratch: Option<ZipInflateScratch>,
    ) -> bool {
        // Release the previous section and typeset buffers before inflating the next section
        // so the 320 KB heap never holds two sections alongside a 43 KB InflateState.
        self.direct_section_blocks = Vec::new();
        self.active_pages = Vec::new();
        self.current_layout = None;

        let Some(ref direct) = self.active_direct else {
            return false;
        };
        let sec_count = direct.section_count().max(1);
        let start = target_sec.min(sec_count - 1);
        let mut chosen = start;
        power.set_sd_power_save(false);
        self.power_stats.sd_power_save = false;

        let mut loaded = Vec::new();
        for _ in 0..16 {
            let res = match scratch.as_mut() {
                Some(sc) => direct.load_section_with_scratch(storage, chosen, sc),
                None => direct.load_section(storage, chosen),
            };
            match res {
                Ok(blks) if !blks.is_empty() => {
                    loaded = blks;
                    break;
                }
                Ok(_) => {
                    if forward {
                        if chosen + 1 < sec_count {
                            chosen += 1;
                            continue;
                        }
                    } else if chosen > 0 {
                        chosen -= 1;
                        continue;
                    }
                    break;
                }
                Err(_) => break,
            }
        }

        if loaded.is_empty() && forward {
            for step in 1..=16usize {
                let Some(cand) = start.checked_sub(step) else {
                    break;
                };
                let res = match scratch.as_mut() {
                    Some(sc) => direct.load_section_with_scratch(storage, cand, sc),
                    None => direct.load_section(storage, cand),
                };
                match res {
                    Ok(blks) if !blks.is_empty() => {
                        chosen = cand;
                        loaded = blks;
                        break;
                    }
                    Ok(_) => continue,
                    Err(_) => break,
                }
            }
        }

        // Release the ~43.5 KB ZIP inflate state before allocating Knuth-Plass pagination buffers.
        drop(scratch);

        power.set_sd_power_save(true);
        self.power_stats.sd_power_save = true;

        if loaded.is_empty() {
            loaded.push(Block::SceneBreak);
        }
        self.direct_section_idx = chosen;
        self.direct_section_blocks = loaded;
        self.active_pages = paginate_blocks_slice(&self.direct_section_blocks, &self.typo, 1024);
        if self.active_pages.is_empty() {
            self.active_pages.push(PageAnchor {
                block_idx: 0,
                word_offset: 0,
            });
        }
        self.direct_avg_pages_per_sec = self.active_pages.len().max(1);
        true
    }

    pub fn open_active_book<S: StorageBackend, P: PowerControl>(
        &mut self,
        storage: &mut S,
        power: &mut P,
    ) -> bool {
        self.open_active_book_interactive::<NullSink, S, P>(None, storage, power)
    }

    pub fn open_active_book_interactive<D: DisplaySink, S: StorageBackend, P: PowerControl>(
        &mut self,
        mut sink: Option<&mut D>,
        storage: &mut S,
        power: &mut P,
    ) -> bool {
        let Some(entry) = self.catalog.get(self.active_book_idx).cloned() else {
            self.last_open_diag = Some(String::from("FAIL stage=no_catalog_entry"));
            self.library_notice = Some(String::from(tr(self.lang).open_failed_notice));
            return false;
        };
        let typo_hash = self.typo.layout_hash();
        let same_book = self.active_book_id.as_deref() == Some(entry.id_hex.as_str());
        if same_book
            && self.active_typo_hash == typo_hash
            && !self.active_pages.is_empty()
            && (self.active_index.is_some() || self.active_direct.is_some())
        {
            self.current_page_num = self.current_page_num.min(self.active_pages.len().saturating_sub(1));
            self.load_current_page_with_prefetch(storage, power);
            self.library_notice = None;
            self.last_open_diag = Some(format!(
                "OK path={} mode=cached pages={}",
                entry.source_path,
                self.active_pages.len()
            ));
            return true;
        }

        // Zero-SD `Aa` repagination when an active DirectBook section is already in RAM.
        if same_book && self.active_direct.is_some() && !self.direct_section_blocks.is_empty() {
            let cur_blk = self
                .active_pages
                .get(self.current_page_num)
                .map(|a| a.block_idx)
                .unwrap_or(0);
            self.active_pages = paginate_blocks_slice(&self.direct_section_blocks, &self.typo, 1024);
            if self.active_pages.is_empty() {
                self.active_pages.push(PageAnchor {
                    block_idx: 0,
                    word_offset: 0,
                });
            }
            self.direct_avg_pages_per_sec = self.active_pages.len().max(1);
            self.current_page_num = self
                .active_pages
                .iter()
                .position(|a| a.block_idx >= cur_blk)
                .unwrap_or_else(|| self.active_pages.len().saturating_sub(1));
            self.active_typo_hash = typo_hash;
            let est_total = self.estimated_total_pages() as u32;
            let disp_page = self.display_page_num() as u32;
            if let Some(c) = self.catalog.get_mut(self.active_book_idx) {
                c.total_pages_cached = est_total;
                c.current_page_cached = disp_page;
            }
            self.load_current_page_with_prefetch(storage, power);
            self.library_notice = None;
            self.last_open_diag = Some(format!(
                "OK path={} mode=direct_repaginate pages={}",
                entry.source_path,
                self.active_pages.len()
            ));
            return true;
        }

        // Release stale book allocations before ingesting or repaginating so two books never share SRAM.
        let reused_index = if same_book {
            self.active_index.take()
        } else {
            self.active_index = None;
            None
        };
        self.active_direct = None;
        self.direct_section_idx = 0;
        self.direct_section_blocks = Vec::new();
        self.direct_avg_pages_per_sec = 1;
        self.active_pages = Vec::new();
        self.pending_layout_anchor = None;
        self.prefetch_cache.clear();
        self.current_layout = None;
        self.home_cover_cache = None;

        power.set_sd_power_save(false);
        self.power_stats.sd_power_save = false;

        let vvb_path = Self::resolved_vvb_path(&entry);
        let lower_src = entry.source_path.to_ascii_lowercase();
        if reused_index.is_none()
            && let Some(src_len) = Self::direct_stream_source_len(storage, &entry)
        {
            let is_zip = lower_src.ends_with(".epub") || lower_src.ends_with(".zip");
            let mut scratch = if is_zip {
                Some(ZipInflateScratch::new())
            } else {
                None
            };
            let direct_res = match scratch.as_mut() {
                Some(sc) => DirectBook::open_with_scratch(
                    storage,
                    &entry.source_path,
                    src_len,
                    Some(&entry.id_hex),
                    sc,
                ),
                None => {
                    DirectBook::open(storage, &entry.source_path, src_len, Some(&entry.id_hex))
                }
            };
            let direct = match direct_res {
                Ok(d) => d,
                Err(e) => {
                    power.set_sd_power_save(true);
                    self.power_stats.sd_power_save = true;
                    self.last_open_diag = Some(format!(
                        "FAIL path={} stage=direct_open err={e:?}",
                        entry.source_path
                    ));
                    self.library_notice = Some(String::from(tr(self.lang).open_failed_notice));
                    return false;
                }
            };
            let sec_count = direct.section_count().max(1);
            let saved_sec = ((entry.last_block >> DIRECT_SEC_SHIFT) as usize).min(sec_count - 1);
            let saved_blk = entry.last_block & 0xFF;
            let est_words = direct.estimated_word_count();
            self.active_direct = Some(direct);
            self.active_book_id = Some(entry.id_hex);
            self.active_typo_hash = typo_hash;
            if !self.load_direct_section_into_ram_with_scratch(
                storage,
                power,
                saved_sec,
                true,
                scratch,
            ) {
                self.release_open_book_ram();
                power.set_sd_power_save(true);
                self.power_stats.sd_power_save = true;
                self.last_open_diag = Some(format!(
                    "FAIL path={} stage=direct_section sec={saved_sec}",
                    entry.source_path
                ));
                self.library_notice = Some(String::from(tr(self.lang).open_failed_notice));
                return false;
            }
            self.current_page_num = self
                .active_pages
                .iter()
                .position(|a| {
                    a.block_idx > saved_blk
                        || (a.block_idx == saved_blk && a.word_offset >= entry.last_word)
                })
                .unwrap_or(0)
                .min(self.active_pages.len().saturating_sub(1));
            let est_total = self.estimated_total_pages() as u32;
            let disp_page = self.display_page_num() as u32;
            if let Some(c) = self.catalog.get_mut(self.active_book_idx) {
                c.word_count = est_words;
                c.total_pages_cached = est_total;
                c.current_page_cached = disp_page;
            }
            self.load_current_page_with_prefetch(storage, power);
            self.library_notice = None;
            self.last_open_diag = Some(format!(
                "OK path={} mode=direct secs={sec_count} pages={}",
                entry.source_path,
                self.active_pages.len()
            ));
            return true;
        }

        let index = match reused_index {
            Some(idx) => idx,
            None => match VvbIndex::open_from_storage(storage, &vvb_path) {
                Ok(idx) => idx,
                Err(e)
                    if !entry.source_path.is_empty()
                        && !entry.source_path.to_ascii_lowercase().ends_with(".vvb") =>
                {
                    let _ = e;
                    let mut last_bucket = 0usize;
                    let title_owned = entry.title.clone();
                    let opening_lbl = tr(self.lang).opening_book;
                    let panel_standby = &mut self.power_stats.panel_hv_standby;
                    let idx = match ingest_storage_to_vvb_with_progress(
                        storage,
                        &entry.source_path,
                        &vvb_path,
                        Some(&entry.id_hex),
                        |done, total| {
                            if total >= 8 {
                                let bucket = (done.min(total) * 8) / total;
                                if bucket > last_bucket {
                                    last_bucket = bucket;
                                    if let Some(sk) = sink.as_deref_mut() {
                                        draw_opening_placard_to_sink(
                                            sk,
                                            power,
                                            panel_standby,
                                            &title_owned,
                                            opening_lbl,
                                            done,
                                            total,
                                        );
                                    }
                                }
                            }
                        },
                    ) {
                        Ok(idx) => idx,
                        Err(e) => {
                            power.set_sd_power_save(true);
                            self.power_stats.sd_power_save = true;
                            self.last_open_diag = Some(format!(
                                "FAIL path={} stage=vvb_ingest err={e:?}",
                                entry.source_path
                            ));
                            self.library_notice =
                                Some(String::from(tr(self.lang).open_failed_notice));
                            return false;
                        }
                    };
                    if let Some(c) = self.catalog.get_mut(self.active_book_idx) {
                        c.word_count = idx.meta.word_count;
                        c.block_count = idx.block_count() as u32;
                    }
                    let _ = storage.write_atomic(CATALOG_PATH, &encode_catalog(&self.catalog));
                    idx
                }
                Err(_) => {
                    power.set_sd_power_save(true);
                    self.power_stats.sd_power_save = true;
                    self.last_open_diag = Some(format!(
                        "FAIL path={} stage=vvb_open",
                        entry.source_path
                    ));
                    self.library_notice = Some(String::from(tr(self.lang).open_failed_notice));
                    return false;
                }
            },
        };

        // Load `.vvl` if compact, otherwise progressively paginate first 64 pages (or up to resume bookmark)
        let layout_path = format!("/.vvbook/layout/{}_{:08x}.vvl", entry.id_hex, typo_hash);
        let (pages, pending) = if storage
            .file_len(&layout_path)
            .ok()
            .is_some_and(|len| len <= 128 * 1024)
            && let Ok(raw_vvl) = storage.read_all(&layout_path)
            && let Ok(cached_pages) = decode_layout_cache(&raw_vvl)
        {
            (cached_pages, None)
        } else {
            let (mut p, mut next) = paginate_from_anchor(
                &index,
                storage,
                &vvb_path,
                PageAnchor {
                    block_idx: 0,
                    word_offset: 0,
                },
                64,
                &self.typo,
            );
            if entry.last_block > 0 {
                while let Some(na) = next {
                    if p.last().is_some_and(|a| a.block_idx >= entry.last_block)
                        || p.len() >= 16_384
                    {
                        break;
                    }
                    let (more, next2) = paginate_from_anchor(
                        &index,
                        storage,
                        &vvb_path,
                        na,
                        64.min(16_384 - p.len()),
                        &self.typo,
                    );
                    p.extend(more);
                    next = next2;
                }
                if let Some(na) = next
                    && p.len() < 16_384
                {
                    let (more, next2) = paginate_from_anchor(
                        &index,
                        storage,
                        &vvb_path,
                        na,
                        48.min(16_384 - p.len()),
                        &self.typo,
                    );
                    p.extend(more);
                    next = next2;
                }
            }
            let final_next = if p.len() >= 16_384 { None } else { next };
            if final_next.is_none() {
                let encoded = encode_layout_cache(&p);
                if encoded.len() <= 128 * 1024 {
                    let _ = storage.write_atomic(&layout_path, &encoded);
                }
            }
            (p, final_next)
        };

        let page_num = pages
            .iter()
            .position(|a| a.block_idx >= entry.last_block)
            .unwrap_or(0)
            .min(pages.len().saturating_sub(1));

        self.active_index = Some(index);
        self.active_book_id = Some(entry.id_hex);
        self.active_typo_hash = typo_hash;
        self.active_pages = pages;
        self.pending_layout_anchor = pending;
        self.current_page_num = page_num;
        self.prefetch_cache.clear();

        let est_total = self.estimated_total_pages() as u32;
        if let Some(c) = self.catalog.get_mut(self.active_book_idx) {
            c.total_pages_cached = est_total;
            c.current_page_cached = page_num as u32;
        }

        self.load_current_page_with_prefetch(storage, power);
        self.library_notice = None;
        self.last_open_diag = Some(format!(
            "OK path={} mode=vvb pages={}",
            entry.source_path,
            self.active_pages.len()
        ));
        true
    }

    fn extend_pending_pagination<S: StorageBackend, P: PowerControl>(
        &mut self,
        storage: &mut S,
        power: &mut P,
        budget_pages: usize,
    ) {
        let Some(start_anchor) = self.pending_layout_anchor else {
            return;
        };
        let (Some(index), Some(entry)) = (
            self.active_index.as_ref(),
            self.catalog.get(self.active_book_idx),
        ) else {
            self.pending_layout_anchor = None;
            return;
        };
        if self.active_pages.len() >= 16_384 {
            self.pending_layout_anchor = None;
            return;
        }
        let vvb_path = Self::resolved_vvb_path(entry);
        let was_save = self.power_stats.sd_power_save;
        if was_save {
            power.set_sd_power_save(false);
            self.power_stats.sd_power_save = false;
        }
        let (more, next) = paginate_from_anchor(
            index,
            storage,
            &vvb_path,
            start_anchor,
            budget_pages.min(16_384 - self.active_pages.len()),
            &self.typo,
        );
        self.active_pages.extend(more);
        self.pending_layout_anchor = if self.active_pages.len() >= 16_384 {
            None
        } else {
            next
        };
        if self.pending_layout_anchor.is_none() {
            if let Some(c) = self.catalog.get_mut(self.active_book_idx) {
                c.total_pages_cached = self.active_pages.len() as u32;
            }
            if let Some(ref id_hex) = self.active_book_id {
                let layout_path =
                    format!("/.vvbook/layout/{}_{:08x}.vvl", id_hex, self.active_typo_hash);
                let encoded = encode_layout_cache(&self.active_pages);
                if encoded.len() <= 128 * 1024 {
                    let _ = storage.write_atomic(&layout_path, &encoded);
                }
            }
        }
        if was_save {
            power.set_sd_power_save(true);
            self.power_stats.sd_power_save = true;
        }
    }

    /// Reads ONLY missing blocks in `start_block..start_block+16` into `prefetch_cache`,
    /// then IMMEDIATELY gates microSD power (`set_sd_power_save(true)`).
    /// Consecutive page turns within the prefetch window cost ZERO SD card wakeups!
    fn load_current_page_with_prefetch<S: StorageBackend, P: PowerControl>(
        &mut self,
        storage: &mut S,
        power: &mut P,
    ) {
        let anchor = self
            .active_pages
            .get(self.current_page_num)
            .copied()
            .unwrap_or(PageAnchor {
                block_idx: 0,
                word_offset: 0,
            });
        if self.active_direct.is_some() {
            self.power_stats.pages_from_prefetch += 1;
            let layout = layout_page_slice(&self.direct_section_blocks, anchor, &self.typo);
            self.current_layout = Some(layout);
            let disp_page = self.display_page_num() as u32;
            let sec_packed = ((self.direct_section_idx as u32) << DIRECT_SEC_SHIFT)
                | anchor.block_idx.min(255);
            if let Some(c) = self.catalog.get_mut(self.active_book_idx) {
                c.last_block = sec_packed;
                c.last_word = anchor.word_offset;
                c.current_page_cached = disp_page;
            }
            return;
        }
        let Some(ref index) = self.active_index else {
            return;
        };
        let Some(entry) = self.catalog.get(self.active_book_idx) else {
            return;
        };
        let vvb_path = Self::resolved_vvb_path(entry);
        let total_blocks = index.block_count();
        let start_b = anchor.block_idx as usize;

        // Ensure EVERY block required for the current page (up to next page's anchor) is cached in SRAM
        let needed_end = self
            .active_pages
            .get(self.current_page_num + 1)
            .map(|a| (a.block_idx as usize) + 1)
            .unwrap_or(total_blocks)
            .min(total_blocks);
        let all_hot = (start_b..needed_end).all(|bi| self.prefetch_cache.contains_key(&bi));

        if all_hot {
            self.power_stats.pages_from_prefetch += 1;
        } else {
            power.set_sd_power_save(false);
            self.power_stats.sd_power_save = false;
            // Prune stale blocks far behind current anchor
            self.prefetch_cache.retain(|&k, _| k >= start_b.saturating_sub(2));
            let window_end = needed_end.max(start_b + PREFETCH_WINDOW_BLOCKS).min(total_blocks);
            if let Some(first_miss) =
                (start_b..window_end).find(|bi| !self.prefetch_cache.contains_key(bi))
                && let Ok(blocks) =
                    index.read_blocks_range(storage, &vvb_path, first_miss, window_end - first_miss)
            {
                for (offset, blk) in blocks.into_iter().enumerate() {
                    self.prefetch_cache.insert(first_miss + offset, blk);
                }
            }
            power.set_sd_power_save(true);
            self.power_stats.sd_power_save = true;
        }

        let cache = &self.prefetch_cache;
        let layout = layout_page_with(total_blocks, anchor, &self.typo, |bi| cache.get(&bi).cloned());
        self.current_layout = Some(layout);

        if let Some(c) = self.catalog.get_mut(self.active_book_idx) {
            c.last_block = anchor.block_idx;
            c.last_word = anchor.word_offset;
            c.current_page_cached = self.current_page_num as u32;
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn handle_event<D, S, E, P, W, R>(
        &mut self,
        event: InputEvent,
        sink: &mut D,
        storage: &mut S,
        sensors: &mut E,
        power: &mut P,
        portal: &mut W,
        entropy: &mut R,
    ) where
        D: DisplaySink,
        S: StorageBackend,
        E: DeviceSensors,
        P: PowerControl,
        W: NetworkPortal,
        R: EntropySource,
    {
        // Ignore button releases and any non-Press event on SleepPoster BEFORE waking
        // EPD HV rails or polling I2C sensors (prevents Release(Power) after sleep entry
        // from transmitting PON 0x04 over a powered-down shared SPI2 rail).
        if matches!(event, InputEvent::Release(_))
            || (self.screen == Screen::SleepPoster && !matches!(event, InputEvent::Press(_)))
        {
            return;
        }

        let now_ms = sensors.uptime_ms();
        self.last_activity_ms = now_ms;

        // Wake panel HV rails & refresh battery telemetry when throttled (>= 60s)
        if self.power_stats.panel_hv_standby {
            power.set_panel_hv_standby(false);
            self.power_stats.panel_hv_standby = false;
        }
        if now_ms.saturating_sub(self.last_battery_poll_ms) >= 60_000 {
            self.battery = sensors.battery();
            self.clock = sensors.now();
            self.last_battery_poll_ms = now_ms;
        }

        // Any button press wakes from SleepPoster smoothly (ignore Release so releasing
        // Power immediately after entering sleep never wakes the device right back up).
        if self.screen == Screen::SleepPoster {
            if matches!(event, InputEvent::Press(_)) {
                self.screen = self.prev_screen_before_sleep;
                self.fast_turns_since_scrub = 0;
                self.apply_screen_power_gates(power, portal, entropy, now_ms);
                self.redraw(sink, RefreshMode::HalfScrub, power);
            }
            return;
        }

        // Map tilt events to reader page turns only when IMU flick is enabled on Reader
        let btn = match event {
            InputEvent::Press(b) => b,
            InputEvent::LongPress(Button::Confirm) => {
                if self.screen == Screen::Chess {
                    if let GameAction::Redraw(mode) = self.chess.handle_long_confirm() {
                        self.persistence_dirty = true;
                        self.redraw(sink, mode, power);
                    }
                    return;
                } else if self.screen == Screen::Library {
                    self.library_notice = None;
                    power.set_sd_power_save(false);
                    self.scan_and_ingest_books(storage);
                    power.set_sd_power_save(true);
                    self.redraw(sink, RefreshMode::HalfScrub, power);
                    return;
                } else {
                    Button::Confirm
                }
            }
            InputEvent::LongPress(Button::Left | Button::Up)
                if self.screen == Screen::Library && !self.catalog.is_empty() =>
            {
                self.library_notice = None;
                let count = self.catalog.len();
                let step = LIBRARY_PER_PAGE.min(count);
                self.lib_cursor = (self.lib_cursor + count - step) % count;
                self.redraw(sink, RefreshMode::FastDu, power);
                return;
            }
            InputEvent::LongPress(Button::Right | Button::Down)
                if self.screen == Screen::Library && !self.catalog.is_empty() =>
            {
                self.library_notice = None;
                let count = self.catalog.len();
                let step = LIBRARY_PER_PAGE.min(count);
                self.lib_cursor = (self.lib_cursor + step) % count;
                self.redraw(sink, RefreshMode::FastDu, power);
                return;
            }
            InputEvent::TiltFlickNext if self.screen == Screen::Reader && self.tilt_flick_enabled => {
                Button::Right
            }
            InputEvent::TiltFlickPrev if self.screen == Screen::Reader && self.tilt_flick_enabled => {
                Button::Left
            }
            InputEvent::OrientationFlip(rot) => {
                power.set_rotated_180(rot);
                self.redraw(sink, RefreshMode::HalfScrub, power);
                return;
            }
            _ => return,
        };

        // Dedicated Power button sleeps cleanly from any screen (except when Chess coach hint handles it? Let's make Power ALWAYS sleep on all screens — Chess coach hint already triggers via Confirm on empty square or LongPress Confirm!)
        if btn == Button::Power {
            self.enter_sleep(sink, storage, power, portal);
            return;
        }

        match self.screen {
            Screen::Home => {
                match btn {
                    Button::Up | Button::Left => {
                        self.home_cursor = (self.home_cursor + 5) % 6;
                        self.redraw(sink, RefreshMode::FastDu, power);
                    }
                    Button::Down | Button::Right => {
                        self.home_cursor = (self.home_cursor + 1) % 6;
                        self.redraw(sink, RefreshMode::FastDu, power);
                    }
                    Button::Back => {
                        if self.home_cursor != 0 {
                            self.home_cursor = 0;
                            self.redraw(sink, RefreshMode::FastDu, power);
                        }
                    }
                    Button::Confirm => {
                        if self.home_cursor == 0 {
                            if self.book_needs_indexing(storage, self.active_book_idx) {
                                self.present_opening_placard(sink, power, self.active_book_idx);
                            }
                            if self.open_active_book_interactive(Some(sink), storage, power) {
                                self.screen = Screen::Reader;
                                self.persistence_dirty = true;
                                self.reader_overlay = ReaderOverlay::None;
                                self.apply_screen_power_gates(power, portal, entropy, now_ms);
                                let mode = self.interactive_refresh_mode();
                                self.redraw(sink, mode, power);
                            } else {
                                self.screen = Screen::Library;
                                let mode = self.interactive_refresh_mode();
                                self.redraw(sink, mode, power);
                            }
                        } else {
                            self.screen = match self.home_cursor {
                                1 => Screen::Library,
                                2 => {
                                    self.persistence_dirty = true;
                                    Screen::Chess
                                }
                                3 => {
                                    self.persistence_dirty = true;
                                    Screen::PocketPip
                                }
                                4 => Screen::VvDrop,
                                _ => Screen::Settings,
                            };
                            self.apply_screen_power_gates(power, portal, entropy, now_ms);
                            let mode = self.interactive_refresh_mode();
                            self.redraw(sink, mode, power);
                        }
                    }
                    _ => {}
                }
            }
            Screen::Library => {
                let count = self.catalog.len();
                match btn {
                    Button::Back => {
                        self.library_notice = None;
                        self.screen = Screen::Home;
                        let mode = self.interactive_refresh_mode();
                        self.redraw(sink, mode, power);
                    }
                    Button::Up | Button::Left if count > 0 => {
                        self.library_notice = None;
                        self.lib_cursor = (self.lib_cursor + count - 1) % count;
                        self.redraw(sink, RefreshMode::FastDu, power);
                    }
                    Button::Down | Button::Right if count > 0 => {
                        self.library_notice = None;
                        self.lib_cursor = (self.lib_cursor + 1) % count;
                        self.redraw(sink, RefreshMode::FastDu, power);
                    }
                    Button::Confirm => {
                        if count == 0 {
                            power.set_sd_power_save(false);
                            self.scan_and_ingest_books(storage);
                            power.set_sd_power_save(true);
                            let mode = self.interactive_refresh_mode();
                            self.redraw(sink, mode, power);
                        } else {
                            self.active_book_idx = self.lib_cursor;
                            self.refresh_home_cover(storage);
                            if self.book_needs_indexing(storage, self.active_book_idx) {
                                self.present_opening_placard(sink, power, self.active_book_idx);
                            }
                            if self.open_active_book_interactive(Some(sink), storage, power) {
                                self.screen = Screen::Reader;
                                self.persistence_dirty = true;
                                self.reader_overlay = ReaderOverlay::None;
                                self.apply_screen_power_gates(power, portal, entropy, now_ms);
                                let mode = self.interactive_refresh_mode();
                                self.redraw(sink, mode, power);
                            } else {
                                let mode = self.interactive_refresh_mode();
                                self.redraw(sink, mode, power);
                            }
                        }
                    }
                    _ => {}
                }
            }
            Screen::Reader => match self.reader_overlay {
                ReaderOverlay::None => match btn {
                    Button::Right | Button::Down => {
                        let turned = if self.active_direct.is_some() {
                            if self.current_page_num + 1 < self.active_pages.len() {
                                self.current_page_num += 1;
                                true
                            } else {
                                let prev_sec = self.direct_section_idx;
                                let next_sec = prev_sec + 1;
                                let sec_cnt = self
                                    .active_direct
                                    .as_ref()
                                    .map(|d| d.section_count())
                                    .unwrap_or(0);
                                if next_sec < sec_cnt
                                    && self.load_direct_section_into_ram(storage, power, next_sec, true)
                                {
                                    if self.direct_section_idx > prev_sec {
                                        self.current_page_num = 0;
                                        true
                                    } else {
                                        self.current_page_num =
                                            self.active_pages.len().saturating_sub(1);
                                        false
                                    }
                                } else {
                                    false
                                }
                            }
                        } else {
                            if self.pending_layout_anchor.is_some()
                                && self.current_page_num + 12 >= self.active_pages.len()
                            {
                                self.extend_pending_pagination(storage, power, 32);
                            }
                            if self.current_page_num + 1 < self.active_pages.len() {
                                self.current_page_num += 1;
                                true
                            } else {
                                false
                            }
                        };
                        if turned {
                            self.unique_pages_read_in_session += 1;
                            if self.unique_pages_read_in_session.is_multiple_of(5) {
                                self.pip.award_reading_starberry();
                            }
                            self.load_current_page_with_prefetch(storage, power);
                            self.persistence_dirty = true;
                            self.pending_journal_turns += 1;
                            if self.pending_journal_turns >= 10 {
                                self.persistence_dirty = false;
                                self.flush_persistence(storage, power);
                            }
                            let mode = self.interactive_refresh_mode();
                            self.redraw(sink, mode, power);
                        }
                    }
                    Button::Left | Button::Up => {
                        let turned = if self.current_page_num > 0 {
                            self.current_page_num -= 1;
                            true
                        } else if self.active_direct.is_some() && self.direct_section_idx > 0 {
                            let prev_sec = self.direct_section_idx - 1;
                            if self.load_direct_section_into_ram(storage, power, prev_sec, false) {
                                self.current_page_num = self.active_pages.len().saturating_sub(1);
                                true
                            } else {
                                false
                            }
                        } else {
                            false
                        };
                        if turned {
                            self.load_current_page_with_prefetch(storage, power);
                            self.persistence_dirty = true;
                            let mode = self.interactive_refresh_mode();
                            self.redraw(sink, mode, power);
                        }
                    }
                    Button::Confirm => {
                        self.reader_overlay = ReaderOverlay::AaHud;
                        self.hud_cursor = 0;
                        self.redraw(sink, RefreshMode::FastDu, power);
                    }
                    Button::Back => {
                        self.persistence_dirty = true;
                        self.screen = Screen::Home;
                        self.refresh_home_cover(storage);
                        self.apply_screen_power_gates(power, portal, entropy, now_ms);
                        let mode = self.interactive_refresh_mode();
                        self.redraw(sink, mode, power);
                    }
                    _ => {}
                },
                ReaderOverlay::AaHud => match btn {
                    Button::Back => {
                        self.reader_overlay = ReaderOverlay::None;
                        let mode = self.interactive_refresh_mode();
                        self.redraw(sink, mode, power);
                    }
                    Button::Up | Button::Left => {
                        self.hud_cursor = (self.hud_cursor + 5) % 6;
                        self.redraw(sink, RefreshMode::FastDu, power);
                    }
                    Button::Down | Button::Right => {
                        self.hud_cursor = (self.hud_cursor + 1) % 6;
                        self.redraw(sink, RefreshMode::FastDu, power);
                    }
                    Button::Confirm => {
                        match self.hud_cursor {
                            0 => {
                                self.typo.font_family = match self.typo.font_family {
                                    FontFamily::School => FontFamily::Serif,
                                    FontFamily::Serif => FontFamily::Sans,
                                    FontFamily::Sans => FontFamily::School,
                                };
                                self.persistence_dirty = true;
                                if self.book_needs_indexing(storage, self.active_book_idx) {
                                    self.present_opening_placard(sink, power, self.active_book_idx);
                                }
                                self.open_active_book_interactive(Some(sink), storage, power);
                            }
                            1 => {
                                self.typo.font_size = match self.typo.font_size {
                                    FontSizePreset::Compact => FontSizePreset::Classic,
                                    FontSizePreset::Classic => FontSizePreset::Large,
                                    FontSizePreset::Large => FontSizePreset::Extra,
                                    FontSizePreset::Extra => FontSizePreset::Compact,
                                };
                                self.persistence_dirty = true;
                                if self.book_needs_indexing(storage, self.active_book_idx) {
                                    self.present_opening_placard(sink, power, self.active_book_idx);
                                }
                                self.open_active_book_interactive(Some(sink), storage, power);
                            }
                            2 => {
                                self.typo.margin_preset = match self.typo.margin_preset {
                                    MarginPreset::Narrow => MarginPreset::Editorial,
                                    MarginPreset::Editorial => MarginPreset::Wide,
                                    MarginPreset::Wide => MarginPreset::Narrow,
                                };
                                self.persistence_dirty = true;
                                if self.book_needs_indexing(storage, self.active_book_idx) {
                                    self.present_opening_placard(sink, power, self.active_book_idx);
                                }
                                self.open_active_book_interactive(Some(sink), storage, power);
                            }
                            3 => {
                                let raw_blk = self
                                    .active_pages
                                    .get(self.current_page_num)
                                    .map(|a| a.block_idx)
                                    .unwrap_or(0);
                                let blk = if self.active_direct.is_some() {
                                    ((self.direct_section_idx as u32) << DIRECT_SEC_SHIFT)
                                        | raw_blk.min(255)
                                } else {
                                    raw_blk
                                };
                                if let Some(c) = self.catalog.get_mut(self.active_book_idx) {
                                    c.bookmarked_block = if c.bookmarked_block == Some(blk) {
                                        None
                                    } else {
                                        Some(blk)
                                    };
                                }
                                self.persistence_dirty = true;
                            }
                            4 => {
                                self.reader_overlay = ReaderOverlay::TocDrawer;
                                self.toc_cursor = 0;
                            }
                            _ => {
                                self.reader_overlay = ReaderOverlay::None;
                                self.persistence_dirty = true;
                                self.screen = Screen::Home;
                                self.apply_screen_power_gates(power, portal, entropy, now_ms);
                                let mode = self.interactive_refresh_mode();
                                self.redraw(sink, mode, power);
                                return;
                            }
                        }
                        self.redraw(sink, RefreshMode::FastDu, power);
                    }
                    _ => {}
                },
                ReaderOverlay::TocDrawer => {
                    let toc_len = self
                        .active_direct
                        .as_ref()
                        .map(|d| d.toc.len())
                        .or_else(|| self.active_index.as_ref().map(|i| i.toc.len()))
                        .unwrap_or(0);
                    match btn {
                        Button::Back => {
                            self.reader_overlay = ReaderOverlay::AaHud;
                            self.redraw(sink, RefreshMode::FastDu, power);
                        }
                        Button::Up | Button::Left if toc_len > 0 => {
                            self.toc_cursor = (self.toc_cursor + toc_len - 1) % toc_len;
                            self.redraw(sink, RefreshMode::FastDu, power);
                        }
                        Button::Down | Button::Right if toc_len > 0 => {
                            self.toc_cursor = (self.toc_cursor + 1) % toc_len;
                            self.redraw(sink, RefreshMode::FastDu, power);
                        }
                        Button::Confirm if toc_len > 0 => {
                            if let Some(target) = self
                                .active_direct
                                .as_ref()
                                .and_then(|d| d.toc.get(self.toc_cursor).cloned())
                            {
                                self.load_direct_section_into_ram(
                                    storage,
                                    power,
                                    target.section_idx,
                                    true,
                                );
                                self.current_page_num = self
                                    .active_pages
                                    .iter()
                                    .position(|p| p.block_idx as usize >= target.block_in_section)
                                    .unwrap_or(0);
                            } else if let Some(ref idx) = self.active_index {
                                let target_blk = idx.toc[self.toc_cursor].block_index;
                                while self.pending_layout_anchor.is_some()
                                    && self.active_pages.last().is_some_and(|p| p.block_idx < target_blk)
                                    && self.active_pages.len() < 16_384
                                {
                                    self.extend_pending_pagination(storage, power, 64);
                                }
                                self.current_page_num = self
                                    .active_pages
                                    .iter()
                                    .position(|p| p.block_idx >= target_blk)
                                    .unwrap_or_else(|| self.active_pages.len().saturating_sub(1));
                            }
                            self.reader_overlay = ReaderOverlay::None;
                            self.load_current_page_with_prefetch(storage, power);
                            self.persistence_dirty = true;
                            let mode = self.interactive_refresh_mode();
                            self.redraw(sink, mode, power);
                        }
                        _ => {}
                    }
                }
            },
            Screen::VvDrop => match btn {
                Button::Back => {
                    portal.stop();
                    self.vvdrop_status = None;
                    self.vvdrop_client_badge_due_ms = None;
                    self.power_stats.wifi_powered = false;
                    self.screen = Screen::Home;
                    self.apply_screen_power_gates(power, portal, entropy, now_ms);
                    let mode = self.interactive_refresh_mode();
                    self.redraw(sink, mode, power);
                }
                Button::Confirm => {
                    if let Some(ref mut auth) = self.vvdrop_auth {
                        auth.rotate_pin(entropy);
                        let key = auth.pin_str().to_string();
                        portal.start(&key);
                        let st = portal.status();
                        self.vvdrop_qr = make_wifi_qr(&st.ssid, &key);
                        self.vvdrop_status = Some(st);
                        self.vvdrop_client_badge_due_ms = None;
                        self.last_vvdrop_activity_ms = now_ms;
                        let mode = self.interactive_refresh_mode();
                        self.redraw(sink, mode, power);
                    }
                }
                _ => {}
            },
            Screen::PocketPip => match self.pip.handle_button(btn, self.lang.is_ru()) {
                GameAction::Redraw(RefreshMode::FastDu) => {
                    self.persistence_dirty = true;
                    let mode = self.interactive_refresh_mode();
                    self.redraw(sink, mode, power);
                }
                GameAction::Redraw(mode) => {
                    self.persistence_dirty = true;
                    self.redraw(sink, mode, power);
                }
                GameAction::ExitToHome => {
                    self.persistence_dirty = true;
                    self.screen = Screen::Home;
                    let mode = self.interactive_refresh_mode();
                    self.redraw(sink, mode, power);
                }
                GameAction::None => {}
            },
            Screen::Chess => match self.chess.handle_button(btn) {
                GameAction::Redraw(RefreshMode::FastDu) => {
                    self.persistence_dirty = true;
                    let mode = self.interactive_refresh_mode();
                    self.redraw(sink, mode, power);
                }
                GameAction::Redraw(mode) => {
                    self.persistence_dirty = true;
                    self.redraw(sink, mode, power);
                }
                GameAction::ExitToHome => {
                    self.persistence_dirty = true;
                    self.screen = Screen::Home;
                    let mode = self.interactive_refresh_mode();
                    self.redraw(sink, mode, power);
                }
                GameAction::None => {}
            },
            Screen::Settings => match btn {
                Button::Back => {
                    self.persistence_dirty = true;
                    self.screen = Screen::Home;
                    let mode = self.interactive_refresh_mode();
                    self.redraw(sink, mode, power);
                }
                Button::Up | Button::Left => {
                    self.settings_cursor = (self.settings_cursor + 5) % 6;
                    self.redraw(sink, RefreshMode::FastDu, power);
                }
                Button::Down | Button::Right => {
                    self.settings_cursor = (self.settings_cursor + 1) % 6;
                    self.redraw(sink, RefreshMode::FastDu, power);
                }
                Button::Confirm => {
                    match self.settings_cursor {
                        0 => self.lang = self.lang.toggle(),
                        1 => {
                            self.full_scrub_interval = match self.full_scrub_interval {
                                8 => 12,
                                12 => 20,
                                20 => 32,
                                _ => 8,
                            };
                        }
                        2 => self.tilt_flick_enabled = !self.tilt_flick_enabled,
                        3 => {
                            self.sleep_poster = SleepPosterKind::from_u8(self.sleep_poster as u8 + 1);
                        }
                        4 => {
                            self.auto_sleep_mins = match self.auto_sleep_mins {
                                5 => 10,
                                10 => 15,
                                15 => 30,
                                _ => 5,
                            };
                        }
                        _ => {
                            self.battery = sensors.battery();
                            self.clock = sensors.now();
                            power.set_sd_power_save(false);
                            self.scan_and_ingest_books(storage);
                            self.refresh_home_cover(storage);
                            power.set_sd_power_save(true);
                        }
                    }
                    self.persistence_dirty = true;
                    let mode = self.interactive_refresh_mode();
                    self.redraw(sink, mode, power);
                }
                _ => {}
            },
            Screen::SleepPoster => {}
        }
    }

    fn apply_screen_power_gates<P: PowerControl, W: NetworkPortal, R: EntropySource>(
        &mut self,
        power: &mut P,
        portal: &mut W,
        entropy: &mut R,
        now_ms: u64,
    ) {
        // Wi-Fi strictly OFF everywhere except VVDrop
        if self.screen == Screen::VvDrop {
            self.release_open_book_ram();
            if self.vvdrop_auth.is_none() {
                self.vvdrop_auth = Some(VvDropAuth::new(entropy));
            }
            let pin = self.vvdrop_auth.as_ref().unwrap().pin_str().to_string();
            if !self.power_stats.wifi_powered {
                portal.start(&pin);
            }
            let st = portal.status();
            self.vvdrop_qr = make_wifi_qr(&st.ssid, &pin);
            self.vvdrop_status = Some(st);
            self.vvdrop_client_badge_due_ms = None;
            self.power_stats.wifi_powered = true;
            self.last_vvdrop_activity_ms = now_ms;
        } else if self.power_stats.wifi_powered {
            portal.stop();
            self.vvdrop_status = None;
            self.vvdrop_client_badge_due_ms = None;
            self.power_stats.wifi_powered = false;
        }

        // IMU flick strictly active ONLY in Reader when enabled
        let want_imu = self.screen == Screen::Reader && self.tilt_flick_enabled;
        if self.power_stats.imu_powered != want_imu {
            power.set_imu_flick(want_imu);
            self.power_stats.imu_powered = want_imu;
        }
    }

    pub fn enter_sleep<D: DisplaySink, S: StorageBackend, P: PowerControl, W: NetworkPortal>(
        &mut self,
        sink: &mut D,
        storage: &mut S,
        power: &mut P,
        portal: &mut W,
    ) {
        if self.screen != Screen::SleepPoster {
            self.prev_screen_before_sleep = self.screen;
        }
        self.persistence_dirty = false;
        self.flush_persistence(storage, power);
        portal.stop();
        self.vvdrop_status = None;
        self.vvdrop_client_badge_due_ms = None;
        power.set_imu_flick(false);
        self.power_stats.wifi_powered = false;
        self.power_stats.imu_powered = false;
        self.screen = Screen::SleepPoster;
        self.redraw(sink, RefreshMode::FullGc, power);
        power.set_panel_hv_standby(true);
        power.set_sd_power_save(true);
        self.power_stats.panel_hv_standby = true;
        self.power_stats.sd_power_save = true;
        power.enter_deep_sleep();
    }

    /// Periodic background tick (~200ms - 1000ms) running the **3-Stage Battery Governor**
    /// and polling VVDrop uploads only while `Screen::VvDrop` is open.
    pub fn tick<D, S, E, P, W>(
        &mut self,
        sink: &mut D,
        storage: &mut S,
        sensors: &mut E,
        power: &mut P,
        portal: &mut W,
    ) where
        D: DisplaySink,
        S: StorageBackend,
        E: DeviceSensors,
        P: PowerControl,
        W: NetworkPortal,
    {
        let now_ms = sensors.uptime_ms();

        // Check incoming VVDrop documents if Wi-Fi portal is open
        if self.screen == Screen::VvDrop {
            portal.poll_io(storage, now_ms);
            let mut clock_changed = false;
            if let Some(dt) = portal.take_synced_datetime() {
                let prev = self.clock;
                if sensors.set_datetime(dt) {
                    self.clock = sensors.now();
                } else {
                    self.clock = dt;
                }
                clock_changed = self.clock.hour != prev.hour || self.clock.minute != prev.minute;
            }
            if let Some(saved_path) = portal.take_uploaded_path() {
                self.last_vvdrop_activity_ms = now_ms;
                self.last_activity_ms = now_ms;
                power.set_sd_power_save(false);
                if self.ingest_saved_file(storage, &saved_path).is_ok() {
                    self.flush_persistence(storage, power);
                }
                self.vvdrop_status = Some(portal.status());
                self.redraw(sink, RefreshMode::HalfScrub, power);
                return;
            }
            if let Some((filename, raw)) = portal.take_pending_ingest() {
                self.last_vvdrop_activity_ms = now_ms;
                self.last_activity_ms = now_ms;
                power.set_sd_power_save(false);
                if self.ingest_raw_document(storage, &filename, &raw).is_ok() {
                    self.flush_persistence(storage, power);
                }
                self.vvdrop_status = Some(portal.status());
                self.redraw(sink, RefreshMode::HalfScrub, power);
                return;
            }
            let st = portal.status();
            if st.clients_connected > 0 || st.rx_bytes > 0 {
                self.last_vvdrop_activity_ms = now_ms;
                self.last_activity_ms = now_ms;
            }
            let urgent_changed = match self.vvdrop_status.as_ref() {
                None => true,
                Some(prev) => {
                    (prev.rx_bytes > 0) != (st.rx_bytes > 0)
                        || prev.last_received_file != st.last_received_file
                        || prev.busy_message != st.busy_message
                        || prev.pin != st.pin
                }
            };
            let clients_changed = self
                .vvdrop_status
                .as_ref()
                .is_some_and(|prev| prev.clients_connected != st.clients_connected);
            if urgent_changed || clock_changed {
                self.vvdrop_client_badge_due_ms = None;
                self.vvdrop_status = Some(st);
                self.redraw(sink, RefreshMode::FastDu, power);
                return;
            }
            if clients_changed {
                let due = *self
                    .vvdrop_client_badge_due_ms
                    .get_or_insert_with(|| now_ms.saturating_add(1_500));
                if now_ms >= due {
                    self.vvdrop_client_badge_due_ms = None;
                    self.vvdrop_status = Some(st);
                    self.redraw(sink, RefreshMode::FastDu, power);
                    return;
                }
            } else {
                self.vvdrop_client_badge_due_ms = None;
            }
            // Auto-close Wi-Fi after 3 minutes of inactivity so user can never accidentally drain battery!
            if now_ms.saturating_sub(self.last_vvdrop_activity_ms) >= 180_000 {
                portal.stop();
                self.vvdrop_status = None;
                self.vvdrop_client_badge_due_ms = None;
                self.power_stats.wifi_powered = false;
                self.screen = Screen::Home;
                self.redraw(sink, RefreshMode::HalfScrub, power);
                return;
            }
        }

        if self.screen == Screen::SleepPoster {
            return;
        }

        let idle_ms = now_ms.saturating_sub(self.last_activity_ms);

        // Background progressive pagination while actively reading (before Stage 1 idle shutdown)
        if self.screen == Screen::Reader
            && idle_ms < 2_500
            && !self.power_stats.panel_hv_standby
            && self.pending_layout_anchor.is_some()
        {
            self.extend_pending_pagination(storage, power, 24);
        }

        // Flush deferred catalog/journal writes calmly after 1.5s of user idle
        if self.persistence_dirty && idle_ms >= 1_500 {
            self.persistence_dirty = false;
            self.flush_persistence(storage, power);
        }

        // Stage 1 (2.5s): Shut off EPD high-voltage ±15V charge pump + gate microSD rail
        if idle_ms >= 2_500 && !self.power_stats.panel_hv_standby {
            power.set_panel_hv_standby(true);
            power.set_sd_power_save(true);
            self.power_stats.panel_hv_standby = true;
            self.power_stats.sd_power_save = true;
        }

        // Stage 2 (60s): Gate IMU wrist-tilt sensor if user paused reading
        if idle_ms >= 60_000 && self.power_stats.imu_powered {
            power.set_imu_flick(false);
            self.power_stats.imu_powered = false;
        }

        // Stage 3 (Auto-sleep minutes): Persist journal, draw Museum Sleep Poster, enter ESP32-C3 deep sleep
        let sleep_ms = self.auto_sleep_mins as u64 * 60_000;
        if idle_ms >= sleep_ms {
            self.enter_sleep(sink, storage, power, portal);
        }
    }

    pub fn redraw<D: DisplaySink, P: PowerControl>(&mut self, sink: &mut D, mode: RefreshMode, power: &mut P) {
        if self.power_stats.panel_hv_standby {
            power.set_panel_hv_standby(false);
            self.power_stats.panel_hv_standby = false;
        }
        render_to_sink(sink, mode, |canvas| {
            self.render_current_screen(canvas);
        });
    }

    pub fn render_current_screen(&self, canvas: &mut Canvas<'_>) {
        match self.screen {
            Screen::SleepPoster => {
                self.render_sleep_poster(canvas);
                return;
            }
            Screen::Reader => {
                self.render_reader_screen(canvas);
                return;
            }
            _ => {}
        }

        // Global Cupertino status bar for Desk / Library / Portal / Games / Settings
        self.render_status_bar(canvas);

        match self.screen {
            Screen::Home => self.render_home(canvas),
            Screen::Library => self.render_library(canvas),
            Screen::VvDrop => self.render_vvdrop(canvas),
            Screen::PocketPip => self.pip.render(canvas, self.lang.is_ru()),
            Screen::Chess => self.chess.render(canvas, self.lang.is_ru()),
            Screen::Settings => self.render_settings(canvas),
            Screen::Reader | Screen::SleepPoster => {}
        }
    }

    fn render_status_bar(&self, canvas: &mut Canvas<'_>) {
        let y = SAFE_INSETS.top as i32 + 28;
        let left = 36;
        let right = PORTRAIT_W as i32 - 36;
        let time_s = format!("{:02}:{:02}", self.clock.hour, self.clock.minute);
        canvas.draw_text(&baked_fonts::FONT_MONO_20_SEMI, left, y, &time_s, Tone::Ink);

        draw_battery_capsule(canvas, right - 33, y - 14, self.battery.percent, self.battery.charging);
        let bat_s = format!("{}%", self.battery.percent);
        canvas.draw_text_aligned(&baked_fonts::FONT_MONO_20_SEMI, right - 41, y, &bat_s, Tone::Ink, Align::Right);

        if self.power_stats.wifi_powered {
            canvas.draw_text_aligned(&baked_fonts::FONT_UI_19_SEMI, right - 104, y, "WI-FI", Tone::Ink, Align::Right);
        }
    }

    fn render_home(&self, canvas: &mut Canvas<'_>) {
        let s = tr(self.lang);
        let left = 36;
        let right = PORTRAIT_W as i32 - 36;
        let col_w = right - left;
        let cx = PORTRAIT_W as i32 / 2;

        // Top structural rule separating system telemetry from folio sheet
        canvas.hline(left, 56, col_w, Tone::Ink);

        // 1. Hero book focal point (frameless Swiss editorial typography on bare paper)
        if self.home_cursor == 0 {
            canvas.fill_rect(18, 86, 5, 258, Tone::Ink);
        }
        canvas.draw_text_tracked(&baked_fonts::FONT_UI_19_SEMI, left, 108, s.now_reading, Tone::Ink, 1);

        if let Some(entry) = self.catalog.get(self.active_book_idx) {
            let cur_page = entry.current_page_cached + 1;
            let total_pages = entry.total_pages_cached.max(1);
            let page_stat = format!("{} · {}/{}", entry.source_format, cur_page, total_pages);
            canvas.draw_text_aligned(
                &baked_fonts::FONT_MONO_20_SEMI,
                right,
                108,
                &page_stat,
                Tone::Ink,
                Align::Right,
            );

            let use_big = Canvas::count_wrapped_lines(&baked_fonts::FONT_SERIF_38_BOLD, col_w, 3, &entry.title) <= 2;
            let (title_font, title_step) = if use_big {
                (&baked_fonts::FONT_SERIF_38_BOLD, 46)
            } else {
                (&baked_fonts::FONT_SERIF_32_BOLD, 40)
            };
            let title_lines = canvas.draw_text_lines(title_font, left, 174, col_w, title_step, 2, &entry.title, Tone::Ink);
            let author_y = if title_lines > 1 { 262 } else { 220 };
            canvas.draw_text_truncated(
                &baked_fonts::FONT_SERIF_27_ITALIC,
                left,
                author_y,
                col_w,
                &entry.author,
                Tone::Ink,
            );

            let pct = (cur_page * 100)
                .checked_div(entry.total_pages_cached)
                .unwrap_or(0)
                .min(100);
            let track_y = 330;
            let track_w = col_w - 76;
            canvas.hline_dotted(left, track_y, track_w, 3, Tone::Ink);
            if pct > 0 {
                let fw = ((track_w as u32 * pct) / 100).max(6) as i32;
                canvas.fill_rect(left, track_y - 2, fw.min(track_w), 4, Tone::Ink);
            }
            let pct_s = format!("{pct}%");
            canvas.draw_text_aligned(
                &baked_fonts::FONT_MONO_20_SEMI,
                right,
                337,
                &pct_s,
                Tone::Ink,
                Align::Right,
            );
        } else {
            canvas.draw_text(&baked_fonts::FONT_SERIF_38_BOLD, left, 174, s.no_books_yet, Tone::Ink);
            canvas.draw_text_lines(
                &baked_fonts::FONT_SERIF_23_ITALIC,
                left,
                224,
                col_w,
                32,
                3,
                s.no_books_hint,
                Tone::Ink,
            );
        }

        // 2. Heavy architectural rule separating literature from utility index
        canvas.fill_rect(left, 368, col_w, 3, Tone::Ink);

        // 3. 1 + 2 + 2 Sharp Typographic Index Ledger
        let row1_y = 396;
        let row1_h = 80;
        let row2_y = 508;
        let row3_y = 626;
        let half_h = 92;
        let half_w = (col_w - 16) / 2;

        // Row 1 (01 Library)
        let lib_focus = self.home_cursor == 1;
        if lib_focus {
            canvas.fill_rect(left, row1_y, col_w, row1_h, Tone::Ink);
        }
        let lib_fg = if lib_focus { Tone::Paper } else { Tone::Ink };
        canvas.draw_text(&baked_fonts::FONT_MONO_20_SEMI, left + 14, row1_y + 47, "01", lib_fg);
        canvas.draw_text(&baked_fonts::FONT_UI_25_BOLD, left + 56, row1_y + 49, s.dock_library, lib_fg);
        let vol_str = format!("{} {}", self.catalog.len(), s.books_count_suffix);
        canvas.draw_text_aligned(&baked_fonts::FONT_MONO_20_SEMI, right - 14, row1_y + 47, &vol_str, lib_fg, Align::Right);

        // Honest 1-bpp dotted grid separators
        canvas.hline_dotted(left, 492, col_w, 3, Tone::Ink);
        canvas.hline_dotted(left, 612, col_w, 3, Tone::Ink);
        canvas.vline_dotted(cx, row2_y + 8, half_h - 16, 3, Tone::Ink);
        canvas.vline_dotted(cx, row3_y + 8, half_h - 16, 3, Tone::Ink);

        let chess_meta = if !self.chess.history.is_empty() {
            format!("#{}", self.chess.history.len())
        } else {
            String::new()
        };
        let wifi_meta = if self.power_stats.wifi_powered {
            String::from("AP")
        } else {
            String::new()
        };
        let pairs = [
            (2usize, left, row2_y, "02", s.dock_chess, chess_meta),
            (3usize, cx + 8, row2_y, "03", s.dock_pip, String::new()),
            (4usize, left, row3_y, "04", s.dock_vvdrop, wifi_meta),
            (5usize, cx + 8, row3_y, "05", s.dock_settings, String::new()),
        ];
        for (idx, tx, ty, num, label, meta) in &pairs {
            let focused = self.home_cursor == *idx;
            if focused {
                canvas.fill_rect(*tx, *ty, half_w, half_h, Tone::Ink);
            }
            let fg = if focused { Tone::Paper } else { Tone::Ink };
            canvas.draw_text(&baked_fonts::FONT_MONO_20_SEMI, *tx + 14, *ty + 32, num, fg);
            if !meta.is_empty() {
                canvas.draw_text_aligned(&baked_fonts::FONT_MONO_20_SEMI, *tx + half_w - 14, *ty + 32, meta, fg, Align::Right);
            }
            canvas.draw_text_truncated(
                &baked_fonts::FONT_UI_25_BOLD,
                *tx + 14,
                *ty + 69,
                half_w - 28,
                label,
                fg,
            );
        }

        let back_hint = if self.home_cursor > 0 { s.chin_back } else { "" };
        canvas.draw_chin_bar(&baked_fonts::FONT_UI_19_SEMI, [back_hint, s.chin_open, s.chin_prev, s.chin_next], Some(1));
    }

    fn render_library(&self, canvas: &mut Canvas<'_>) {
        let s = tr(self.lang);
        let left = 36;
        let right = PORTRAIT_W as i32 - 36;
        let col_w = right - left;
        let top = SAFE_INSETS.top as i32 + 52;
        canvas.draw_text(&baked_fonts::FONT_UI_32_BOLD, left, top + 30, s.library_title, Tone::Ink);

        let per_page = LIBRARY_PER_PAGE;
        let total_pages = self.catalog.len().div_ceil(per_page).max(1);
        let cur_page = (self.lib_cursor / per_page) + 1;
        let count_s = if self.catalog.is_empty() {
            format!("0 {}", s.books_count_suffix)
        } else {
            format!("{} {} · {}/{}", self.catalog.len(), s.books_count_suffix, cur_page, total_pages)
        };
        canvas.draw_text_aligned(&baked_fonts::FONT_MONO_20_SEMI, right, top + 30, &count_s, Tone::Ink, Align::Right);
        canvas.fill_rect(left, top + 42, col_w, 3, Tone::Ink);

        if self.catalog.is_empty() {
            canvas.draw_text(&baked_fonts::FONT_SERIF_32_BOLD, left, 280, s.no_books_yet, Tone::Ink);
            canvas.draw_text_lines(&baked_fonts::FONT_SERIF_23_ITALIC, left, 332, col_w, 30, 3, s.no_books_hint, Tone::Ink);
            canvas.draw_chin_bar(&baked_fonts::FONT_UI_19_SEMI, [s.chin_home, s.rescan_sd, "", ""], Some(1));
            return;
        }

        let has_notice = self.library_notice.is_some();
        let step_h = if has_notice { 68 } else { 78 };
        let window_start = (self.lib_cursor / per_page) * per_page;
        let visible_count = (self.catalog.len() - window_start).min(per_page);
        let pill_w = 44;
        let tx = left + 56;
        let tw = col_w - 56;
        let mut y = top + 49;

        for (offset, entry) in self.catalog.iter().skip(window_start).take(per_page).enumerate() {
            let idx = window_start + offset;
            let sel = idx == self.lib_cursor;
            if sel {
                canvas.fill_rect(left, y + 6, 4, step_h - 14, Tone::Ink);
                canvas.fill_rect(left + 8, y + 6, pill_w, 24, Tone::Ink);
            }
            let num_s = format!("{:02}", idx + 1);
            let num_x = if idx + 1 >= 100 { left + 10 } else { left + 16 };
            canvas.draw_text(
                &baked_fonts::FONT_MONO_20_SEMI,
                num_x,
                y + 25,
                &num_s,
                if sel { Tone::Paper } else { Tone::Ink },
            );

            let y_title = if has_notice { y + 25 } else { y + 28 };
            let y_sub = if has_notice { y + 50 } else { y + 57 };
            canvas.draw_text_truncated(&baked_fonts::FONT_SERIF_27_BOLD, tx, y_title, tw, &entry.title, Tone::Ink);

            let sub = if is_unknown_author(&entry.author) {
                fallback_folder_author(&entry.source_path)
            } else {
                entry.author.clone()
            };
            canvas.draw_text_truncated(&baked_fonts::FONT_SERIF_23_ITALIC, tx, y_sub, tw - 152, &sub, Tone::Ink);

            let pct = ((entry.current_page_cached + 1) * 100)
                .checked_div(entry.total_pages_cached)
                .unwrap_or(0)
                .min(100);
            let meta = if pct > 0 {
                format!("{} · {}%", entry.source_format, pct)
            } else {
                String::from(entry.source_format)
            };
            canvas.draw_text_aligned(&baked_fonts::FONT_MONO_20_SEMI, right, y_sub, &meta, Tone::Ink, Align::Right);

            if offset + 1 < visible_count {
                canvas.hline_dotted(left, y + step_h - 4, col_w, 3, Tone::Ink);
            }
            y += step_h;
        }

        if let Some(ref notice) = self.library_notice {
            canvas.fill_rect(left, 662, col_w, 66, Tone::Paper);
            canvas.stroke_rect(left, 662, col_w, 66, 2, Tone::Ink);
            canvas.draw_text_lines(&baked_fonts::FONT_UI_19_SEMI, left + 14, 688, col_w - 28, 24, 2, notice, Tone::Ink);
        }

        canvas.draw_chin_bar(&baked_fonts::FONT_UI_19_SEMI, [s.chin_home, s.chin_open, s.chin_prev, s.chin_next], Some(1));
    }

    fn render_reader_screen(&self, canvas: &mut Canvas<'_>) {
        let s = tr(self.lang);
        if let Some(ref page) = self.current_layout {
            let entry = self.catalog.get(self.active_book_idx);
            let title = entry.map(|e| e.title.as_str()).unwrap_or("");
            let cur_blk_key = if self.active_direct.is_some() {
                ((self.direct_section_idx as u32) << DIRECT_SEC_SHIFT)
                    | page.anchor.block_idx.min(255)
            } else {
                page.anchor.block_idx
            };
            let bookmarked = entry
                .and_then(|e| e.bookmarked_block)
                .map(|b| b == cur_blk_key)
                .unwrap_or(false);
            let ch_hint = if !page.chapter_hint.is_empty() {
                page.chapter_hint.as_str()
            } else {
                title
            };
            render_page(
                page,
                &self.typo,
                canvas,
                &PageChrome {
                    book_title: title,
                    chapter_title: ch_hint,
                    page_num: self.display_page_num(),
                    total_pages: self.estimated_total_pages(),
                    bookmarked,
                    chin_hints: [s.chin_home, s.chin_aa, s.chin_page_prev, s.chin_page_next],
                },
            );
        }

        if self.reader_overlay == ReaderOverlay::AaHud {
            canvas.dim_scrim();
            let sx = 32;
            let sw = PORTRAIT_W as i32 - 64;
            let sh = 474;
            let sy = 718 - sh;
            canvas.fill_rect(sx - 8, sy - 8, sw + 16, sh + 16, Tone::Paper);
            canvas.fill_rect(sx, sy, sw, sh, Tone::Paper);
            canvas.stroke_rect(sx, sy, sw, sh, 2, Tone::Ink);
            canvas.fill_rect(sx, sy, sw, 6, Tone::Ink);
            canvas.draw_text(&baked_fonts::FONT_UI_25_BOLD, sx + 22, sy + 42, s.reader_hud_title, Tone::Ink);
            canvas.hline(sx + 20, sy + 58, sw - 40, Tone::Ink);

            let fam_val = match (self.typo.font_family, self.lang) {
                (FontFamily::Serif, _) => "Palatino",
                (FontFamily::School, UiLang::Ru) => "Школьная",
                (FontFamily::School, UiLang::En) => "Schoolbook",
                (FontFamily::Sans, _) => "OpenSans",
            };
            let size_val = match self.typo.font_size {
                FontSizePreset::Compact => "23 px",
                FontSizePreset::Classic => "27 px",
                FontSizePreset::Large => "32 px",
                FontSizePreset::Extra => "38 px",
            };
            let marg_val = match (self.typo.margin_preset, self.lang) {
                (MarginPreset::Narrow, UiLang::Ru) => "Узкие",
                (MarginPreset::Editorial, UiLang::Ru) => "Книжные",
                (MarginPreset::Wide, UiLang::Ru) => "Широкие",
                (MarginPreset::Narrow, _) => "Narrow",
                (MarginPreset::Editorial, _) => "Editorial",
                (MarginPreset::Wide, _) => "Wide",
            };
            let bm_on = self
                .catalog
                .get(self.active_book_idx)
                .and_then(|c| c.bookmarked_block)
                .is_some();

            let rows = [
                (s.font_family, fam_val, false),
                (s.font_size, size_val, true),
                (s.margins, marg_val, false),
                (s.bookmark_toggle, if bm_on { s.bookmark_saved } else { s.bookmark_none }, false),
                (s.chapters_toc, "→", false),
                (s.back_to_desk, "←", false),
            ];
            let mut y = sy + 72;
            for (idx, (lbl, val, mono_val)) in rows.iter().enumerate() {
                let sel = idx == self.hud_cursor;
                let fg = if sel { Tone::Paper } else { Tone::Ink };
                if sel {
                    canvas.fill_rect(sx + 16, y, sw - 32, 54, Tone::Ink);
                } else if idx + 1 < rows.len() && idx + 1 != self.hud_cursor {
                    canvas.hline_dotted(sx + 20, y + 58, sw - 40, 3, Tone::Ink);
                }
                canvas.draw_text(&baked_fonts::FONT_UI_22_SEMI, sx + 28, y + 35, lbl, fg);
                let val_font = if *mono_val {
                    &baked_fonts::FONT_MONO_20_SEMI
                } else {
                    &baked_fonts::FONT_UI_22_SEMI
                };
                canvas.draw_text_aligned(val_font, sx + sw - 28, y + 35, val, fg, Align::Right);
                y += 64;
            }
            let ok_hint = if self.hud_cursor >= 4 { s.chin_open } else { s.chin_change };
            canvas.draw_chin_bar(&baked_fonts::FONT_UI_19_SEMI, [s.chin_back, ok_hint, s.chin_prev, s.chin_next], Some(1));
        } else if self.reader_overlay == ReaderOverlay::TocDrawer {
            canvas.dim_scrim();
            let sx = 32;
            let sw = PORTRAIT_W as i32 - 64;
            let sh = 620;
            let sy = 96;
            canvas.fill_rect(sx - 8, sy - 8, sw + 16, sh + 16, Tone::Paper);
            canvas.fill_rect(sx, sy, sw, sh, Tone::Paper);
            canvas.stroke_rect(sx, sy, sw, sh, 2, Tone::Ink);
            canvas.fill_rect(sx, sy, sw, 6, Tone::Ink);
            canvas.draw_text(&baked_fonts::FONT_UI_25_BOLD, sx + 22, sy + 42, s.chapters_toc, Tone::Ink);

            let total_toc = if let Some(ref d) = self.active_direct {
                d.toc.len()
            } else if let Some(ref idx) = self.active_index {
                idx.toc.len()
            } else {
                0
            };
            if total_toc > 0 {
                let toc_pos = format!("{}/{}", (self.toc_cursor + 1).min(total_toc), total_toc);
                canvas.draw_text_aligned(&baked_fonts::FONT_MONO_20_SEMI, sx + sw - 22, sy + 42, &toc_pos, Tone::Ink, Align::Right);
            }
            canvas.hline(sx + 20, sy + 58, sw - 40, Tone::Ink);

            let win = (self.toc_cursor / 8) * 8;
            let mut y = sy + 72;
            let draw_toc_row = |canvas: &mut Canvas<'_>, y: i32, num: usize, sel: bool, last: bool, label: &str| {
                let fg = if sel { Tone::Paper } else { Tone::Ink };
                if sel {
                    canvas.fill_rect(sx + 16, y, sw - 32, 54, Tone::Ink);
                } else if !last {
                    canvas.hline_dotted(sx + 20, y + 58, sw - 40, 3, Tone::Ink);
                }
                let num_s = format!("{:02}", num + 1);
                canvas.draw_text(&baked_fonts::FONT_MONO_20_SEMI, sx + 26, y + 35, &num_s, fg);
                canvas.draw_text_truncated(&baked_fonts::FONT_UI_22_SEMI, sx + 64, y + 35, sw - 92, label, fg);
            };
            if let Some(ref d) = self.active_direct {
                let vis = d.toc.len().saturating_sub(win).min(8);
                for (offset, t) in d.toc.iter().skip(win).take(8).enumerate() {
                    let row_i = win + offset;
                    draw_toc_row(canvas, y, row_i, row_i == self.toc_cursor, offset + 1 == vis, &t.title);
                    y += 64;
                }
            } else if let Some(ref idx) = self.active_index {
                let vis = idx.toc.len().saturating_sub(win).min(8);
                for (offset, t) in idx.toc.iter().skip(win).take(8).enumerate() {
                    let row_i = win + offset;
                    draw_toc_row(canvas, y, row_i, row_i == self.toc_cursor, offset + 1 == vis, &t.title);
                    y += 64;
                }
            }
            canvas.draw_chin_bar(&baked_fonts::FONT_UI_19_SEMI, [s.chin_back, s.chin_open, s.chin_prev, s.chin_next], Some(1));
        }
    }

    fn render_vvdrop(&self, canvas: &mut Canvas<'_>) {
        let s = tr(self.lang);
        let left = 36;
        let right = PORTRAIT_W as i32 - 36;
        let col_w = right - left;
        let top = SAFE_INSETS.top as i32 + 52;

        canvas.draw_text(&baked_fonts::FONT_UI_32_BOLD, left, top + 34, s.vvdrop_title, Tone::Ink);
        canvas.draw_text_aligned(&baked_fonts::FONT_MONO_20_SEMI, right, top + 34, "WPA2-PSK", Tone::Ink, Align::Right);
        canvas.fill_rect(left, top + 48, col_w, 3, Tone::Ink);

        let ssid = self
            .vvdrop_status
            .as_ref()
            .map(|st| st.ssid.as_str())
            .unwrap_or("VVBook-Folio");
        let url = self
            .vvdrop_status
            .as_ref()
            .map(|st| st.url.as_str())
            .unwrap_or(VVDROP_URL);

        let qr_y = top + 68;
        let (qr_m_px, qr_w) = if let Some(ref qr) = self.vvdrop_qr {
            let m_px = if qr.size > 33 { 4 } else if qr.size > 25 { 5 } else { 6 };
            (m_px, (qr.size as i32 + 4) * m_px)
        } else {
            (5, 165)
        };
        if canvas.intersects_y(qr_y - 4, qr_y + qr_w + 4)
            && let Some(ref qr) = self.vvdrop_qr
        {
            qr.draw(canvas, left, qr_y, qr_m_px, 2);
            canvas.stroke_rect(left, qr_y, qr_w, qr_w, 2, Tone::Ink);
        }

        // Right column beside Wi-Fi join QR
        let rx = left + qr_w + 24;
        let rw = right - rx;
        let net_lbl = if self.lang.is_ru() { "1 · СЕТЬ WI-FI" } else { "1 · WI-FI (WPA2)" };
        let scan_lbl = if self.lang.is_ru() { "СКАН QR ДЛЯ ВХОДА" } else { "SCAN QR TO JOIN" };
        canvas.draw_text(&baked_fonts::FONT_UI_19_SEMI, rx, qr_y + 22, net_lbl, Tone::Ink);
        canvas.draw_text_truncated(&baked_fonts::FONT_UI_25_BOLD, rx, qr_y + 56, rw, ssid, Tone::Ink);
        canvas.hline_dotted(rx, qr_y + 76, rw, 3, Tone::Ink);
        canvas.draw_text(&baked_fonts::FONT_UI_19_SEMI, rx, qr_y + 106, scan_lbl, Tone::Ink);
        canvas.draw_text_lines(&baked_fonts::FONT_UI_19_SEMI, rx, qr_y + 134, rw, 26, 2, s.vvdrop_rotate_pin, Tone::Ink);

        // WPA2 passphrase / access key block
        let key_y = qr_y + qr_w + 32;
        canvas.draw_text(&baked_fonts::FONT_UI_19_SEMI, left, key_y, s.vvdrop_pin_label, Tone::Ink);
        let raw_pin = self.vvdrop_auth.as_ref().map(|a| a.pin_str()).unwrap_or("42819305");
        let pretty_key = if raw_pin.len() == 8 {
            format!("{}  {}", &raw_pin[..4], &raw_pin[4..])
        } else if raw_pin.len() == 6 {
            format!("{}  {}", &raw_pin[..3], &raw_pin[3..])
        } else {
            raw_pin.to_string()
        };
        canvas.draw_text(&baked_fonts::FONT_UI_44_BOLD, left, key_y + 54, &pretty_key, Tone::Ink);
        canvas.fill_rect(left, key_y + 74, col_w, 2, Tone::Ink);

        // Browser portal URL row
        let url_y = key_y + 108;
        let url_lbl = if self.lang.is_ru() { "2 · ОТКРОЙТЕ В БРАУЗЕРЕ" } else { "2 · OPEN IN BROWSER" };
        canvas.draw_text(&baked_fonts::FONT_UI_19_SEMI, left, url_y, url_lbl, Tone::Ink);
        canvas.draw_text_truncated(&baked_fonts::FONT_MONO_20_SEMI, left, url_y + 34, col_w, url, Tone::Ink);
        canvas.hline_dotted(left, url_y + 54, col_w, 3, Tone::Ink);

        // Live transfer status & supported formats
        let status_y = url_y + 88;
        let st_kicker = if self.lang.is_ru() { "СОСТОЯНИЕ" } else { "STATUS" };
        canvas.draw_text(&baked_fonts::FONT_UI_19_SEMI, left, status_y, st_kicker, Tone::Ink);
        let status_line: String = if let Some(ref st) = self.vvdrop_status {
            if st.rx_bytes > 0 {
                if self.lang.is_ru() {
                    format!("Загрузка: {} КБ", (st.rx_bytes / 1024).max(1))
                } else {
                    format!("Receiving: {} KB", (st.rx_bytes / 1024).max(1))
                }
            } else if let Some(ref msg) = st.busy_message {
                msg.clone()
            } else if let Some(ref file) = st.last_received_file {
                if self.lang.is_ru() {
                    format!("Принято: {file}")
                } else {
                    format!("Saved: {file}")
                }
            } else if st.clients_connected > 0 {
                if self.lang.is_ru() {
                    String::from("Устройство подключено · Готово")
                } else {
                    String::from("Client connected · Ready")
                }
            } else if self.lang.is_ru() {
                String::from("Ожидание подключения…")
            } else {
                String::from("Ready for transfer")
            }
        } else if self.lang.is_ru() {
            String::from("Ожидание подключения…")
        } else {
            String::from("Ready for transfer")
        };
        canvas.draw_text_truncated(&baked_fonts::FONT_UI_25_BOLD, left, status_y + 34, col_w, &status_line, Tone::Ink);
        canvas.draw_text_truncated(&baked_fonts::FONT_UI_19_SEMI, left, status_y + 66, col_w, s.vvdrop_waiting, Tone::Ink);

        canvas.draw_chin_bar(&baked_fonts::FONT_UI_19_SEMI, [s.chin_home, s.chin_new_pin, "", ""], Some(1));
    }

    fn render_settings(&self, canvas: &mut Canvas<'_>) {
        let s = tr(self.lang);
        let left = 36;
        let right = PORTRAIT_W as i32 - 36;
        let col_w = right - left;
        let top = SAFE_INSETS.top as i32 + 52;
        canvas.draw_text(&baked_fonts::FONT_UI_32_BOLD, left, top + 34, s.settings_title, Tone::Ink);
        canvas.fill_rect(left, top + 48, col_w, 3, Tone::Ink);

        let poster_lbl = match (self.sleep_poster, self.lang.is_ru()) {
            (SleepPosterKind::Literary, true) => "1. Текущая книга",
            (SleepPosterKind::Literary, false) => "1. Current Book",
            (SleepPosterKind::Cupertino, true) => "2. Эпиграф",
            (SleepPosterKind::Cupertino, false) => "2. Epigraph",
            (SleepPosterKind::PipStarlight, true) => "3. Минимум",
            (SleepPosterKind::PipStarlight, false) => "3. Minimal",
        };
        let sec_val = format!(
            "{}% · {} mV · {} {}",
            self.battery.percent,
            self.battery.millivolts,
            self.catalog.len(),
            s.books_count_suffix
        );
        let on_off = |b: bool| -> &'static str {
            match (b, self.lang.is_ru()) {
                (true, true) => "Вкл",
                (true, false) => "On",
                (false, true) => "Выкл",
                (false, false) => "Off",
            }
        };
        let min_unit = if self.lang.is_ru() { "мин" } else { "min" };
        let pp_unit = if self.lang.is_ru() { "стр." } else { "pp" };

        let items = [
            (s.set_lang, s.set_lang_val.to_string(), false),
            (s.set_refresh, format!("{} {}", self.full_scrub_interval, pp_unit), true),
            (s.set_tilt, on_off(self.tilt_flick_enabled).to_string(), false),
            (s.set_poster, poster_lbl.to_string(), false),
            (s.set_autosleep, format!("{} {}", self.auto_sleep_mins, min_unit), true),
            (s.set_security, sec_val, true),
        ];

        let mut y = top + 72;
        for (idx, (lbl, val, mono_val)) in items.iter().enumerate() {
            let sel = idx == self.settings_cursor;
            let fg = if sel { Tone::Paper } else { Tone::Ink };
            if sel {
                canvas.fill_rect(left, y, col_w, 70, Tone::Ink);
            } else if idx + 1 < items.len() && idx + 1 != self.settings_cursor {
                canvas.hline_dotted(left, y + 78, col_w, 3, Tone::Ink);
            }
            canvas.draw_text(&baked_fonts::FONT_UI_22_SEMI, left + 16, y + 43, lbl, fg);
            let val_font = if *mono_val {
                &baked_fonts::FONT_MONO_20_SEMI
            } else {
                &baked_fonts::FONT_UI_22_SEMI
            };
            canvas.draw_text_aligned(val_font, right - 16, y + 43, val, fg, Align::Right);
            y += 88;
        }

        canvas.draw_chin_bar(&baked_fonts::FONT_UI_19_SEMI, [s.chin_home, s.chin_change, s.chin_prev, s.chin_next], Some(1));
    }

    fn render_sleep_poster(&self, canvas: &mut Canvas<'_>) {
        let s = tr(self.lang);
        let cx = PORTRAIT_W as i32 / 2;
        let left = 56;
        let right = PORTRAIT_W as i32 - 56;
        let w = right - left;

        match self.sleep_poster {
            SleepPosterKind::Literary => {
                canvas.fill_rect(left, 112, w, 4, Tone::Ink);
                canvas.draw_text_tracked(&baked_fonts::FONT_UI_19_SEMI, left, 148, s.now_reading, Tone::Ink, 1);

                if let Some(entry) = self.catalog.get(self.active_book_idx) {
                    let lines = canvas.draw_text_lines(
                        &baked_fonts::FONT_SERIF_38_BOLD,
                        left,
                        252,
                        w,
                        48,
                        3,
                        &entry.title,
                        Tone::Ink,
                    );
                    let author_y = 252 + (lines.saturating_sub(1) as i32) * 48 + 48;
                    canvas.draw_text_lines(
                        &baked_fonts::FONT_SERIF_27_ITALIC,
                        left,
                        author_y,
                        w,
                        34,
                        2,
                        &entry.author,
                        Tone::Ink,
                    );

                    let cur_p = entry.current_page_cached + 1;
                    let tot_p = entry.total_pages_cached.max(1);
                    let pct = (cur_p * 100)
                        .checked_div(entry.total_pages_cached)
                        .unwrap_or(0)
                        .min(100);
                    canvas.hline_dotted(left, 548, w, 3, Tone::Ink);
                    if pct > 0 {
                        let fw = ((w as u32 * pct) / 100).max(6) as i32;
                        canvas.fill_rect(left, 546, fw.min(w), 4, Tone::Ink);
                    }
                    let stat = format!("{} · {}% · {}/{}", entry.source_format, pct, cur_p, tot_p);
                    canvas.draw_text(&baked_fonts::FONT_MONO_20_SEMI, left, 584, &stat, Tone::Ink);
                } else {
                    canvas.draw_text_tracked(&baked_fonts::FONT_UI_44_BOLD, left, 320, "VVBOOK", Tone::Ink, 3);
                    canvas.draw_text(&baked_fonts::FONT_SERIF_27_ITALIC, left, 376, s.no_books_yet, Tone::Ink);
                }
                canvas.hline_dotted(left, 668, w, 3, Tone::Ink);
            }
            SleepPosterKind::Cupertino => {
                let (quote, auth) = if self.lang.is_ru() {
                    (
                        "«Чтение — вот лучшее учение. Следовать за мыслью великого человека — есть наука самая занимательная.»",
                        "— А. С. Пушкин",
                    )
                } else {
                    (
                        "“A room without books is like a body without a soul.”",
                        "— Marcus Tullius Cicero",
                    )
                };
                canvas.fill_rect(left, 264, 72, 4, Tone::Ink);
                let q_lines = canvas.draw_text_lines(
                    &baked_fonts::FONT_SERIF_32_ITALIC,
                    left,
                    336,
                    w,
                    44,
                    5,
                    quote,
                    Tone::Ink,
                );
                let auth_y = 336 + (q_lines.saturating_sub(1) as i32) * 44 + 54;
                canvas.draw_text(&baked_fonts::FONT_UI_22_SEMI, left, auth_y, auth, Tone::Ink);
                canvas.hline_dotted(left, auth_y + 44, w, 3, Tone::Ink);
            }
            SleepPosterKind::PipStarlight => {
                canvas.draw_text_aligned(&baked_fonts::FONT_UI_44_BOLD, cx, 356, "VVBOOK", Tone::Ink, Align::Center);
                canvas.hline_dotted(cx - 64, 386, 128, 4, Tone::Ink);
                let sub = if self.lang.is_ru() {
                    "Бумага, тушь и тишина"
                } else {
                    "Paper, ink & silence"
                };
                canvas.draw_text_aligned(&baked_fonts::FONT_SERIF_23_ITALIC, cx, 428, sub, Tone::Ink, Align::Center);
            }
        }

        canvas.draw_text_aligned(&baked_fonts::FONT_UI_19_SEMI, cx, PORTRAIT_H as i32 - 42, s.wake_hint, Tone::Ink, Align::Center);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::Cell;
    use vvbook_pal::{DirEntry, ROW_2BPP_BYTES, StorageError};

    struct MemStorage {
        files: BTreeMap<String, Vec<u8>>,
        dirs: Vec<String>,
        read_calls: Cell<usize>,
    }

    impl MemStorage {
        fn new() -> Self {
            Self {
                files: BTreeMap::new(),
                dirs: Vec::new(),
                read_calls: Cell::new(0),
            }
        }
    }

    impl StorageBackend for MemStorage {
        fn exists(&self, path: &str) -> bool {
            self.files.contains_key(path)
        }
        fn file_len(&self, path: &str) -> Result<u64, StorageError> {
            self.files.get(path).map(|v| v.len() as u64).ok_or(StorageError::NotFound)
        }
        fn read_all(&self, path: &str) -> Result<Vec<u8>, StorageError> {
            self.read_calls.set(self.read_calls.get() + 1);
            self.files.get(path).cloned().ok_or(StorageError::NotFound)
        }
        fn read_range(&self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize, StorageError> {
            self.read_calls.set(self.read_calls.get() + 1);
            let data = self.files.get(path).ok_or(StorageError::NotFound)?;
            let off = offset as usize;
            if off >= data.len() {
                return Ok(0);
            }
            let n = buf.len().min(data.len() - off);
            buf[..n].copy_from_slice(&data[off..off + n]);
            Ok(n)
        }
        fn write_all(&mut self, path: &str, data: &[u8]) -> Result<(), StorageError> {
            self.files.insert(path.to_string(), data.to_vec());
            Ok(())
        }
        fn write_atomic(&mut self, path: &str, data: &[u8]) -> Result<(), StorageError> {
            self.files.insert(path.to_string(), data.to_vec());
            Ok(())
        }
        fn append(&mut self, path: &str, data: &[u8]) -> Result<(), StorageError> {
            self.files.entry(path.to_string()).or_default().extend_from_slice(data);
            Ok(())
        }
        fn remove(&mut self, path: &str) -> Result<(), StorageError> {
            self.files.remove(path);
            Ok(())
        }
        fn mkdir_all(&mut self, path: &str) -> Result<(), StorageError> {
            self.dirs.push(path.to_string());
            Ok(())
        }
        fn list_dir(&self, path: &str) -> Result<Vec<DirEntry>, StorageError> {
            let prefix = if path == "/" || path.is_empty() {
                String::from("/")
            } else if path.ends_with('/') {
                path.to_string()
            } else {
                format!("{path}/")
            };
            let mut seen_dirs = alloc::collections::BTreeSet::new();
            let mut out = Vec::new();
            for (k, v) in &self.files {
                if let Some(rest) = k.strip_prefix(&prefix) {
                    if let Some((dir_name, _)) = rest.split_once('/') {
                        if seen_dirs.insert(dir_name.to_string()) {
                            out.push(DirEntry {
                                name: dir_name.to_string(),
                                is_dir: true,
                                size: 0,
                            });
                        }
                    } else if !rest.is_empty() {
                        out.push(DirEntry {
                            name: rest.to_string(),
                            is_dir: false,
                            size: v.len() as u64,
                        });
                    }
                }
            }
            for d in &self.dirs {
                if let Some(rest) = d.strip_prefix(&prefix)
                    && !rest.is_empty()
                {
                    let dir_name = rest.split('/').next().unwrap_or(rest);
                    if seen_dirs.insert(dir_name.to_string()) {
                        out.push(DirEntry {
                            name: dir_name.to_string(),
                            is_dir: true,
                            size: 0,
                        });
                    }
                }
            }
            Ok(out)
        }
    }

    struct MockSink {
        fb: Vec<u8>,
        frames: usize,
        last_mode: RefreshMode,
    }
    impl MockSink {
        fn new() -> Self {
            Self {
                fb: vec![0xFFu8; PORTRAIT_H * ROW_2BPP_BYTES],
                frames: 0,
                last_mode: RefreshMode::FullGc,
            }
        }
    }
    impl DisplaySink for MockSink {
        fn begin_frame(&mut self, mode: RefreshMode) {
            self.last_mode = mode;
        }
        fn write_strip_2bpp(&mut self, y_start: usize, rows: usize, packed: &[u8]) {
            let off = y_start * ROW_2BPP_BYTES;
            let len = rows * ROW_2BPP_BYTES;
            self.fb[off..off + len].copy_from_slice(&packed[..len]);
        }
        fn end_frame(&mut self, _mode: RefreshMode) {
            self.frames += 1;
        }
    }

    struct MockSensors {
        uptime: u64,
    }
    impl DeviceSensors for MockSensors {
        fn battery(&mut self) -> BatteryStatus {
            BatteryStatus { percent: 88, millivolts: 4080, current_ma: -2, charging: false }
        }
        fn now(&mut self) -> DateTime {
            DateTime { year: 2026, month: 11, day: 12, hour: 19, minute: 30, second: 0 }
        }
        fn uptime_ms(&self) -> u64 {
            self.uptime
        }
        fn temperature_c_x10(&mut self) -> i16 {
            245
        }
    }

    struct MockPower {
        deep_sleeps: usize,
        hv_standby: bool,
        sd_save: bool,
        imu: bool,
    }
    impl PowerControl for MockPower {
        fn enter_deep_sleep(&mut self) {
            self.deep_sleeps += 1;
        }
        fn set_imu_flick(&mut self, enabled: bool) {
            self.imu = enabled;
        }
        fn set_rotated_180(&mut self, _r: bool) {}
        fn controller_name(&self) -> &'static str { "MockEPD" }
        fn set_panel_hv_standby(&mut self, standby: bool) {
            self.hv_standby = standby;
        }
        fn set_sd_power_save(&mut self, ps: bool) {
            self.sd_save = ps;
        }
    }

    struct MockPortal {
        active: bool,
        pending: Option<(String, Vec<u8>)>,
    }
    impl NetworkPortal for MockPortal {
        fn start(&mut self, _pin: &str) { self.active = true; }
        fn stop(&mut self) { self.active = false; }
        fn status(&mut self) -> vvbook_pal::PortalStatus {
            vvbook_pal::PortalStatus {
                active: self.active,
                ssid: "VVBook-Folio".into(),
                url: "http://192.168.4.1".into(),
                pin: "42819305".into(),
                clients_connected: 0,
                rx_bytes: 0,
                total_bytes: 0,
                last_received_file: None,
                busy_message: None,
            }
        }
        fn take_pending_ingest(&mut self) -> Option<(String, Vec<u8>)> {
            self.pending.take()
        }
    }

    struct MockEntropy(u64);
    impl EntropySource for MockEntropy {
        fn fill_bytes(&mut self, out: &mut [u8]) {
            for b in out.iter_mut() {
                self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
                *b = (self.0 >> 32) as u8;
            }
        }
    }

    #[test]
    fn full_device_lifecycle_prefetch_power_and_reboot_persistence() {
        let mut storage = MemStorage::new();
        // Long Russian book so it spans >= 8 pages
        let mut long_md = String::from("# Сказка\n\n");
        for i in 1..=35 {
            long_md.push_str(&format!(
                "Глава {i}. У лукоморья дуб зелёный; златая цепь на дубе том: и днём и ночью кот учёный всё ходит по цепи кругом. Идёт направо — песнь заводит, налево — сказку говорит.\n\n"
            ));
        }
        storage.write_all("/books/lukomorye.md", long_md.as_bytes()).unwrap();

        let mut sink = MockSink::new();
        let mut sensors = MockSensors { uptime: 1000 };
        let mut power = MockPower { deep_sleeps: 0, hv_standby: false, sd_save: false, imu: true };
        let mut portal = MockPortal { active: true, pending: None };
        let mut ent = MockEntropy(12345);

        let mut app = FolioApp::new();
        app.boot(&mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
        assert_eq!(app.catalog.len(), 1);
        assert!(!portal.active, "Wi-Fi must be powered off at boot");
        assert!(!power.imu, "IMU must be off by default");
        assert!(power.sd_save, "SD power save must be enabled when idle");

        // Open Reader via Confirm
        app.handle_event(InputEvent::Press(Button::Confirm), &mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
        assert_eq!(app.screen, Screen::Reader);
        assert!(app.active_pages.len() >= 4);

        // Turn 3 pages right -> must hit SRAM prefetch cache with ZERO extra SD reads!
        let reads_before = storage.read_calls.get();
        for _ in 0..3 {
            app.handle_event(InputEvent::Press(Button::Right), &mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
        }
        assert_eq!(storage.read_calls.get(), reads_before, "Prefetched page turns must not wake SD card!");
        assert!(app.power_stats.pages_from_prefetch >= 2);
        assert!(power.sd_save);

        // Switch language to Russian in Settings, then trigger Stage 1..3 idle sleep
        app.handle_event(InputEvent::Press(Button::Back), &mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
        app.lang = UiLang::Ru;
        app.flush_persistence(&mut storage, &mut power);

        sensors.uptime = 5_000;
        app.tick(&mut sink, &mut storage, &mut sensors, &mut power, &mut portal);
        assert!(power.hv_standby, "Stage 1 must drop HV rails after 2.5s");

        sensors.uptime = 1_000_000;
        app.tick(&mut sink, &mut storage, &mut sensors, &mut power, &mut portal);
        assert_eq!(app.screen, Screen::SleepPoster);
        assert_eq!(power.deep_sleeps, 1);

        // Cold reboot into fresh FolioApp over same SD card -> verifies persisted page & Russian language!
        let mut app2 = FolioApp::new();
        app2.boot(&mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
        assert_eq!(app2.lang, UiLang::Ru);
        assert_eq!(app2.catalog[0].current_page_cached, 3);
    }

    #[test]
    fn strip_vs_full_frame_exact_equivalence_all_screens() {
        let mut storage = MemStorage::new();
        storage.write_all("/books/test.md", b"# Hello\n\nOnce upon a time in Cupertino.").unwrap();
        let mut sink = MockSink::new();
        let mut sensors = MockSensors { uptime: 100 };
        let mut power = MockPower { deep_sleeps: 0, hv_standby: false, sd_save: false, imu: false };
        let mut portal = MockPortal { active: false, pending: None };
        let mut ent = MockEntropy(99);
        let mut app = FolioApp::new();
        app.boot(&mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);

        let screens = [Screen::Home, Screen::Library, Screen::VvDrop, Screen::PocketPip, Screen::Chess, Screen::Settings, Screen::SleepPoster];
        for scr in screens {
            app.screen = scr;
            app.redraw(&mut sink, RefreshMode::FullGc, &mut power);
            let mut full = vec![0xFFu8; PORTRAIT_H * ROW_2BPP_BYTES];
            let mut c = Canvas::new(&mut full, 0, PORTRAIT_H);
            c.clear(Tone::Paper);
            app.render_current_screen(&mut c);
            assert_eq!(sink.fb, full, "Strip rendering diverged on screen {:?}", scr);
        }
    }

    #[test]
    fn reader_hud_typography_bookmarks_starberries_and_vvdrop_ingest() {
        let mut storage = MemStorage::new();
        let mut long_md = String::from("# Каштанка\n\n");
        for i in 1..=45 {
            long_md.push_str(&format!(
                "Абзац {i}. Молодая рыжая собака — помесь таксы с дворняжкой — очень похожа мордой на лисицу, бегала взад и вперёд по тротуару и с тревогой оглядывалась на запертую дверь.\n\n"
            ));
        }
        storage.write_all("/books/kashtanka.md", long_md.as_bytes()).unwrap();

        let mut sink = MockSink::new();
        let mut sensors = MockSensors { uptime: 100 };
        let mut power = MockPower { deep_sleeps: 0, hv_standby: false, sd_save: false, imu: false };
        let mut portal = MockPortal { active: false, pending: None };
        let mut ent = MockEntropy(777);

        let mut app = FolioApp::new();
        app.boot(&mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
        assert_eq!(app.catalog.len(), 1);

        // Open book into Reader
        app.handle_event(InputEvent::Press(Button::Confirm), &mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
        assert_eq!(app.screen, Screen::Reader);
        let init_pages = app.active_pages.len();
        assert!(init_pages >= 5);

        // Turn 5 pages -> earns 1 Reading Starberry for user's daughter!
        let stars_before = app.pip.progress.starberries;
        for _ in 0..5 {
            app.handle_event(InputEvent::Press(Button::Right), &mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
        }
        assert_eq!(app.pip.progress.starberries, stars_before + 1);

        // Open Aa HUD via Confirm, cycle font family -> Serif & larger size, toggle bookmark
        app.handle_event(InputEvent::Press(Button::Confirm), &mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
        assert_eq!(app.reader_overlay, ReaderOverlay::AaHud);
        assert_eq!(app.typo.font_family, FontFamily::School);
        app.handle_event(InputEvent::Press(Button::Confirm), &mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent); // FontFamily::Serif
        app.handle_event(InputEvent::Press(Button::Right), &mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent); // Size row
        app.handle_event(InputEvent::Press(Button::Confirm), &mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent); // Large
        assert_eq!(app.typo.font_family, FontFamily::Serif);
        assert_eq!(app.typo.font_size, FontSizePreset::Large);
        assert!(app.active_pages.len() >= init_pages, "Larger font must re-paginate to more pages");

        // Down twice to Bookmark row (row 3), press Confirm to bookmark
        app.handle_event(InputEvent::Press(Button::Down), &mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
        app.handle_event(InputEvent::Press(Button::Down), &mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
        app.handle_event(InputEvent::Press(Button::Confirm), &mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
        assert!(app.catalog[0].bookmarked_block.is_some());

        // Close HUD, go Home -> open VVDrop via Home ledger -> simulate incoming file upload -> auto-ingest -> Back stops Wi-Fi
        app.handle_event(InputEvent::Press(Button::Back), &mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
        app.handle_event(InputEvent::Press(Button::Back), &mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
        assert_eq!(app.screen, Screen::Home);
        app.home_cursor = 4; // VVDrop (Row 3 left)
        app.handle_event(InputEvent::Press(Button::Confirm), &mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
        assert_eq!(app.screen, Screen::VvDrop);
        assert!(portal.active, "Wi-Fi portal must start when entering VVDrop");
        portal.pending = Some(("poems.md".into(), b"# Poems\n\nWinter evening.".to_vec()));
        sensors.uptime = 500;
        app.tick(&mut sink, &mut storage, &mut sensors, &mut power, &mut portal);
        assert_eq!(app.catalog.len(), 2);
        assert!(portal.active, "Wi-Fi stays open for batch drops until Back or 180s timeout");
        app.handle_event(InputEvent::Press(Button::Back), &mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
        assert!(!portal.active, "Wi-Fi must shut off immediately when leaving VVDrop");
    }

    #[test]
    fn unified_chin_and_shoulder_navigation_across_screens() {
        let mut storage = MemStorage::new();
        storage.write_atomic("/books/a.txt", b"Alpha chapter content.").unwrap();
        let mut sink = MockSink::new();
        let mut sensors = MockSensors { uptime: 100 };
        let mut power = MockPower { deep_sleeps: 0, hv_standby: false, sd_save: false, imu: false };
        let mut portal = MockPortal { active: false, pending: None };
        let mut ent = MockEntropy(9);

        let mut app = FolioApp::new();
        app.boot(&mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
        assert_eq!(app.home_cursor, 0);
        // Right/Down both advance cursor 0 -> 1 -> 2, Up/Left return cleanly
        app.handle_event(InputEvent::Press(Button::Right), &mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
        assert_eq!(app.home_cursor, 1);
        app.handle_event(InputEvent::Press(Button::Down), &mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
        assert_eq!(app.home_cursor, 2);
        app.handle_event(InputEvent::Press(Button::Up), &mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
        assert_eq!(app.home_cursor, 1);
        app.handle_event(InputEvent::Press(Button::Back), &mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
        assert_eq!(app.home_cursor, 0);

        // Step backwards wraps to Settings (5), Confirm opens Settings
        app.handle_event(InputEvent::Press(Button::Left), &mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
        assert_eq!(app.home_cursor, 5);
        app.handle_event(InputEvent::Press(Button::Confirm), &mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
        assert_eq!(app.screen, Screen::Settings);
        // Left/Right move cursor cleanly in Settings
        app.handle_event(InputEvent::Press(Button::Right), &mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
        assert_eq!(app.settings_cursor, 1);
        app.handle_event(InputEvent::Press(Button::Left), &mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
        assert_eq!(app.settings_cursor, 0);
        app.handle_event(InputEvent::Press(Button::Confirm), &mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
        assert_eq!(app.lang, UiLang::Ru);
    }

    #[test]
    fn recursive_sd_subfolder_discovery_and_lazy_large_book_open() {
        let mut storage = MemStorage::new();
        storage.write_all("/root_notes.txt", b"Root Sketch\n\nMinimalist paper composition at sunrise.").unwrap();
        storage.write_all("/Books/essay.md", b"# Typography\n\nMargins hold silence.").unwrap();

        let mut big_fb2 = String::from(
            r#"<?xml version="1.0" encoding="utf-8"?><FictionBook><description><title-info><author><first-name>Иван</first-name><last-name>Бунин</last-name></author><book-title>Антоновские яблоки</book-title><lang>ru</lang></title-info></description><body><section><title><p>I</p></title>"#,
        );
        for i in 0..140 {
            big_fb2.push_str(&format!("<p>Абзац {i}. Помню раннее, свежее, тихое утро и сад весь в росе.</p>"));
        }
        big_fb2.push_str("</section></body></FictionBook>");
        storage.write_all("/Calibre/Bunin/apples.fb2", big_fb2.as_bytes()).unwrap();

        let mut sink = MockSink::new();
        let mut sensors = MockSensors { uptime: 100 };
        let mut power = MockPower { deep_sleeps: 0, hv_standby: false, sd_save: false, imu: false };
        let mut portal = MockPortal { active: false, pending: None };
        let mut ent = MockEntropy(5);

        let mut app = FolioApp::new();
        app.boot(&mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
        assert_eq!(app.catalog.len(), 3, "Must discover root, /Books, and nested /Calibre/Bunin folders");

        let bunin_idx = app
            .catalog
            .iter()
            .position(|c| c.title == "Антоновские яблоки")
            .expect("Nested FB2 book must be discovered");
        let bunin_vvb = app.catalog[bunin_idx].vvb_path();
        assert!(!storage.exists(&bunin_vvb), "Lazy scan must not convert large nested book during boot");

        app.active_book_idx = bunin_idx;
        assert!(app.open_active_book(&mut storage, &mut power));
        assert!(app.active_direct.is_some(), "Zero-write DirectBook must open nested .fb2 cleanly");
        assert!(!storage.exists(&bunin_vvb), "DirectBook must not write .vvb during open");
        assert!(app.active_pages.len() >= 4);
    }

    #[test]
    fn games_and_reader_survive_deep_sleep_and_idle_power_off() {
        let mut storage = MemStorage::new();
        let mut big = String::from("Chapter One\n\n");
        for i in 0..80 {
            big.push_str(&format!("Paragraph {i}. Calm evening over quiet water and ink on cotton paper. "));
        }
        storage.write_atomic("/books/story.txt", big.as_bytes()).unwrap();
        let mut sink = MockSink::new();
        let mut sensors = MockSensors { uptime: 100 };
        let mut power = MockPower { deep_sleeps: 0, hv_standby: false, sd_save: false, imu: false };
        let mut portal = MockPortal { active: false, pending: None };
        let mut ent = MockEntropy(77);

        let mut app = FolioApp::new();
        app.boot(&mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);

        // 1. Play 1. e4 against Chess AI, then sleep with Power button -> reboot directly into Chess
        app.home_cursor = 2;
        app.handle_event(InputEvent::Press(Button::Confirm), &mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
        assert_eq!(app.screen, Screen::Chess);
        for b in [Button::Down, Button::Confirm, Button::Confirm, Button::Up, Button::Up, Button::Confirm] {
            sensors.uptime += 50;
            app.handle_event(InputEvent::Press(b), &mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
        }
        let saved_hash = app.chess.pos.hash;
        let saved_hist_len = app.chess.history.len();
        assert_eq!(saved_hist_len, 2);
        app.handle_event(InputEvent::Press(Button::Power), &mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);

        let mut app2 = FolioApp::new();
        app2.boot(&mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
        assert_eq!(app2.screen, Screen::Chess);
        assert_eq!(app2.chess.pos.hash, saved_hash);
        assert_eq!(app2.chess.history.len(), saved_hist_len);

        // Undo in resumed Chess still works cleanly
        app2.handle_event(InputEvent::Press(Button::Back), &mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
        assert_eq!(app2.chess.history.len(), 0);

        // 2. Exit Chess -> open Pocket Pip Meadow -> move Pip & push acorn -> idle flush -> cold reboot (no Power press)
        for _ in 0..3 {
            if app2.screen == Screen::Home {
                break;
            }
            app2.handle_event(InputEvent::Press(Button::Back), &mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
        }
        assert_eq!(app2.screen, Screen::Home);
        app2.home_cursor = 3;
        app2.handle_event(InputEvent::Press(Button::Confirm), &mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
        assert_eq!(app2.screen, Screen::PocketPip);
        for b in [Button::Down, Button::Down, Button::Confirm, Button::Right, Button::Right] {
            sensors.uptime += 50;
            app2.handle_event(InputEvent::Press(b), &mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
        }
        assert_eq!(app2.pip.tab, vvbook_games::PipTab::Meadow);
        let pip_pos = app2.pip.meadow.pip;
        let acorns_pos = app2.pip.meadow.acorns.clone();
        let undo_len = app2.pip.undo_stack.len();
        assert_eq!(undo_len, 2);
        sensors.uptime += 2_000;
        app2.tick(&mut sink, &mut storage, &mut sensors, &mut power, &mut portal);

        let mut app3 = FolioApp::new();
        app3.boot(&mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
        assert_eq!(app3.screen, Screen::PocketPip);
        assert_eq!(app3.pip.tab, vvbook_games::PipTab::Meadow);
        assert_eq!(app3.pip.meadow.pip, pip_pos);
        assert_eq!(app3.pip.meadow.acorns, acorns_pos);
        assert_eq!(app3.pip.undo_stack.len(), undo_len);

        // 3. Return to Home -> open Reader -> turn page -> sleep -> cold reboot resumes Reader page
        app3.handle_event(InputEvent::Press(Button::Back), &mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
        app3.handle_event(InputEvent::Press(Button::Back), &mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
        app3.handle_event(InputEvent::Press(Button::Back), &mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
        app3.handle_event(InputEvent::Press(Button::Back), &mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
        assert_eq!(app3.screen, Screen::Home);
        app3.home_cursor = 0;
        app3.handle_event(InputEvent::Press(Button::Confirm), &mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
        assert_eq!(app3.screen, Screen::Reader);
        app3.handle_event(InputEvent::Press(Button::Right), &mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
        let page_after = app3.current_page_num;
        assert_eq!(page_after, 1);
        app3.handle_event(InputEvent::Press(Button::Power), &mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);

        let mut app4 = FolioApp::new();
        app4.boot(&mut sink, &mut storage, &mut sensors, &mut power, &mut portal, &mut ent);
        assert_eq!(app4.screen, Screen::Reader);
        assert_eq!(app4.current_page_num, page_after);
    }
}
