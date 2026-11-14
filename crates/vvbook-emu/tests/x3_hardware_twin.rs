use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::UnsafeCell;
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use vvbook_emu::x3_twin::{
    X3HardwareTwin, build_adversarial_sd_image, build_fat32_sd_image_from_slice,
};
use vvbook_emu::build_zip_bytes;
use vvbook_format::{
    CatalogEntry, DirectBook, InputFormat, ParseError, ZipInflateScratch,
    catalog_format_version, encode_catalog, probe_book_storage,
};
use vvbook_games::{ChessMode, PipTab};
use vvbook_pal::{
    BatteryStatus, Button, DateTime, DeviceSensors, DirEntry, DisplaySink, EntropySource,
    InputEvent, NetworkPortal, PORTRAIT_W, PortalStatus, PowerControl, RefreshMode,
    StorageBackend, StorageError,
};
use vvbook_security::{compute_auth_mac, decode_hex_32, encode_hex};
use vvbook_typeset::{ReaderTypography, paginate_blocks_slice};
use vvbook_userspace::{
    FolioApp, LIBRARY_PER_PAGE, ReaderOverlay, Screen, is_valid_library_book_path,
};
use vvbook_x3_hal::WakePowerGuard;

static ESP32_TEST_LOCK: Mutex<()> = Mutex::new(());

const HEAP_BYTES: usize = 208 * 1024;
const HDR_SIZE: usize = 16;

#[repr(C, align(16))]
struct AlignedArena([u8; HEAP_BYTES]);

#[repr(C)]
struct FreeNode {
    size: usize,
    next: *mut FreeNode,
}

#[derive(Clone, Copy, Debug)]
struct OomRecord {
    req_size: usize,
    req_align: usize,
    free_total: usize,
    largest_contig: usize,
    live_bytes: usize,
}

const EMPTY_OOM: OomRecord = OomRecord {
    req_size: 0,
    req_align: 0,
    free_total: 0,
    largest_contig: 0,
    live_bytes: 0,
};

/// Exact host mirror of `firmware/vvbook-x3-fw/src/main.rs` 320 KB `CoalescingAllocator`,
/// scoped via `Esp32HeapGuard` so pre-test SD image construction stays in host `System` RAM.
struct ScopedEsp32Allocator {
    arena: UnsafeCell<AlignedArena>,
    head: UnsafeCell<*mut FreeNode>,
    initialized: UnsafeCell<bool>,
    live_bytes: UnsafeCell<usize>,
    peak_live_bytes: UnsafeCell<usize>,
    lock: AtomicBool,
    enabled: AtomicBool,
    oom_count: AtomicUsize,
    oom_records: UnsafeCell<[OomRecord; 8]>,
}

unsafe impl Sync for ScopedEsp32Allocator {}

#[inline]
const fn align_up(v: usize, align: usize) -> usize {
    (v + align - 1) & !(align - 1)
}

impl ScopedEsp32Allocator {
    const fn new() -> Self {
        Self {
            arena: UnsafeCell::new(AlignedArena([0u8; HEAP_BYTES])),
            head: UnsafeCell::new(ptr::null_mut()),
            initialized: UnsafeCell::new(false),
            live_bytes: UnsafeCell::new(0),
            peak_live_bytes: UnsafeCell::new(0),
            lock: AtomicBool::new(false),
            enabled: AtomicBool::new(false),
            oom_count: AtomicUsize::new(0),
            oom_records: UnsafeCell::new([EMPTY_OOM; 8]),
        }
    }

    #[inline]
    fn acquire(&self) {
        while self
            .lock
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
    }

    #[inline]
    fn release(&self) {
        self.lock.store(false, Ordering::Release);
    }

    #[inline]
    fn arena_bounds(&self) -> (usize, usize) {
        let base = self.arena.get() as usize;
        (base, base + HEAP_BYTES)
    }

    #[inline]
    fn contains(&self, ptr_user: *const u8) -> bool {
        let addr = ptr_user as usize;
        let (start, end) = self.arena_bounds();
        addr >= start && addr < end
    }

    unsafe fn ensure_init(&self) -> *mut FreeNode {
        unsafe {
            if !*self.initialized.get() {
                let base = (*self.arena.get()).0.as_mut_ptr() as *mut FreeNode;
                (*base).size = HEAP_BYTES;
                (*base).next = ptr::null_mut();
                *self.head.get() = base;
                *self.live_bytes.get() = 0;
                *self.peak_live_bytes.get() = 0;
                *self.initialized.get() = true;
            }
            *self.head.get()
        }
    }

    unsafe fn free_stats_locked(&self) -> (usize, usize, usize, usize) {
        unsafe {
            let mut total_free = 0usize;
            let mut largest = 0usize;
            let mut cur = *self.head.get();
            while !cur.is_null() {
                let sz = (*cur).size;
                total_free += sz;
                if sz > largest {
                    largest = sz;
                }
                cur = (*cur).next;
            }
            (
                total_free,
                largest,
                *self.live_bytes.get(),
                *self.peak_live_bytes.get(),
            )
        }
    }

    unsafe fn alloc_in_arena_locked(&self, layout: Layout) -> *mut u8 {
        unsafe {
            let align = layout.align().max(16);
            let need = align_up(layout.size().max(16), 16);

            let mut head = self.ensure_init();
            let mut prev: *mut FreeNode = ptr::null_mut();
            let mut cur = head;

            while !cur.is_null() {
                let cur_size = (*cur).size;
                let base_addr = cur as usize;
                let user_addr = align_up(base_addr + HDR_SIZE, align);
                let hdr_addr = user_addr - HDR_SIZE;
                let end_addr = user_addr + need;
                let needed_from_base = end_addr - base_addr;

                if cur_size >= needed_from_base {
                    let rem = cur_size - needed_from_base;
                    let next_cur = (*cur).next;
                    let replacement = if rem >= 32 {
                        let tail = end_addr as *mut FreeNode;
                        (*tail).size = rem;
                        (*tail).next = next_cur;
                        tail
                    } else {
                        next_cur
                    };
                    if prev.is_null() {
                        head = replacement;
                        *self.head.get() = head;
                    } else {
                        (*prev).next = replacement;
                    }
                    let actual_block = if rem >= 32 { needed_from_base } else { cur_size };
                    let prefix_pad = hdr_addr - base_addr;
                    let meta_ptr = hdr_addr as *mut usize;
                    *meta_ptr = actual_block - prefix_pad;
                    *meta_ptr.add(1) = base_addr;

                    let new_live = *self.live_bytes.get() + actual_block;
                    *self.live_bytes.get() = new_live;
                    if new_live > *self.peak_live_bytes.get() {
                        *self.peak_live_bytes.get() = new_live;
                    }
                    return user_addr as *mut u8;
                }
                prev = cur;
                cur = (*cur).next;
            }

            let (free_total, largest_contig, live_bytes, _) = self.free_stats_locked();
            let idx = self.oom_count.fetch_add(1, Ordering::Relaxed);
            if idx < 8 {
                (*self.oom_records.get())[idx] = OomRecord {
                    req_size: layout.size(),
                    req_align: layout.align(),
                    free_total,
                    largest_contig,
                    live_bytes,
                };
            }
            ptr::null_mut()
        }
    }

    unsafe fn dealloc_in_arena_locked(&self, ptr_user: *mut u8) {
        unsafe {
            let hdr = ptr_user.sub(HDR_SIZE) as *mut usize;
            let payload_span = *hdr;
            let base_addr = *hdr.add(1);
            let full_size = ((ptr_user as usize - HDR_SIZE) - base_addr) + payload_span;
            *self.live_bytes.get() = (*self.live_bytes.get()).saturating_sub(full_size);

            let node = base_addr as *mut FreeNode;
            let mut cur = *self.head.get();
            let mut prev: *mut FreeNode = ptr::null_mut();

            while !cur.is_null() && (cur as usize) < base_addr {
                prev = cur;
                cur = (*cur).next;
            }

            (*node).size = full_size;
            (*node).next = cur;
            if prev.is_null() {
                *self.head.get() = node;
            } else {
                (*prev).next = node;
            }

            // Coalesce right neighbor
            if !cur.is_null() && base_addr + full_size == cur as usize {
                (*node).size += (*cur).size;
                (*node).next = (*cur).next;
            }
            // Coalesce left neighbor
            if !prev.is_null() && (prev as usize) + (*prev).size == node as usize {
                (*prev).size += (*node).size;
                (*prev).next = (*node).next;
            }
        }
    }

    fn stats(&self) -> (usize, usize, usize, usize) {
        self.acquire();
        let st = unsafe {
            let _ = self.ensure_init();
            self.free_stats_locked()
        };
        self.release();
        st
    }

    fn reset_and_enable(&self) {
        self.acquire();
        unsafe {
            *self.initialized.get() = false;
            let _ = self.ensure_init();
        }
        self.oom_count.store(0, Ordering::SeqCst);
        self.release();
        self.enabled.store(true, Ordering::SeqCst);
    }

    fn disable(&self) {
        self.enabled.store(false, Ordering::SeqCst);
    }
}

fn with_host_alloc<R>(f: impl FnOnce() -> R) -> R {
    let was_enabled = ESP32_ALLOC.enabled.swap(false, Ordering::SeqCst);
    let out = f();
    if was_enabled {
        ESP32_ALLOC.enabled.store(true, Ordering::SeqCst);
    }
    out
}

unsafe impl GlobalAlloc for ScopedEsp32Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if self.enabled.load(Ordering::Acquire) {
            self.acquire();
            let p = unsafe { self.alloc_in_arena_locked(layout) };
            self.release();
            if !p.is_null() {
                return p;
            }
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr_user: *mut u8, layout: Layout) {
        if ptr_user.is_null() {
            return;
        }
        if self.contains(ptr_user) {
            self.acquire();
            unsafe { self.dealloc_in_arena_locked(ptr_user) };
            self.release();
        } else {
            unsafe { System.dealloc(ptr_user, layout) };
        }
    }

    unsafe fn realloc(&self, ptr_user: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if !self.enabled.load(Ordering::Acquire) && !self.contains(ptr_user) {
            return unsafe { System.realloc(ptr_user, layout, new_size) };
        }
        let Ok(new_layout) = Layout::from_size_align(new_size, layout.align()) else {
            return ptr::null_mut();
        };
        let new_ptr = unsafe { self.alloc(new_layout) };
        if new_ptr.is_null() {
            return ptr::null_mut();
        }
        if !ptr_user.is_null() {
            unsafe {
                ptr::copy_nonoverlapping(ptr_user, new_ptr, layout.size().min(new_size));
                self.dealloc(ptr_user, layout);
            }
        }
        new_ptr
    }
}

#[global_allocator]
static ESP32_ALLOC: ScopedEsp32Allocator = ScopedEsp32Allocator::new();

struct Esp32HeapGuard;

impl Esp32HeapGuard {
    fn enable() -> Self {
        ESP32_ALLOC.reset_and_enable();
        let (free, largest, live, _) = ESP32_ALLOC.stats();
        assert_eq!(live, 0);
        assert_eq!(free, HEAP_BYTES);
        assert_eq!(largest, HEAP_BYTES);
        Self
    }

    fn assert_no_oom(&self, milestone: &str) {
        let count = ESP32_ALLOC.oom_count.load(Ordering::SeqCst);
        if count > 0 {
            ESP32_ALLOC.disable();
            ESP32_ALLOC.acquire();
            let first = unsafe { (*ESP32_ALLOC.oom_records.get())[0] };
            ESP32_ALLOC.release();
            panic!(
                "ESP32-C3 {} KB SRAM OOM at milestone '{milestone}': count={count}, first_req={}B (align {}), free_total={}B, largest_contig={}B, live={}B",
                HEAP_BYTES / 1024,
                first.req_size,
                first.req_align,
                first.free_total,
                first.largest_contig,
                first.live_bytes
            );
        }
    }

    fn verify_clean_and_disable(self) -> (usize, usize) {
        self.assert_no_oom("teardown");
        ESP32_ALLOC.disable();
        ESP32_ALLOC.stats();
        let (free, largest, live, peak) = ESP32_ALLOC.stats();
        assert_eq!(
            live,
            0,
            "leaked {live} bytes inside {} KB arena on X3HardwareTwin teardown",
            HEAP_BYTES / 1024
        );
        assert_eq!(
            free, HEAP_BYTES,
            "free arena mismatch after drop: free={free}, expected={HEAP_BYTES}"
        );
        assert_eq!(
            largest, HEAP_BYTES,
            "free list failed to coalesce fully: largest_contig={largest}, expected={HEAP_BYTES}"
        );
        std::mem::forget(self);
        (peak, HEAP_BYTES)
    }
}

impl Drop for Esp32HeapGuard {
    fn drop(&mut self) {
        ESP32_ALLOC.disable();
    }
}

#[test]
fn x3_twin_56_books_nested_boot_unplug_multibook_open_and_budgets() {
    let _test_guard = ESP32_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Allocate simulated 64 MiB flash array on host RAM BEFORE arming the 320 KB SRAM cap.
    let mut sd_img = build_adversarial_sd_image();
    let guard = Esp32HeapGuard::enable();

    {
        let mut twin = X3HardwareTwin::from_sd_image(sd_img);
        twin.boot();
        guard.assert_no_oom("boot");

        assert_eq!(
            twin.app.catalog.len(),
            56,
            "expected all 56 nested microSD books discovered on boot"
        );
        assert!(
            twin.count_dark_panel_pixels() > 2_000,
            "Home screen should draw visible dark ink pixels on UC8253 panel"
        );
        assert_eq!(twin.hw_stats().sd_rail_faults, 0);
        assert_eq!(twin.hw_stats().oversize_read_all_rejections, 0);

        // 1. USB-Serial/JTAG unplug & replug EP1 64-byte FIFO resilience test
        twin.set_usb_cable(false);
        twin.open_home_item(1); // Library
        for _ in 0..4 {
            twin.press_button(Button::Down);
        }
        let unplug = twin.usb_stats();
        assert_eq!(
            unplug.timeout_stalls, 1,
            "firmware must hit the 1.5ms USB EP1 stall at most once when unplugged"
        );
        assert!(unplug.dropped_bytes > 0);
        assert!(!unplug.host_online);

        twin.set_usb_cable(true);
        twin.press_button(Button::Up);
        let replug = twin.usb_stats();
        assert!(replug.host_online);
        assert!(replug.transmitted_bytes > unplug.transmitted_bytes);
        guard.assert_no_oom("usb_unplug_replug");

        // 2. Cold open + prefetched page turns + warm reopen across all 5 flagship adversarial books
        let targets = [
            "war_and_svg_1100kb.epub",
            "tolstoy_adversarial_1100kb.fb2.zip",
            "chronicles_6000_blocks.md",
            "chekhov_cp1251_full.txt",
            "BAUHAUS_MANIFESTO.HTML",
        ];
        for suffix in targets {
            let idx = twin
                .app
                .catalog
                .iter()
                .position(|c| c.source_path.ends_with(suffix))
                .unwrap_or_else(|| panic!("missing book suffix {suffix} in catalog"));

            twin.select_library_index(idx);
            let frames_before = twin.frame_count();
            let sd_us_before = twin.hw_stats().spi_sd_us;
            twin.press_button(Button::Confirm);
            let cold_sd_ms = (twin.hw_stats().spi_sd_us - sd_us_before) / 1_000;

            assert_eq!(twin.app.screen, Screen::Reader, "failed to open {suffix}");
            assert!(
                twin.frame_count() >= frames_before + 2,
                "expected FastDu opening placard + Reader frame for {suffix}"
            );
            assert_eq!(
                twin.frame_mode_at(frames_before),
                Some(RefreshMode::FastDu),
                "cold open must present FastDu opening placard first for {suffix}"
            );
            let cold_budget_ms = 800;
            assert!(
                cold_sd_ms < cold_budget_ms,
                "cold open SPI-SD latency for {suffix} was {cold_sd_ms} ms (>= {cold_budget_ms} ms)"
            );
            assert!(
                twin.count_dark_panel_pixels() > 1_000,
                "Reader screen blank after opening {suffix}"
            );

            // Flush any deferred persistence from prior navigation before measuring zero-SD turns.
            if twin.app.pending_journal_turns > 0 {
                twin.app.persistence_dirty = true;
            }
            twin.advance_idle_ms(1_600);

            // Next 5 consecutive page turns inside prefetch window -> ZERO SD sector reads
            let turns = twin.prefetched_turn_budget(5);
            assert_eq!(
                turns, 5,
                "expected at least 5 pages of {suffix} inside 16-block prefetch window"
            );
            let sec0 = twin.hw_stats().sd_sectors_read;
            for _ in 0..turns {
                twin.press_button(Button::Right);
            }
            let sec_delta = twin.hw_stats().sd_sectors_read - sec0;
            assert_eq!(
                sec_delta, 0,
                "prefetched page turns for {suffix} triggered {sec_delta} SD sector reads"
            );

            // Warm cached reopen < 80 ms SPI-SD time
            twin.press_button(Button::Back); // Back to Home
            twin.open_home_item(1); // Library
            let warm_us_before = twin.hw_stats().spi_sd_us;
            twin.open_library_index(idx);
            let warm_sd_ms = (twin.hw_stats().spi_sd_us - warm_us_before) / 1_000;
            assert!(
                warm_sd_ms < 80,
                "warm cached reopen SPI-SD latency for {suffix} was {warm_sd_ms} ms (>= 80 ms)"
            );
            if suffix == "war_and_svg_1100kb.epub" {
                let direct_ref = twin
                    .app
                    .active_direct
                    .as_ref()
                    .expect("1.2 MB EPUB must open in zero-write DirectBook mode");
                assert!(
                    direct_ref.section_count() >= 650,
                    "expected >= 650 EPUB sections, got {}",
                    direct_ref.section_count()
                );
                assert!(
                    twin.app.active_pages.len() >= 6,
                    "expected 3,200-word <br/> mega-paragraph in Chapter 1 to span >= 6 pages, got {}",
                    twin.app.active_pages.len()
                );
                // Jump via AaHud -> TocDrawer to the final chapter (>= 649) inside 320 KB heap
                twin.press_button(Button::Confirm);
                for _ in 0..4 {
                    twin.press_button(Button::Down);
                }
                twin.press_button(Button::Confirm);
                assert_eq!(twin.app.reader_overlay, ReaderOverlay::TocDrawer);
                twin.press_button(Button::Up);
                twin.press_button(Button::Confirm);
                assert_eq!(twin.app.reader_overlay, ReaderOverlay::None);
                assert!(
                    twin.app.direct_section_idx >= 649,
                    "expected final-chapter TOC jump >= 649, got {}",
                    twin.app.direct_section_idx
                );
                assert!(
                    twin.app.display_page_num() > 50,
                    "expected late-chapter TOC jump display_page_num > 50, got {}",
                    twin.app.display_page_num()
                );
            }
            guard.assert_no_oom(suffix);
        }

        // 3. `Aa` typography HUD repagination inside 320 KB heap
        twin.press_button(Button::Confirm);
        assert_eq!(twin.app.reader_overlay, ReaderOverlay::AaHud);
        twin.press_button(Button::Down); // Font Size row
        twin.press_button(Button::Confirm); // Repaginate active volume
        twin.press_button(Button::Back);
        assert_eq!(twin.app.reader_overlay, ReaderOverlay::None);
        guard.assert_no_oom("aa_repagination");

        // 4. Grandmaster Chess AI interactive turn (1. e4 -> Black AI responds)
        twin.open_home_item(2);
        assert_eq!(twin.app.screen, Screen::Chess);
        twin.press_button(Button::Down); // Play vs AI
        twin.press_button(Button::Confirm);
        twin.press_button(Button::Confirm); // Select e2
        twin.press_button(Button::Up);
        twin.press_button(Button::Up);
        twin.press_button(Button::Confirm); // Move e2-e4 -> AI plays response
        assert_eq!(twin.app.chess.history.len(), 2);
        twin.ensure_home();
        guard.assert_no_oom("chess_ai");

        // 5. Pocket Pip & Settings EN/RU toggle
        twin.open_home_item(3);
        assert_eq!(twin.app.screen, Screen::PocketPip);
        twin.press_button(Button::Confirm);
        twin.press_button(Button::Right);
        twin.ensure_home();
        guard.assert_no_oom("pocket_pip");

        twin.open_home_item(5);
        assert_eq!(twin.app.screen, Screen::Settings);
        twin.press_button(Button::Confirm);
        assert!(twin.app.lang.is_ru());
        twin.press_button(Button::Confirm);
        assert!(!twin.app.lang.is_ru());
        twin.ensure_home();
        guard.assert_no_oom("settings_en_ru");

        // 6. Stage 1 HV standby (2.5s) -> Stage 3 deep sleep + SleepPoster -> wake
        twin.advance_idle_ms(3_000);
        assert!(twin.hw_stats().panel_hv_standby);

        twin.advance_idle_ms(605_000);
        assert_eq!(twin.app.screen, Screen::SleepPoster);
        assert!(twin.hw_stats().deep_sleeping);
        assert!(!twin.hw_stats().sd_power_en);

        twin.press_button(Button::Confirm);
        assert_eq!(twin.app.screen, Screen::Home);
        assert!(!twin.hw_stats().deep_sleeping);
        assert!(twin.hw_stats().sd_power_en);

        let hw = twin.hw_stats();
        assert_eq!(hw.sd_rail_faults, 0);
        assert_eq!(hw.oversize_read_all_rejections, 0);

        // Return prebuilt 64 MiB flash vector outside arena drop check
        sd_img = twin.into_sd_image();
    }

    let (peak_bytes, heap_cap) = guard.verify_clean_and_disable();
    assert!(
        peak_bytes <= heap_cap,
        "peak SRAM {peak_bytes} exceeded {heap_cap}"
    );
    drop(sd_img);
}

fn is_drm_encrypted_mobi_host(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    if !matches!(ext.as_str(), "mobi" | "prc" | "azw" | "azw3") {
        return false;
    }
    with_host_alloc(|| {
        let Ok(mut f) = File::open(path) else {
            return false;
        };
        let mut hdr = [0u8; 84];
        if f.read_exact(&mut hdr).is_err() {
            return false;
        }
        let ident = &hdr[60..68];
        if ident != b"BOOKMOBI" && ident != b"TEXtREAd" {
            return false;
        }
        let num_records = u16::from_be_bytes([hdr[76], hdr[77]]);
        if num_records == 0 {
            return false;
        }
        let rec0_off = u32::from_be_bytes([hdr[78], hdr[79], hdr[80], hdr[81]]) as u64;
        if f.seek(SeekFrom::Start(rec0_off)).is_err() {
            return false;
        }
        let mut r0 = [0u8; 16];
        if f.read_exact(&mut r0).is_err() {
            return false;
        }
        u16::from_be_bytes([r0[12], r0[13]]) != 0
    })
}

struct ReadOnlyHostTreeStorage {
    files: BTreeMap<String, PathBuf>,
}

impl ReadOnlyHostTreeStorage {
    fn collect_dir(root: &Path, cur: &Path, map: &mut BTreeMap<String, PathBuf>) {
        let Ok(rd) = fs::read_dir(cur) else {
            return;
        };
        for ent in rd.flatten() {
            let p = ent.path();
            let name = ent.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            if p.is_dir() {
                Self::collect_dir(root, &p, map);
            } else if InputFormat::from_filename(&name).is_some() {
                let rel = p
                    .strip_prefix(root)
                    .unwrap_or(&p)
                    .to_string_lossy()
                    .replace('\\', "/");
                map.insert(format!("/{rel}"), p);
            }
        }
    }

    fn from_root(root: &Path) -> Self {
        let mut files = BTreeMap::new();
        Self::collect_dir(root, root, &mut files);
        Self { files }
    }
}

impl StorageBackend for ReadOnlyHostTreeStorage {
    fn exists(&self, path: &str) -> bool {
        self.files.contains_key(path)
    }

    fn file_len(&self, path: &str) -> Result<u64, StorageError> {
        let p = self.files.get(path).ok_or(StorageError::NotFound)?;
        with_host_alloc(|| fs::metadata(p).map(|m| m.len()).map_err(|_| StorageError::Io))
    }

    fn read_all(&self, path: &str) -> Result<Vec<u8>, StorageError> {
        let len = self.file_len(path)?;
        if len > 256 * 1024 {
            return Err(StorageError::Io);
        }
        let mut buf = vec![0u8; len as usize];
        let n = self.read_range(path, 0, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    fn read_range(&self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize, StorageError> {
        let p = self.files.get(path).ok_or(StorageError::NotFound)?;
        with_host_alloc(|| {
            let mut f = File::open(p).map_err(|_| StorageError::Io)?;
            f.seek(SeekFrom::Start(offset)).map_err(|_| StorageError::Io)?;
            let mut total = 0usize;
            while total < buf.len() {
                match f.read(&mut buf[total..]) {
                    Ok(0) => break,
                    Ok(n) => total += n,
                    Err(_) => return Err(StorageError::Io),
                }
            }
            Ok(total)
        })
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

#[test]
fn real_home_books_library_and_largest_55mb_68mb_open_within_320kb_sram() {
    let _test_guard = ESP32_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = match std::env::var_os("HOME") {
        Some(h) => PathBuf::from(h).join("books"),
        None => return,
    };
    if !home.is_dir() {
        eprintln!("Skipping ~/books real-library test ({} not found)", home.display());
        return;
    }

    assert!(!is_valid_library_book_path(
        "/Unknown/My Clippings (160)/My Clippings - Unknown.txt"
    ));

    // Part A: Probe, open via DirectBook, decode first readable section, and paginate EVERY valid
    // DRM-free book in ~/books inside real ESP32-C3 SRAM (and verify DRM files return UnsupportedFormat).
    let store = ReadOnlyHostTreeStorage::from_root(&home);
    assert!(
        store.files.len() >= 200,
        "expected >= 200 books in {}, found {}",
        home.display(),
        store.files.len()
    );
    let paths: Vec<(String, PathBuf, u64)> = store
        .files
        .iter()
        .map(|(rel, p)| {
            (
                rel.clone(),
                p.clone(),
                fs::metadata(p).map(|m| m.len()).unwrap_or(0),
            )
        })
        .collect();

    {
        let guard = Esp32HeapGuard::enable();
        let typo = ReaderTypography::default();
        let mut epub_scratch = ZipInflateScratch::new();
        let mut ok_count = 0usize;
        let mut drm_rejected_count = 0usize;

        for (rel, host_path, sz) in &paths {
            if !is_valid_library_book_path(rel) {
                continue;
            }
            if is_drm_encrypted_mobi_host(host_path) {
                assert!(
                    matches!(probe_book_storage(&store, rel, *sz), Err(ParseError::UnsupportedFormat)),
                    "expected UnsupportedFormat from probe_book_storage on DRM book {rel}"
                );
                assert!(
                    matches!(DirectBook::open(&store, rel, *sz, None), Err(ParseError::UnsupportedFormat)),
                    "expected UnsupportedFormat from DirectBook::open on DRM book {rel}"
                );
                guard.assert_no_oom(rel);
                drm_rejected_count += 1;
                continue;
            }
            let meta = probe_book_storage(&store, rel, *sz)
                .unwrap_or_else(|e| panic!("probe failed for {rel}: {e:?}"));
            assert!(!meta.title.is_empty(), "empty title for {rel}");

            let is_epub = rel.to_ascii_lowercase().ends_with(".epub");
            let direct = if is_epub {
                DirectBook::open_with_scratch(&store, rel, *sz, Some(&meta.id_hex), &mut epub_scratch)
            } else {
                DirectBook::open(&store, rel, *sz, Some(&meta.id_hex))
            }
            .unwrap_or_else(|e| panic!("DirectBook::open failed for {rel}: {e:?}"));

            let sec_count = direct.section_count().max(1);
            let mut found_blocks = Vec::new();
            for sec in 0..sec_count.min(16) {
                let blks = if is_epub {
                    direct.load_section_with_scratch(&store, sec, &mut epub_scratch)
                } else {
                    direct.load_section(&store, sec)
                }
                .unwrap_or_else(|e| panic!("load_section({sec}) failed for {rel}: {e:?}"));
                if !blks.is_empty() {
                    found_blocks = blks;
                    break;
                }
            }
            assert!(!found_blocks.is_empty(), "all initial sections empty for {rel}");
            let pages = paginate_blocks_slice(&found_blocks, &typo, 1024);
            assert!(!pages.is_empty(), "pagination yielded 0 pages for {rel}");
            drop(pages);
            drop(found_blocks);
            drop(direct);
            guard.assert_no_oom(rel);
            ok_count += 1;
        }
        drop(epub_scratch);
        let (peak, cap) = guard.verify_clean_and_disable();
        assert!(
            ok_count >= 190,
            "expected >= 190 valid DRM-free books, verified {ok_count}"
        );
        assert_eq!(
            drm_rejected_count, 10,
            "expected 10 DRM-locked files in ~/books, saw {drm_rejected_count}"
        );
        eprintln!(
            "Verified all {ok_count} DRM-free + rejected {drm_rejected_count} DRM books in {} inside {} KB SRAM (peak {peak}/{cap} B)",
            home.display(),
            cap / 1024
        );
    }

    // Part B: Full FAT32 SPI-SD + X3HardwareTwin UI open/turn/TOC-jump on the 3 largest DRM-free books
    // (~55MB EPUB, ~54MB PRC, ~9MB AZW) plus a 64 KB prefix of a real DRM AZW that must stay excluded.
    let mut largest_by_kind: Vec<(&str, PathBuf)> = Vec::new();
    for (label, suffix) in [
        ("/Books/Lurie/st_petersburg.epub", ".epub"),
        ("/Books/Lurie_PRC/st_petersburg.prc", ".prc"),
        ("/Books/Simanovskii/ierofieiev.azw", ".azw"),
    ] {
        if let Some((_, best)) = store
            .files
            .values()
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|s| s.to_ascii_lowercase().ends_with(suffix))
                    && !is_drm_encrypted_mobi_host(p)
            })
            .map(|p| (fs::metadata(p).map(|m| m.len()).unwrap_or(0), p.clone()))
            .filter(|(sz, _)| *sz > 0)
            .max_by_key(|(sz, _)| *sz)
        {
            largest_by_kind.push((label, best));
        }
    }
    assert_eq!(largest_by_kind.len(), 3, "expected largest DRM-free .epub, .prc, .azw");

    let mut loaded_files: Vec<(&str, Vec<u8>)> = largest_by_kind
        .iter()
        .map(|(fat_path, host_path)| (*fat_path, fs::read(host_path).expect("read largest book")))
        .collect();
    if let Some((_, drm_path)) = store
        .files
        .values()
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|s| s.to_ascii_lowercase().ends_with(".azw"))
                && is_drm_encrypted_mobi_host(p)
        })
        .map(|p| (fs::metadata(p).map(|m| m.len()).unwrap_or(0), p.clone()))
        .min_by_key(|(sz, _)| *sz)
    {
        let mut f = File::open(&drm_path).expect("open DRM azw sample");
        let mut prefix = vec![0u8; 64 * 1024];
        let n = f.read(&mut prefix).expect("read DRM azw prefix");
        prefix.truncate(n);
        loaded_files.push(("/Books/Oxford/new_oxford_drm.azw", prefix));
    }

    let sd_refs: Vec<(&str, &[u8])> = loaded_files
        .iter()
        .map(|(p, b)| (*p, b.as_slice()))
        .collect();
    let mut sd_img = build_fat32_sd_image_from_slice(&sd_refs).expect("build large FAT32 SD");
    drop(loaded_files);

    let mut cold_timings = [0u64; 3];
    let guard = Esp32HeapGuard::enable();
    {
        let mut twin = X3HardwareTwin::from_sd_image(sd_img);
        twin.boot();
        assert_eq!(
            twin.app.catalog.len(),
            3,
            "expected only the 3 DRM-free flagship books in catalog (DRM excluded)"
        );

        twin.open_home_item(1);
        for (slot_idx, fat_path) in [
            "/Books/Lurie/st_petersburg.epub",
            "/Books/Lurie_PRC/st_petersburg.prc",
            "/Books/Simanovskii/ierofieiev.azw",
        ]
        .iter()
        .enumerate()
        {
            let idx = twin
                .app
                .catalog
                .iter()
                .position(|c| c.source_path == *fat_path)
                .unwrap_or_else(|| panic!("missing {fat_path} in catalog"));
            twin.select_library_index(idx);
            let sd_us0 = twin.hw_stats().spi_sd_us;
            twin.press_button(Button::Confirm);
            let cold_ms = (twin.hw_stats().spi_sd_us - sd_us0) / 1_000;
            cold_timings[slot_idx] = cold_ms;
            assert_eq!(twin.app.screen, Screen::Reader, "failed opening {fat_path}");
            assert!(twin.app.active_direct.is_some(), "expected DirectBook for {fat_path}");
            assert!(
                cold_ms < 800,
                "cold open SPI-SD latency for {fat_path} was {cold_ms} ms (>= 800 ms)"
            );
            assert!(twin.count_dark_panel_pixels() > 1_000, "blank reader for {fat_path}");

            twin.press_button(Button::Right);
            twin.press_button(Button::Right);
            if twin
                .app
                .active_direct
                .as_ref()
                .is_some_and(|d| d.toc.len() >= 2)
            {
                // Jump via AaHud -> TocDrawer to the final chapter inside real ESP32-C3 SRAM
                twin.press_button(Button::Confirm);
                for _ in 0..4 {
                    twin.press_button(Button::Down);
                }
                twin.press_button(Button::Confirm);
                assert_eq!(twin.app.reader_overlay, ReaderOverlay::TocDrawer);
                twin.press_button(Button::Up);
                twin.press_button(Button::Confirm);
                assert_eq!(twin.app.reader_overlay, ReaderOverlay::None);
                assert_eq!(twin.app.screen, Screen::Reader);
                assert!(twin.count_dark_panel_pixels() > 1_000);
            }

            twin.ensure_home();
            twin.open_home_item(1);
            guard.assert_no_oom(fat_path);
        }

        sd_img = twin.into_sd_image();
    }
    let (peak, cap) = guard.verify_clean_and_disable();
    assert!(peak <= cap, "peak SRAM {peak} exceeded {cap}");
    eprintln!(
        "  FAT32 X3Twin opened 55MB EPUB={}ms, 54MB PRC={}ms, DRM-free AZW={}ms inside {} KB (peak {peak}/{cap} B)",
        cold_timings[0],
        cold_timings[1],
        cold_timings[2],
        cap / 1024
    );
    drop(sd_img);
}

#[test]
fn x3_twin_chess_survives_deep_sleep_cold_reboot() {
    let _test_guard = ESP32_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut sd_img = build_adversarial_sd_image();
    let guard = Esp32HeapGuard::enable();

    let saved_hash: u64;
    let saved_hist_len: usize;
    let saved_moves: Vec<String>;
    {
        let mut twin = X3HardwareTwin::from_sd_image(sd_img);
        twin.boot();
        twin.open_home_item(2);
        assert_eq!(twin.app.screen, Screen::Chess);
        twin.press_button(Button::Down); // Play vs AI
        twin.press_button(Button::Confirm);
        twin.press_button(Button::Confirm); // Select e2
        twin.press_button(Button::Up);
        twin.press_button(Button::Up);
        twin.press_button(Button::Confirm); // Move e2-e4 -> Black AI responds
        assert_eq!(twin.app.chess.history.len(), 2);
        saved_hash = twin.app.chess.pos.hash;
        saved_hist_len = twin.app.chess.history.len();
        saved_moves = twin.app.chess.history.iter().map(|h| h.2.clone()).collect();

        // Press physical Power button -> sleep poster -> deep sleep
        twin.press_button(Button::Power);
        assert_eq!(twin.app.screen, Screen::SleepPoster);
        assert!(twin.hw_stats().deep_sleeping);
        guard.assert_no_oom("chess_deep_sleep");
        sd_img = twin.into_sd_image();
    }

    // Cold boot brand-new X3HardwareTwin from the persisted microSD card
    {
        let mut twin2 = X3HardwareTwin::from_sd_image(sd_img);
        twin2.boot();
        assert_eq!(twin2.app.screen, Screen::Chess);
        assert_eq!(twin2.app.chess.pos.hash, saved_hash);
        assert_eq!(twin2.app.chess.history.len(), saved_hist_len);
        let restored_moves: Vec<String> = twin2.app.chess.history.iter().map(|h| h.2.clone()).collect();
        assert_eq!(restored_moves, saved_moves);
        drop(saved_moves);
        assert!(twin2.count_dark_panel_pixels() > 1_000);

        // Undo works cleanly after cold reboot
        twin2.press_button(Button::Back);
        assert_eq!(twin2.app.chess.history.len(), 0);
        guard.assert_no_oom("chess_cold_reboot");
        sd_img = twin2.into_sd_image();
    }

    let (peak, cap) = guard.verify_clean_and_disable();
    assert!(peak <= cap, "peak SRAM {peak} exceeded {cap}");
    drop(sd_img);
}

#[test]
fn x3_twin_pocket_pip_survives_idle_flush_hard_power_off() {
    let _test_guard = ESP32_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut sd_img = build_adversarial_sd_image();
    let guard = Esp32HeapGuard::enable();

    let saved_pip_pos: u8;
    let saved_acorns: Vec<u8>;
    let saved_undo_len: usize;
    {
        let mut twin = X3HardwareTwin::from_sd_image(sd_img);
        twin.boot();
        twin.open_home_item(3);
        assert_eq!(twin.app.screen, Screen::PocketPip);
        // Enter Starberry Meadow level 0 and make two moves (pushing acorn onto undo stack)
        twin.press_button(Button::Down);
        twin.press_button(Button::Down);
        twin.press_button(Button::Confirm);
        assert_eq!(twin.app.pip.tab, PipTab::Meadow);
        twin.press_button(Button::Right);
        twin.press_button(Button::Right);
        saved_pip_pos = twin.app.pip.meadow.pip;
        saved_acorns = twin.app.pip.meadow.acorns.clone();
        saved_undo_len = twin.app.pip.undo_stack.len();
        assert_eq!(saved_undo_len, 2);

        // Simulate abrupt power cut after 2s idle (no Power button sleep!)
        twin.advance_idle_ms(2_000);
        guard.assert_no_oom("pocket_pip_idle_flush");
        sd_img = twin.into_sd_image();
    }

    {
        let mut twin2 = X3HardwareTwin::from_sd_image(sd_img);
        twin2.boot();
        assert_eq!(twin2.app.screen, Screen::PocketPip);
        assert_eq!(twin2.app.pip.tab, PipTab::Meadow);
        assert_eq!(twin2.app.pip.meadow.pip, saved_pip_pos);
        assert_eq!(twin2.app.pip.meadow.acorns, saved_acorns);
        assert_eq!(twin2.app.pip.undo_stack.len(), saved_undo_len);

        // Undo from resumed Meadow state restores prior step cleanly
        twin2.press_button(Button::Back);
        assert_eq!(twin2.app.pip.undo_stack.len(), 1);
        assert_ne!(twin2.app.pip.meadow.pip, saved_pip_pos);
        guard.assert_no_oom("pocket_pip_hard_reboot");
        sd_img = twin2.into_sd_image();
    }

    let (peak, cap) = guard.verify_clean_and_disable();
    assert!(peak <= cap, "peak SRAM {peak} exceeded {cap}");
    drop(sd_img);
}

#[test]
fn x3_twin_vvdrop_multi_mb_streaming_upload_and_rtc_sync_under_320kb() {
    let _test_guard = ESP32_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Construct a ~1.5 MB valid EPUB on host RAM before entering the 320 KB ESP32-C3 heap ceiling.
    let container = br#"<?xml version="1.0" encoding="utf-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#;
    let mut manifest = String::new();
    let mut spine = String::new();
    let mut chapter_bodies: Vec<(String, Vec<u8>)> = Vec::with_capacity(40);
    for ch in 0..40usize {
        let href = format!("ch{ch:02}.xhtml");
        manifest.push_str(&format!(
            "<item id=\"c{ch}\" href=\"{href}\" media-type=\"application/xhtml+xml\"/>\n"
        ));
        spine.push_str(&format!("<itemref idref=\"c{ch}\"/>\n"));
        let mut xhtml = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?><html xmlns=\"http://www.w3.org/1999/xhtml\"><body><h1>Chapter {}</h1>",
            ch + 1
        );
        while xhtml.len() < 38 * 1024 {
            xhtml.push_str("<p>Calm harbor mist across sunlit slate and clean linen typography for streaming readers on tiny SRAM.</p>");
        }
        xhtml.push_str("</body></html>");
        chapter_bodies.push((format!("OEBPS/{href}"), xhtml.into_bytes()));
    }
    // Include an incompressible 1.45 MB embedded cover image alongside 40 XHTML chapters
    // so DEFLATE produces a realistic >1.45 MB multi-asset EPUB archive.
    let mut prng = 0x9E37_79B9_7F4A_7C15u64;
    let mut cover_bytes = vec![0u8; 1_450_000];
    for b in &mut cover_bytes {
        prng ^= prng << 13;
        prng ^= prng >> 7;
        prng ^= prng << 17;
        *b = (prng >> 24) as u8;
    }
    manifest.push_str("<item id=\"cov\" href=\"cover.jpg\" media-type=\"image/jpeg\"/>\n");
    let opf = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><package xmlns=\"http://www.idpf.org/2007/opf\" version=\"2.0\">\
         <metadata xmlns:dc=\"http://purl.org/dc/elements/1.1/\"><dc:title>The Streaming Odyssey</dc:title><dc:creator>J. Browser</dc:creator><dc:language>en</dc:language></metadata>\
         <manifest>{manifest}</manifest><spine>{spine}</spine></package>"
    );
    let mut zip_entries: Vec<(&str, &[u8])> = vec![
        ("META-INF/container.xml", container),
        ("OEBPS/content.opf", opf.as_bytes()),
        ("OEBPS/cover.jpg", cover_bytes.as_slice()),
    ];
    for (name, bytes) in &chapter_bodies {
        zip_entries.push((name.as_str(), bytes.as_slice()));
    }
    let epub_bytes = build_zip_bytes(&zip_entries);
    assert!(epub_bytes.len() > 1_400_000, "expected >1.4 MB EPUB, got {}", epub_bytes.len());
    drop(cover_bytes);
    drop(chapter_bodies);
    drop(opf);

    let mut sd_img = build_adversarial_sd_image();
    let guard = Esp32HeapGuard::enable();

    {
        let mut twin = X3HardwareTwin::from_sd_image(sd_img);
        twin.boot();
        twin.open_home_item(4);
        assert_eq!(twin.app.screen, Screen::VvDrop);

        let pin = twin
            .app
            .vvdrop_auth
            .as_ref()
            .expect("vvdrop auth initialized")
            .pin_str()
            .to_string();

        // 1. GET /api/nonce
        let mut resp = [0u8; 512];
        let n_len = twin.vvdrop_http_exchange(&[b"GET /api/nonce HTTP/1.1\r\nHost: 192.168.4.1\r\n\r\n"], &mut resp);
        let nonce_txt = core::str::from_utf8(&resp[..n_len]).unwrap();
        let s_nonce_hex = nonce_txt.split("\r\n\r\n").nth(1).unwrap().trim();
        let s_nonce = decode_hex_32(s_nonce_hex).expect("valid server nonce hex");

        // 2. POST /api/auth + X-VVDrop-Datetime clock sync
        let c_nonce = [0x42u8; 32];
        let mut pin_bytes = [0u8; 8];
        pin_bytes.copy_from_slice(&pin.as_bytes()[..8]);
        let mac = compute_auth_mac(&pin_bytes, &s_nonce, &c_nonce);
        let auth_hdr = format!(
            "POST /api/auth HTTP/1.1\r\nHost: 192.168.4.1\r\nX-VVDrop-Client-Nonce: {}\r\nX-VVDrop-Mac: {}\r\nX-VVDrop-Datetime: 2027-03-14 09:26:53\r\nContent-Length: 0\r\n\r\n",
            encode_hex(&c_nonce),
            encode_hex(&mac),
        );
        let a_len = twin.vvdrop_http_exchange(&[auth_hdr.as_bytes()], &mut resp);
        let auth_txt = core::str::from_utf8(&resp[..a_len]).unwrap();
        assert!(auth_txt.starts_with("HTTP/1.1 200 OK"), "auth failed: {auth_txt}");
        let token_hex = auth_txt.split("\r\n\r\n").nth(1).unwrap().trim().to_string();

        twin.advance_idle_ms(200);
        let synced_dt = twin.sensors_now();
        assert_eq!(synced_dt.year, 2027);
        assert_eq!(synced_dt.month, 3);
        assert_eq!(synced_dt.day, 14);
        assert_eq!(synced_dt.hour, 9);
        assert_eq!(synced_dt.minute, 26);

        // 3. Stream >1.4 MB EPUB in 1 KB TCP chunks under 320 KB SRAM ceiling
        let up_hdr = format!(
            "POST /api/upload HTTP/1.1\r\nHost: 192.168.4.1\r\nX-VVDrop-Token: {token_hex}\r\nX-VVDrop-Filename: BigNovel.epub\r\nExpect: 100-continue\r\nContent-Length: {}\r\n\r\n",
            epub_bytes.len()
        );
        let mut u_len = twin.vvdrop_http_exchange(&[up_hdr.as_bytes()], &mut resp);
        let cont = core::str::from_utf8(&resp[..u_len]).unwrap();
        assert!(cont.contains("100 Continue"), "expected 100 Continue, got: {cont}");

        let mut final_resp = [0u8; 512];
        let mut final_len = 0usize;
        for chunk in epub_bytes.chunks(1024) {
            let n = twin.vvdrop_http_exchange(&[chunk], &mut resp);
            if n > 0 && final_len + n <= final_resp.len() {
                final_resp[final_len..final_len + n].copy_from_slice(&resp[..n]);
                final_len += n;
            }
        }
        let fin_txt = core::str::from_utf8(&final_resp[..final_len]).unwrap();
        assert!(fin_txt.starts_with("HTTP/1.1 200 OK"), "upload response failed: {fin_txt}");
        u_len = final_len;
        let _ = u_len;

        twin.advance_idle_ms(200);
        assert!(twin.storage_exists("/books/BigNovel.epub"));
        assert_eq!(
            twin.storage_file_len("/books/BigNovel.epub"),
            Some(epub_bytes.len() as u64)
        );
        let mut head = [0u8; 64];
        let mut tail = [0u8; 64];
        twin.storage_read_slice("/books/BigNovel.epub", 0, &mut head).unwrap();
        twin.storage_read_slice(
            "/books/BigNovel.epub",
            (epub_bytes.len() - 64) as u64,
            &mut tail,
        )
        .unwrap();
        assert_eq!(&head[..], &epub_bytes[..64]);
        assert_eq!(&tail[..], &epub_bytes[epub_bytes.len() - 64..]);
        assert_eq!(twin.app.catalog[0].title, "The Streaming Odyssey");

        // Exit VVDrop and open the newly uploaded 1.5 MB EPUB on Reader
        twin.press_button(Button::Back);
        assert_eq!(twin.app.screen, Screen::Home);
        twin.open_home_item(0);
        assert_eq!(twin.app.screen, Screen::Reader);
        twin.press_button(Button::Right);
        assert!(twin.count_dark_panel_pixels() > 1_000);
        guard.assert_no_oom("vvdrop_1500kb_streaming");
        sd_img = twin.into_sd_image();
    }

    let (peak, cap) = guard.verify_clean_and_disable();
    assert!(peak <= cap, "peak SRAM {peak} exceeded {cap}");
    eprintln!("  VVDrop streamed 1.5 MB EPUB + synced DS3231 RTC under 320 KB SRAM (peak {peak}/{cap} B)");
    drop(sd_img);
}

#[test]
fn x3_twin_rapid_power_cycles_wake_hold_and_wifi_sleep_torture() {
    #[derive(Clone, Copy, Debug)]
    enum ExpectedResume {
        Home,
        Reader(usize),
        Chess(u64),
        PocketPip(u8),
    }

    let _test_guard = ESP32_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut sd_img = build_adversarial_sd_image();
    let guard = Esp32HeapGuard::enable();

    // Part A: Held-Power wake + bouncy contact chatter must never re-sleep immediately.
    {
        let mut twin = X3HardwareTwin::from_sd_image(sd_img);
        twin.boot();
        assert_eq!(twin.app.screen, Screen::Home);

        let mut wake_guard = WakePowerGuard::new(twin.uptime_ms());
        for _ in 0..75 {
            twin.sample_raw_with_wake_guard(20, 3100, 3100, true, &mut wake_guard);
            assert!(!wake_guard.is_armed());
            assert_eq!(twin.app.screen, Screen::Home);
            assert!(!twin.hw_stats().deep_sleeping);
        }

        // Bouncy switch release (false/true chatter every 10ms) must keep wake guard unarmed.
        for &raw_pressed in &[false, true, false, true] {
            twin.sample_raw_with_wake_guard(10, 3100, 3100, raw_pressed, &mut wake_guard);
            assert!(!wake_guard.is_armed());
            assert!(!twin.hw_stats().deep_sleeping);
        }

        // Clean release for 240ms (> 40ms release + 120ms post-release settle) arms the guard.
        for _ in 0..12 {
            twin.sample_raw_with_wake_guard(20, 3100, 3100, false, &mut wake_guard);
        }
        assert!(wake_guard.is_armed());

        // Deliberate Power press now cleanly renders SleepPoster and enters deep sleep.
        twin.sample_raw_with_wake_guard(1, 3100, 3100, true, &mut wake_guard);
        twin.sample_raw_with_wake_guard(25, 3100, 3100, true, &mut wake_guard);
        twin.sample_raw_with_wake_guard(1, 3100, 3100, false, &mut wake_guard);
        twin.sample_raw_with_wake_guard(20, 3100, 3100, false, &mut wake_guard);
        assert_eq!(twin.app.screen, Screen::SleepPoster);
        assert!(twin.hw_stats().deep_sleeping);
        assert!(!twin.hw_stats().sd_power_en);
        assert_eq!(twin.hw_stats().sd_rail_faults, 0);
        assert!(twin.count_dark_panel_pixels() > 500);
        guard.assert_no_oom("wake_power_guard_torture");
        sd_img = twin.into_sd_image();
    }

    // Part B: 25 consecutive Power-off -> Cold-boot cycles across every screen + active WPA2 SoftAP.
    let mut expected_resume = ExpectedResume::Home;
    let mut http_resp = [0u8; 4096];
    for cycle in 0..25usize {
        let mut twin = X3HardwareTwin::from_sd_image(sd_img);
        twin.boot();
        assert!(!twin.portal_is_active(), "Wi-Fi portal must start disabled on boot (cycle {cycle})");

        match expected_resume {
            ExpectedResume::Home => assert_eq!(twin.app.screen, Screen::Home, "cycle {cycle}"),
            ExpectedResume::Reader(page) => {
                assert_eq!(twin.app.screen, Screen::Reader, "cycle {cycle}");
                assert_eq!(twin.app.current_page_num, page, "cycle {cycle}");
            }
            ExpectedResume::Chess(hash) => {
                assert_eq!(twin.app.screen, Screen::Chess, "cycle {cycle}");
                assert_eq!(twin.app.chess.pos.hash, hash, "cycle {cycle}");
            }
            ExpectedResume::PocketPip(pip_pos) => {
                assert_eq!(twin.app.screen, Screen::PocketPip, "cycle {cycle}");
                assert_eq!(twin.app.pip.meadow.pip, pip_pos, "cycle {cycle}");
            }
        }

        match cycle % 6 {
            0 => {
                twin.ensure_home();
                twin.open_home_item(1); // Library
                assert_eq!(twin.app.screen, Screen::Library);
                twin.press_button(Button::Down);
                twin.press_button(Button::Down);
                expected_resume = ExpectedResume::Home;
            }
            1 => {
                twin.ensure_home();
                twin.open_home_item(1);
                twin.open_library_index(0);
                assert_eq!(twin.app.screen, Screen::Reader);
                if twin.app.current_page_num + 2 < twin.app.active_pages.len() {
                    twin.press_button(Button::Right);
                    twin.press_button(Button::Right);
                } else if twin.app.current_page_num >= 2 {
                    twin.press_button(Button::Left);
                    twin.press_button(Button::Left);
                }
                expected_resume = ExpectedResume::Reader(twin.app.current_page_num);
            }
            2 => {
                twin.ensure_home();
                twin.open_home_item(2); // Chess
                assert_eq!(twin.app.screen, Screen::Chess);
                if twin.app.chess.mode == ChessMode::Menu {
                    while twin.app.chess.menu_cursor != 1 {
                        twin.press_button(Button::Down); // Play vs AI
                    }
                    twin.press_button(Button::Confirm);
                    twin.press_button(Button::Confirm); // Select e2
                    twin.press_button(Button::Up);
                    twin.press_button(Button::Up);
                    twin.press_button(Button::Confirm); // Move e2-e4 -> AI responds
                } else if twin.app.chess.history.len() >= 2 {
                    twin.press_button(Button::Back); // Undo last full turn
                }
                expected_resume = ExpectedResume::Chess(twin.app.chess.pos.hash);
            }
            3 => {
                twin.ensure_home();
                twin.open_home_item(3); // PocketPip
                assert_eq!(twin.app.screen, Screen::PocketPip);
                if twin.app.pip.tab != PipTab::Meadow {
                    while twin.app.pip.den_cursor != 2 {
                        twin.press_button(Button::Down);
                    }
                    twin.press_button(Button::Confirm);
                }
                assert_eq!(twin.app.pip.tab, PipTab::Meadow);
                twin.press_button(Button::Right);
                expected_resume = ExpectedResume::PocketPip(twin.app.pip.meadow.pip);
            }
            4 => {
                twin.ensure_home();
                twin.open_home_item(5); // Settings
                assert_eq!(twin.app.screen, Screen::Settings);
                twin.press_button(Button::Down);
                expected_resume = ExpectedResume::Home;
            }
            _ => {
                twin.ensure_home();
                twin.open_home_item(4); // VVDrop with active WPA2 SoftAP + HTTP session
                assert_eq!(twin.app.screen, Screen::VvDrop);
                assert!(twin.portal_is_active(), "cycle {cycle}: VVDrop must activate portal");
                let psk = twin
                    .app
                    .vvdrop_auth
                    .as_ref()
                    .expect("vvdrop auth initialized")
                    .pin_str();
                assert_eq!(psk.len(), 8);
                assert!(psk.bytes().all(|b| b.is_ascii_digit()));

                let rx_len = twin.vvdrop_http_exchange(
                    &[b"GET / HTTP/1.1\r\nHost: 192.168.4.1\r\n\r\n"],
                    &mut http_resp,
                );
                let head = core::str::from_utf8(&http_resp[..rx_len.min(64)]).unwrap_or("");
                assert!(head.starts_with("HTTP/1.1 200 OK"), "cycle {cycle}: bad HTTP response: {head}");
                expected_resume = ExpectedResume::Home;
            }
        }

        // Power off straight from whichever subsystem was active (including live Wi-Fi + HTTP).
        twin.press_button(Button::Power);
        assert!(!twin.portal_is_active(), "cycle {cycle}: portal remained active after Power");
        assert_eq!(twin.app.screen, Screen::SleepPoster, "cycle {cycle}");
        assert!(twin.hw_stats().deep_sleeping, "cycle {cycle}");
        assert!(!twin.hw_stats().sd_power_en, "cycle {cycle}");
        assert_eq!(twin.hw_stats().sd_rail_faults, 0, "cycle {cycle}");
        assert!(twin.count_dark_panel_pixels() > 500, "cycle {cycle}");
        guard.assert_no_oom("rapid_power_cycle");
        sd_img = twin.into_sd_image();
    }

    let (peak, cap) = guard.verify_clean_and_disable();
    assert!(peak <= cap, "peak SRAM {peak} exceeded {cap}");
    eprintln!(
        "  Completed 25 consecutive Power-off/reboot cycles (incl. live WPA2 VVDrop) under {} KB SRAM (peak {peak}/{cap} B)",
        cap / 1024
    );
    drop(sd_img);
}

fn make_gauntlet_epub(title: &str, author: &str) -> Vec<u8> {
    let container = br#"<?xml version="1.0" encoding="utf-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#;
    let opf = format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>{title}</dc:title>
    <dc:creator>{author}</dc:creator>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="ch1" href="ch1.xhtml" media-type="application/xhtml+xml"/>
    <item id="ch2" href="ch2.xhtml" media-type="application/xhtml+xml"/>
    <item id="ch3" href="ch3.xhtml" media-type="application/xhtml+xml"/>
    <item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>
  </manifest>
  <spine toc="ncx">
    <itemref idref="ch1"/>
    <itemref idref="ch2"/>
    <itemref idref="ch3"/>
  </spine>
</package>"#
    );
    let ncx = br#"<?xml version="1.0" encoding="utf-8"?>
<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1">
  <navMap>
    <navPoint id="n1" playOrder="1"><navLabel><text>Chapter 1</text></navLabel><content src="ch1.xhtml"/></navPoint>
    <navPoint id="n2" playOrder="2"><navLabel><text>Chapter 2</text></navLabel><content src="ch2.xhtml"/></navPoint>
    <navPoint id="n3" playOrder="3"><navLabel><text>Chapter 3</text></navLabel><content src="ch3.xhtml"/></navPoint>
  </navMap>
</ncx>"#;
    let ch1 = format!(
        "<html><body><h1>Chapter 1</h1><p>{title} opening section with enough English prose to paginate cleanly on the E-Ink display.</p></body></html>"
    );
    let ch2 = format!(
        "<html><body><h1>Chapter 2</h1><p>{title} second section verifying v2 to v3 reading progress migration across chapters.</p></body></html>"
    );
    let ch3 = format!(
        "<html><body><h1>Chapter 3</h1><p>{title} closing chapter content for complete EPUB spine traversal.</p></body></html>"
    );
    build_zip_bytes(&[
        ("mimetype", b"application/epub+zip"),
        ("META-INF/container.xml", container),
        ("OEBPS/content.opf", opf.as_bytes()),
        ("OEBPS/toc.ncx", ncx),
        ("OEBPS/ch1.xhtml", ch1.as_bytes()),
        ("OEBPS/ch2.xhtml", ch2.as_bytes()),
        ("OEBPS/ch3.xhtml", ch3.as_bytes()),
    ])
}

fn make_gauntlet_fb2(num: usize) -> Vec<u8> {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<FictionBook xmlns="http://www.gribuser.ru/xml/fictionbook/2.0">
  <description>
    <title-info>
      <book-title>Shelf Volume {num:02}</book-title>
      <author><first-name>Elena</first-name><last-name>Author{num:02}</last-name></author>
      <lang>en</lang>
    </title-info>
  </description>
  <body>
    <section>
      <title><p>Volume {num:02}</p></title>
      <p>Clean FB2 prose for Shelf Volume {num:02}, filling all eight rows of the full-screen portrait library layout and opening reliably on click.</p>
    </section>
  </body>
</FictionBook>"#
    )
    .into_bytes()
}

fn make_gauntlet_mobi(
    pdb_title: &str,
    encryption_type: u16,
    mobi_version: u32,
    flows: &[&[u8]],
    include_fdst: bool,
) -> Vec<u8> {
    let mut records: Vec<Vec<u8>> = Vec::with_capacity(1 + flows.len() + usize::from(include_fdst));
    let total_text_len: u32 = flows.iter().map(|f| f.len() as u32).sum();
    let title_bytes = pdb_title.as_bytes();
    let mut r0 = vec![0u8; 256 + title_bytes.len()];
    r0[0..2].copy_from_slice(&1u16.to_be_bytes()); // compression = 1 (None)
    r0[4..8].copy_from_slice(&total_text_len.to_be_bytes());
    r0[8..10].copy_from_slice(&(flows.len() as u16).to_be_bytes());
    r0[10..12].copy_from_slice(&4096u16.to_be_bytes()); // record_size
    r0[12..14].copy_from_slice(&encryption_type.to_be_bytes());
    r0[16..20].copy_from_slice(b"MOBI");
    r0[20..24].copy_from_slice(&232u32.to_be_bytes()); // mobi_hdr_len
    r0[28..32].copy_from_slice(&65001u32.to_be_bytes()); // codepage UTF-8
    r0[36..40].copy_from_slice(&mobi_version.to_be_bytes());
    r0[84..88].copy_from_slice(&256u32.to_be_bytes()); // full_name offset
    r0[88..92].copy_from_slice(&(title_bytes.len() as u32).to_be_bytes());
    r0[256..].copy_from_slice(title_bytes);
    if include_fdst && flows.len() >= 2 {
        let fdst_rec = (1 + flows.len()) as u32;
        r0[192..196].copy_from_slice(&fdst_rec.to_be_bytes());
        r0[196..200].copy_from_slice(&(flows.len() as u32).to_be_bytes());
    } else {
        r0[192..196].copy_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
    }
    r0[240..244].copy_from_slice(&0u32.to_be_bytes()); // extra_flags = 0
    records.push(r0);
    for &flow in flows {
        records.push(flow.to_vec());
    }
    if include_fdst && flows.len() >= 2 {
        let f0 = flows[0].len() as u32;
        let f1 = flows[1].len() as u32;
        let mut fdst = Vec::with_capacity(28);
        fdst.extend_from_slice(b"FDST");
        fdst.extend_from_slice(&12u32.to_be_bytes());
        fdst.extend_from_slice(&(flows.len() as u32).to_be_bytes());
        fdst.extend_from_slice(&0u32.to_be_bytes());
        fdst.extend_from_slice(&f0.to_be_bytes());
        fdst.extend_from_slice(&f0.to_be_bytes());
        fdst.extend_from_slice(&(f0 + f1).to_be_bytes());
        records.push(fdst);
    }

    let num_records = records.len();
    let header_len = 78 + num_records * 8 + 2;
    let mut out = vec![0u8; header_len];
    let n = title_bytes.len().min(31);
    out[..n].copy_from_slice(&title_bytes[..n]);
    out[60..68].copy_from_slice(b"BOOKMOBI");
    out[76..78].copy_from_slice(&(num_records as u16).to_be_bytes());
    let mut off = header_len;
    for (i, rec) in records.iter().enumerate() {
        let tbl = 78 + i * 8;
        out[tbl..tbl + 4].copy_from_slice(&(off as u32).to_be_bytes());
        out[tbl + 4..tbl + 8].copy_from_slice(&(i as u32).to_be_bytes());
        off += rec.len();
        out.extend_from_slice(rec);
    }
    out
}

struct HostCalibreOverlayStorage {
    books_root: PathBuf,
    overlay: BTreeMap<String, Vec<u8>>,
}

impl HostCalibreOverlayStorage {
    fn new(books_root: PathBuf) -> Self {
        Self {
            books_root,
            overlay: BTreeMap::new(),
        }
    }

    fn resolve_books_path(&self, path: &str) -> Option<PathBuf> {
        if path == "/Books" {
            Some(self.books_root.clone())
        } else {
            path.strip_prefix("/Books/").map(|rel| self.books_root.join(rel))
        }
    }

    fn is_virtual_dir(&self, path: &str) -> bool {
        if matches!(path, "/" | "/.vvbook" | "/.vvbook/books" | "/.vvbook/layout" | "/books") {
            return true;
        }
        let prefix = if path == "/" {
            String::from("/")
        } else {
            format!("{}/", path.trim_end_matches('/'))
        };
        self.overlay.keys().any(|k| k.starts_with(&prefix))
    }
}

impl StorageBackend for HostCalibreOverlayStorage {
    fn exists(&self, path: &str) -> bool {
        if self.overlay.contains_key(path) || self.is_virtual_dir(path) {
            return true;
        }
        with_host_alloc(|| self.resolve_books_path(path).is_some_and(|p| p.exists()))
    }

    fn file_len(&self, path: &str) -> Result<u64, StorageError> {
        if let Some(v) = self.overlay.get(path) {
            return Ok(v.len() as u64);
        }
        let p = self.resolve_books_path(path).ok_or(StorageError::NotFound)?;
        with_host_alloc(|| fs::metadata(&p).map(|m| m.len()).map_err(|_| StorageError::Io))
    }

    fn read_all(&self, path: &str) -> Result<Vec<u8>, StorageError> {
        if let Some(v) = self.overlay.get(path) {
            let mut out = Vec::with_capacity(v.len());
            out.extend_from_slice(v);
            return Ok(out);
        }
        let len = self.file_len(path)?;
        if len > 256 * 1024 {
            return Err(StorageError::Io);
        }
        let mut out = vec![0u8; len as usize];
        let n = self.read_range(path, 0, &mut out)?;
        out.truncate(n);
        Ok(out)
    }

    fn read_range(&self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize, StorageError> {
        if let Some(v) = self.overlay.get(path) {
            let start = (offset as usize).min(v.len());
            let take = (v.len() - start).min(buf.len());
            buf[..take].copy_from_slice(&v[start..start + take]);
            return Ok(take);
        }
        let p = self.resolve_books_path(path).ok_or(StorageError::NotFound)?;
        with_host_alloc(|| {
            let mut f = File::open(&p).map_err(|_| StorageError::Io)?;
            f.seek(SeekFrom::Start(offset)).map_err(|_| StorageError::Io)?;
            let mut total = 0usize;
            while total < buf.len() {
                match f.read(&mut buf[total..]) {
                    Ok(0) => break,
                    Ok(n) => total += n,
                    Err(_) => return Err(StorageError::Io),
                }
            }
            Ok(total)
        })
    }

    fn write_all(&mut self, path: &str, data: &[u8]) -> Result<(), StorageError> {
        self.write_atomic(path, data)
    }

    fn write_atomic(&mut self, path: &str, data: &[u8]) -> Result<(), StorageError> {
        if path == "/Books" || path.starts_with("/Books/") {
            return Err(StorageError::InvalidPath);
        }
        with_host_alloc(|| {
            self.overlay.insert(String::from(path), data.to_vec());
        });
        Ok(())
    }

    fn append(&mut self, path: &str, data: &[u8]) -> Result<(), StorageError> {
        if path == "/Books" || path.starts_with("/Books/") {
            return Err(StorageError::InvalidPath);
        }
        with_host_alloc(|| {
            let ent = self.overlay.entry(String::from(path)).or_default();
            ent.extend_from_slice(data);
        });
        Ok(())
    }

    fn remove(&mut self, path: &str) -> Result<(), StorageError> {
        if path == "/Books" || path.starts_with("/Books/") {
            return Err(StorageError::InvalidPath);
        }
        with_host_alloc(|| {
            self.overlay.remove(path);
        });
        Ok(())
    }

    fn mkdir_all(&mut self, path: &str) -> Result<(), StorageError> {
        if path == "/Books" || path.starts_with("/Books/") {
            return Err(StorageError::InvalidPath);
        }
        Ok(())
    }

    fn list_dir(&self, path: &str) -> Result<Vec<DirEntry>, StorageError> {
        if path == "/" {
            return Ok(vec![DirEntry {
                name: String::from("Books"),
                is_dir: true,
                size: 0,
            }]);
        }
        if let Some(host_dir) = self.resolve_books_path(path) {
            let raw_entries: Result<Vec<(String, bool, u64)>, StorageError> =
                with_host_alloc(|| {
                    let rd = fs::read_dir(&host_dir).map_err(|_| StorageError::NotFound)?;
                    let mut list = Vec::new();
                    for ent in rd.flatten() {
                        let name = ent.file_name().to_string_lossy().into_owned();
                        let Ok(meta) = ent.metadata() else { continue };
                        let is_dir = meta.is_dir();
                        let size = if is_dir { 0 } else { meta.len() };
                        list.push((name, is_dir, size));
                    }
                    list.sort_by(|a, b| a.0.cmp(&b.0));
                    Ok(list)
                });
            let raw_entries = raw_entries?;
            let mut out = Vec::with_capacity(raw_entries.len());
            for (name, is_dir, size) in &raw_entries {
                out.push(DirEntry {
                    name: String::from(name.as_str()),
                    is_dir: *is_dir,
                    size: *size,
                });
            }
            with_host_alloc(|| drop(raw_entries));
            return Ok(out);
        }
        if !self.is_virtual_dir(path) {
            return Err(StorageError::NotFound);
        }
        let raw_entries: Vec<(String, bool, u64)> = with_host_alloc(|| {
            let prefix = format!("{}/", path.trim_end_matches('/'));
            let mut seen: BTreeMap<String, (bool, u64)> = BTreeMap::new();
            for (k, v) in &self.overlay {
                if let Some(rest) = k.strip_prefix(&prefix) {
                    if let Some((dir_name, _)) = rest.split_once('/') {
                        seen.entry(String::from(dir_name)).or_insert((true, 0));
                    } else {
                        seen.entry(String::from(rest)).or_insert((false, v.len() as u64));
                    }
                }
            }
            seen.into_iter().map(|(n, (d, s))| (n, d, s)).collect()
        });
        let mut out = Vec::with_capacity(raw_entries.len());
        for (name, is_dir, size) in &raw_entries {
            out.push(DirEntry {
                name: String::from(name.as_str()),
                is_dir: *is_dir,
                size: *size,
            });
        }
        with_host_alloc(|| drop(raw_entries));
        Ok(out)
    }
}

struct BenchNullSink;

impl DisplaySink for BenchNullSink {
    fn begin_frame(&mut self, _mode: RefreshMode) {}
    fn write_strip_2bpp(&mut self, _y_start: usize, _rows: usize, _packed_2bpp: &[u8]) {}
    fn end_frame(&mut self, _mode: RefreshMode) {}
}

struct BenchSensors {
    uptime: u64,
}

impl DeviceSensors for BenchSensors {
    fn battery(&mut self) -> BatteryStatus {
        BatteryStatus {
            percent: 88,
            millivolts: 4050,
            current_ma: -30,
            charging: false,
        }
    }
    fn now(&mut self) -> DateTime {
        DateTime {
            year: 2026,
            month: 11,
            day: 13,
            hour: 15,
            minute: 30,
            second: 0,
        }
    }
    fn uptime_ms(&self) -> u64 {
        self.uptime
    }
    fn temperature_c_x10(&mut self) -> i16 {
        245
    }
}

struct BenchPower;

impl PowerControl for BenchPower {
    fn enter_deep_sleep(&mut self) {}
    fn set_imu_flick(&mut self, _enabled: bool) {}
    fn set_rotated_180(&mut self, _rotated: bool) {}
    fn controller_name(&self) -> &'static str {
        "UC8253"
    }
}

struct BenchPortal;

impl NetworkPortal for BenchPortal {
    fn start(&mut self, _pin: &str) {}
    fn stop(&mut self) {}
    fn status(&mut self) -> PortalStatus {
        PortalStatus {
            active: false,
            ssid: String::new(),
            url: String::new(),
            pin: String::new(),
            clients_connected: 0,
            rx_bytes: 0,
            total_bytes: 0,
            last_received_file: None,
            busy_message: None,
        }
    }
    fn take_pending_ingest(&mut self) -> Option<(String, Vec<u8>)> {
        None
    }
}

struct BenchRng(u64);

impl EntropySource for BenchRng {
    fn fill_bytes(&mut self, out: &mut [u8]) {
        for b in out.iter_mut() {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            *b = (self.0 >> 32) as u8;
        }
    }
}

#[test]
fn real_calibre_corpus_and_garbage_gauntlet_under_esp32_heap() {
    let _test_guard = ESP32_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // Part 1: FAT32 Garbage Gauntlet, v2->v3 migration, KF8 FDST Flow 1 stripping,
    // 8-row full-screen Library rendering, and corrupt-book error notice.
    let great_novel_path = "/Books/CalibreAuthor/Great Novel (42)/Great Novel - CalibreAuthor.epub";
    let great_epub = make_gauntlet_epub("Great Novel", "CalibreAuthor");
    let great_mobi = make_gauntlet_mobi(
        "Great Novel",
        0,
        6,
        &[b"<html><body><h1>Great Novel</h1><p>Mobi sibling chapter for Calibre deduplication check.</p></body></html>"],
        false,
    );
    let flow0 = b"<html><body><h1>Clean KF8 Story</h1><p>Readable prose without stylesheet leak across records.</p></body></html>";
    let flow1 = b"body { font-family: serif; } .calibre1 { display: block; }";
    let fdst_azw3 = make_gauntlet_mobi("KF8 Clean Story", 0, 8, &[flow0, flow1], true);
    let drm_azw = make_gauntlet_mobi(
        "DRM Locked Book",
        2,
        6,
        &[b"<html><body>drm encrypted payload</body></html>"],
        false,
    );

    let seed_v2_entries = vec![
        CatalogEntry {
            id_hex: String::from("legacy_great_novel_v2"),
            source_path: String::from(great_novel_path),
            title: String::from("Great Novel"),
            author: String::from("CalibreAuthor"),
            language: "en",
            source_format: "EPUB",
            word_count: 180,
            block_count: 10,
            last_block: 1,
            last_word: 12,
            total_pages_cached: 10,
            current_page_cached: 3,
            bookmarked_block: Some(1),
        },
        CatalogEntry {
            id_hex: String::from("poison_sdr"),
            source_path: String::from("/Books/Book.sdr/notes.txt"),
            title: String::from("notes"),
            author: String::from("Kindle"),
            language: "en",
            source_format: "TXT",
            word_count: 10,
            block_count: 1,
            last_block: 0,
            last_word: 0,
            total_pages_cached: 1,
            current_page_cached: 0,
            bookmarked_block: None,
        },
        CatalogEntry {
            id_hex: String::from("poison_recycle"),
            source_path: String::from("/$RECYCLE.BIN/recycled.epub"),
            title: String::from("recycled"),
            author: String::from("Trash"),
            language: "en",
            source_format: "EPUB",
            word_count: 10,
            block_count: 1,
            last_block: 0,
            last_word: 0,
            total_pages_cached: 1,
            current_page_cached: 0,
            bookmarked_block: None,
        },
        CatalogEntry {
            id_hex: String::from("poison_drm"),
            source_path: String::from("/Books/Corrupt/drm_locked.azw"),
            title: String::from("DRM"),
            author: String::from("DRM"),
            language: "en",
            source_format: "AZW",
            word_count: 10,
            block_count: 1,
            last_block: 0,
            last_word: 0,
            total_pages_cached: 1,
            current_page_cached: 0,
            bookmarked_block: None,
        },
        CatalogEntry {
            id_hex: String::from("poison_clippings"),
            source_path: String::from("/Books/My Clippings - Unknown.txt"),
            title: String::from("My Clippings"),
            author: String::from("Unknown"),
            language: "en",
            source_format: "TXT",
            word_count: 10,
            block_count: 1,
            last_block: 0,
            last_word: 0,
            total_pages_cached: 1,
            current_page_cached: 0,
            bookmarked_block: None,
        },
    ];
    let mut raw_v2_catalog = encode_catalog(&seed_v2_entries);
    raw_v2_catalog[4..6].copy_from_slice(&2u16.to_le_bytes());
    assert_eq!(catalog_format_version(&raw_v2_catalog), Some(2));

    let fb2_vols: Vec<(String, Vec<u8>)> = (1..=9)
        .map(|i| (format!("/Books/Shelf/vol_{i:02}.fb2"), make_gauntlet_fb2(i)))
        .collect();

    let mut files: Vec<(&str, &[u8])> = vec![
        ("/.vvbook/catalog.vvc", raw_v2_catalog.as_slice()),
        (great_novel_path, great_epub.as_slice()),
        (
            "/Books/CalibreAuthor/Great Novel (42)/Great Novel - CalibreAuthor.mobi",
            great_mobi.as_slice(),
        ),
        (
            "/Books/CalibreAuthor/Great Novel (42)/metadata.opf",
            b"<package><dc:title>Great Novel</dc:title></package>",
        ),
        (
            "/Books/CalibreAuthor/Great Novel (42)/cover.jpg",
            b"\xff\xd8\xff\xe0JFIF_DUMMY",
        ),
        (
            "/Books/Book.sdr/notes.txt",
            b"Kindle sidecar notes that must never appear in the VVBook Library.",
        ),
        ("/$RECYCLE.BIN/recycled.epub", great_epub.as_slice()),
        (
            "/System Volume Information/IndexerVolumeGuid.txt",
            b"{12345678-1234-1234-1234-123456789012}",
        ),
        (
            "/FOUND.000/salvaged.txt",
            b"CHKDSK salvaged text fragment should be ignored completely.",
        ),
        (
            "/crosspoint/cache.txt",
            b"CrossPoint reader metadata text file must be ignored.",
        ),
        (
            "/koreader/history.lua",
            b"-- KOReader Lua metadata file",
        ),
        (
            "/.Spotlight-V100/store.txt",
            b"macOS Spotlight index store text file must be ignored.",
        ),
        ("/Books/._MacResourceFork.epub", great_epub.as_slice()),
        (
            "/Books/My Clippings - Unknown.txt",
            b"==========\nSome highlight\n- Your Highlight on page 1\n",
        ),
        ("/Books/Corrupt/truncated_bad.epub", b"PK\x03\x04broken"),
        ("/Books/Corrupt/empty_file.fb2", b""),
        ("/Books/Corrupt/drm_locked.azw", drm_azw.as_slice()),
        ("/Books/KF8/fdst_book.azw3", fdst_azw3.as_slice()),
    ];
    for (p, data) in &fb2_vols {
        files.push((p.as_str(), data.as_slice()));
    }

    let mut sd_img = build_fat32_sd_image_from_slice(&files).expect("build gauntlet FAT32 SD");
    drop(great_epub);
    drop(great_mobi);
    drop(fdst_azw3);
    drop(drm_azw);
    drop(raw_v2_catalog);
    drop(fb2_vols);

    let guard = Esp32HeapGuard::enable();
    {
        let mut twin = X3HardwareTwin::from_sd_image(sd_img);
        twin.boot();

        let mut hdr_buf = [0u8; 16];
        let hdr_n = twin
            .storage_read_slice("/.vvbook/catalog.vvc", 0, &mut hdr_buf)
            .expect("migrated catalog present");
        assert!(hdr_n >= 10);
        assert_eq!(catalog_format_version(&hdr_buf[..hdr_n]), Some(3));

        assert_eq!(
            twin.app.catalog.len(),
            11,
            "expected exactly 11 clean books (got {:?})",
            twin.app
                .catalog
                .iter()
                .map(|c| c.source_path.as_str())
                .collect::<Vec<_>>()
        );
        let migrated = twin
            .app
            .catalog
            .iter()
            .find(|c| c.source_path == great_novel_path)
            .expect("Great Novel EPUB present in migrated v3 catalog");
        assert_eq!(migrated.last_block, 1);
        assert_eq!(migrated.last_word, 12);
        assert_eq!(migrated.total_pages_cached, 10);
        assert_eq!(migrated.current_page_cached, 3);
        assert_eq!(migrated.bookmarked_block, Some(1));
        for c in &twin.app.catalog {
            let p = c.source_path.as_str();
            assert!(
                !p.contains(".sdr")
                    && !p.contains("$RECYCLE.BIN")
                    && !p.contains("System Volume Information")
                    && !p.contains("FOUND.000")
                    && !p.contains("crosspoint")
                    && !p.contains("koreader")
                    && !p.contains("Spotlight")
                    && !p.contains("._MacResourceFork")
                    && !p.contains("My Clippings")
                    && !p.contains("truncated_bad")
                    && !p.contains("empty_file")
                    && !p.contains("drm_locked")
                    && !p.ends_with(".mobi"),
                "garbage/duplicate leaked into catalog: {p}"
            );
        }

        twin.open_home_item(1);
        assert_eq!(twin.app.screen, Screen::Library);
        assert_eq!(LIBRARY_PER_PAGE, 8);
        let lower_half_dark = {
            let panel = with_host_alloc(|| twin.decode_panel_2bpp());
            let mut dark = 0usize;
            for y in 400..725 {
                let row_off = y * (PORTRAIT_W / 4);
                for x in 0..PORTRAIT_W {
                    let b = panel[row_off + (x / 4)];
                    let bits = (b >> (6 - (x % 4) * 2)) & 0x03;
                    if bits < 3 {
                        dark += 1;
                    }
                }
            }
            with_host_alloc(|| drop(panel));
            dark
        };
        assert!(
            lower_half_dark > 500,
            "lower half of full-screen 8-row Library (y=400..725) must be populated, got {lower_half_dark} dark px"
        );

        for _ in 0..8 {
            twin.press_button(Button::Down);
        }
        assert_eq!(twin.app.lib_cursor, 8);
        for _ in 0..8 {
            twin.press_button(Button::Up);
        }
        assert_eq!(twin.app.lib_cursor, 0);

        for i in 0..twin.app.catalog.len() {
            twin.open_library_index(i);
            assert_eq!(twin.app.screen, Screen::Reader, "failed opening idx {i}");
            assert!(
                !twin.app.active_pages.is_empty(),
                "empty active_pages at idx {i}"
            );
            assert!(
                twin.count_dark_panel_pixels() > 500,
                "blank Reader screen at idx {i}"
            );
            if twin.app.catalog[i].source_path.ends_with("fdst_book.azw3") {
                for b in &twin.app.direct_section_blocks {
                    let dbg = format!("{b:?}");
                    assert!(
                        !dbg.contains("font-family") && !dbg.contains("calibre1"),
                        "KF8 Flow 1 CSS leaked into Reader block: {dbg}"
                    );
                }
            }
            twin.press_button(Button::Back);
            assert_eq!(twin.app.screen, Screen::Home);
            guard.assert_no_oom("gauntlet_open");
        }

        twin.open_home_item(1);
        twin.app.catalog[0].source_path = String::from("/Books/Corrupt/truncated_bad.epub");
        twin.app.catalog[0].id_hex = String::from("broken_entry_test");
        twin.select_library_index(0);
        twin.press_button(Button::Confirm);
        assert_eq!(twin.app.screen, Screen::Library);
        assert!(
            twin.app.library_notice.is_some(),
            "expected Library notice when opening corrupt EPUB"
        );
        assert!(
            twin.app.take_last_open_diag().is_some(),
            "expected take_last_open_diag when opening corrupt EPUB"
        );

        sd_img = twin.into_sd_image();
    }
    let (p1_peak, p1_cap) = guard.verify_clean_and_disable();
    assert!(p1_peak <= p1_cap, "Part 1 peak SRAM {p1_peak} exceeded {p1_cap}");
    drop(sd_img);

    // Part 2: Full FolioApp::boot + exhaustive UI open across 100% of cataloged books in ~/books
    let home = match std::env::var_os("HOME") {
        Some(h) => PathBuf::from(h).join("books"),
        None => return,
    };
    if !home.is_dir() {
        return;
    }

    let mut overlay_storage = HostCalibreOverlayStorage::new(home);
    let cat_len;
    let guard = Esp32HeapGuard::enable();
    {
        let mut app = Box::new(FolioApp::new());
        let mut sink = BenchNullSink;
        let mut sensors = BenchSensors { uptime: 1_000 };
        let mut power = BenchPower;
        let mut portal = BenchPortal;
        let mut rng = BenchRng(0x5656_0000_0000_0602);

        app.boot(
            &mut sink,
            &mut overlay_storage,
            &mut sensors,
            &mut power,
            &mut portal,
            &mut rng,
        );
        guard.assert_no_oom("boot_home_books");
        cat_len = app.catalog.len();
        assert!(
            cat_len >= 150,
            "expected >= 150 deduplicated books from ~/books, got {cat_len}"
        );

        with_host_alloc(|| {
            let mut seen_siblings: BTreeMap<(String, String), String> = BTreeMap::new();
            for c in &app.catalog {
                assert!(
                    is_valid_library_book_path(&c.source_path),
                    "invalid path leaked into catalog: {}",
                    c.source_path
                );
                assert!(
                    !c.source_path.contains("My Clippings"),
                    "My Clippings leaked into catalog: {}",
                    c.source_path
                );
                let dir = c
                    .source_path
                    .rsplit_once('/')
                    .map(|(d, _)| d.to_ascii_lowercase())
                    .unwrap_or_default();
                let file = c.source_path.rsplit('/').next().unwrap_or("");
                let stem = file
                    .to_ascii_lowercase()
                    .strip_suffix(".fb2.zip")
                    .map(String::from)
                    .unwrap_or_else(|| {
                        file.rsplit_once('.')
                            .map(|(s, _)| s.to_ascii_lowercase())
                            .unwrap_or_else(|| file.to_ascii_lowercase())
                    });
                let key = (dir, stem);
                if let Some(prev) = seen_siblings.insert(key, c.source_path.clone()) {
                    panic!(
                        "duplicate Calibre sibling in catalog: {prev} AND {}",
                        c.source_path
                    );
                }
            }
        });

        app.screen = Screen::Library;
        for idx in 0..cat_len {
            let src = with_host_alloc(|| app.catalog[idx].source_path.clone());
            sensors.uptime += 500;
            app.lib_cursor = idx;
            app.library_notice = None;
            app.handle_event(
                InputEvent::Press(Button::Confirm),
                &mut sink,
                &mut overlay_storage,
                &mut sensors,
                &mut power,
                &mut portal,
                &mut rng,
            );
            assert_eq!(
                app.screen, Screen::Reader,
                "FolioApp failed to open catalog[{idx}] {src} (notice: {:?})",
                app.library_notice
            );
            assert!(
                app.library_notice.is_none(),
                "unexpected library_notice for {src}: {:?}",
                app.library_notice
            );
            assert!(
                !app.active_pages.is_empty(),
                "zero pages after opening {src}"
            );

            sensors.uptime += 250;
            app.handle_event(
                InputEvent::Press(Button::Right),
                &mut sink,
                &mut overlay_storage,
                &mut sensors,
                &mut power,
                &mut portal,
                &mut rng,
            );
            assert_eq!(app.screen, Screen::Reader, "turn failed for {src}");

            sensors.uptime += 250;
            app.handle_event(
                InputEvent::Press(Button::Back),
                &mut sink,
                &mut overlay_storage,
                &mut sensors,
                &mut power,
                &mut portal,
                &mut rng,
            );
            assert_eq!(app.screen, Screen::Home, "Back failed for {src}");
            app.screen = Screen::Library;
            guard.assert_no_oom(&src);
            with_host_alloc(|| drop(src));
        }

        drop(app);
    }
    let (p2_peak, p2_cap) = guard.verify_clean_and_disable();
    assert!(p2_peak <= p2_cap, "Part 2 peak SRAM {p2_peak} exceeded {p2_cap}");
    eprintln!(
        "  Calibre Gauntlet Part 1 peak {p1_peak}/{p1_cap} B; Part 2 ({cat_len} books in ~/books via FolioApp UI) peak {p2_peak}/{p2_cap} B"
    );
    drop(overlay_storage);
}
