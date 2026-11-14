#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

/// Logical portrait display width in pixels (Xteink X3 is 528x792 portrait,
/// matching native 792x528 controller scan rotated 90° into reading portrait).
pub const PORTRAIT_W: usize = 528;
/// Logical portrait display height in pixels.
pub const PORTRAIT_H: usize = 792;
/// 1-bpp framebuffer byte count: 528 * 792 / 8 = 52,272 bytes.
pub const FB_1BPP_BYTES: usize = (PORTRAIT_W / 8) * PORTRAIT_H;
/// 2-bpp row byte count: 528 / 4 = 132 bytes.
pub const ROW_2BPP_BYTES: usize = PORTRAIT_W / 4;
/// Strip height used for low-RAM 4-tone E-Ink streaming on ESP32-C3.
pub const STRIP_ROWS: usize = 64;

/// Physical bezel insets on the Xteink X3 chassis where glass is recessed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Insets {
    pub top: usize,
    pub right: usize,
    pub bottom: usize,
    pub left: usize,
}

pub const SAFE_INSETS: Insets = Insets {
    top: 9,
    right: 3,
    bottom: 3,
    left: 3,
};

/// Four-tone E-Ink reflectance levels (2-bit encoded, white = 3, black = 0).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Tone {
    Ink = 0,
    Charcoal = 1,
    Silver = 2,
    Paper = 3,
}

impl Tone {
    #[inline]
    pub const fn from_bits(bits: u8) -> Self {
        match bits & 0x03 {
            0 => Self::Ink,
            1 => Self::Charcoal,
            2 => Self::Silver,
            _ => Self::Paper,
        }
    }

    #[inline]
    pub const fn is_dark(self) -> bool {
        matches!(self, Self::Ink | Self::Charcoal)
    }
}

/// E-Ink refresh waveforms available across UC8253 / UC8279d / host emulator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshMode {
    /// Fast differential B/W page turn (~200-350ms, minimal flash).
    FastDu,
    /// Medium scrub B/W refresh (clears subtle ghosting without full invert).
    HalfScrub,
    /// Full optical GC reset (black/white flash, zeroes ghosting).
    FullGc,
    /// 4-tone grayscale showcase refresh (Home Desk, Covers, Games, Lock Posters).
    Grayscale4,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Up,
    Down,
    Left,
    Right,
    Confirm,
    Back,
    Power,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputEvent {
    Press(Button),
    Release(Button),
    LongPress(Button),
    TiltFlickNext,
    TiltFlickPrev,
    OrientationFlip(bool),
}

/// Errors returned by PAL storage operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageError {
    NotFound,
    Io,
    AlreadyExists,
    InvalidPath,
    OutOfSpace,
    Corrupted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

/// Display sink decoupled from canvas rendering so the exact same UI renders into
/// a full 2-bpp buffer on host OR 64-row strips / 1-bpp SRAM buffer on ESP32-C3.
pub trait DisplaySink {
    /// Present a full 2-bpp portrait strip (`y_start..y_start+rows`, 132 bytes/row).
    /// `first_strip` / `last_strip` bracket a complete frame refresh.
    fn begin_frame(&mut self, mode: RefreshMode);
    fn write_strip_2bpp(&mut self, y_start: usize, rows: usize, packed_2bpp: &[u8]);
    fn end_frame(&mut self, mode: RefreshMode);

    /// Optional fast path for 1-bpp page turns when the host/driver keeps a 1-bpp buffer.
    fn present_1bpp(&mut self, _mode: RefreshMode, _packed_1bpp: &[u8]) -> bool {
        false
    }
}

/// Persistent block/file storage (host directory or 16 GB FAT32 microSD card).
pub trait StorageBackend {
    fn exists(&self, path: &str) -> bool;
    fn file_len(&self, path: &str) -> Result<u64, StorageError>;
    fn read_all(&self, path: &str) -> Result<Vec<u8>, StorageError>;
    fn read_range(&self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize, StorageError>;
    fn write_all(&mut self, path: &str, data: &[u8]) -> Result<(), StorageError>;
    fn write_atomic(&mut self, path: &str, data: &[u8]) -> Result<(), StorageError>;
    fn append(&mut self, path: &str, data: &[u8]) -> Result<(), StorageError>;
    fn remove(&mut self, path: &str) -> Result<(), StorageError>;
    fn mkdir_all(&mut self, path: &str) -> Result<(), StorageError>;
    fn list_dir(&self, path: &str) -> Result<Vec<DirEntry>, StorageError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BatteryStatus {
    pub percent: u8,
    pub millivolts: u16,
    pub current_ma: i16,
    pub charging: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DateTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

pub trait DeviceSensors {
    fn battery(&mut self) -> BatteryStatus;
    fn now(&mut self) -> DateTime;
    fn uptime_ms(&self) -> u64;
    fn temperature_c_x10(&mut self) -> i16;

    /// Optionally persist wall-clock date/time into the hardware RTC (DS3231).
    fn set_datetime(&mut self, _dt: DateTime) -> bool {
        false
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortalStatus {
    pub active: bool,
    pub ssid: String,
    pub url: String,
    pub pin: String,
    pub clients_connected: u8,
    pub rx_bytes: u64,
    pub total_bytes: u64,
    pub last_received_file: Option<String>,
    pub busy_message: Option<String>,
}

pub trait NetworkPortal {
    fn start(&mut self, pin: &str);
    fn stop(&mut self);
    fn status(&mut self) -> PortalStatus;
    /// Take any newly uploaded raw filename (in `/books/` or staging) waiting for ingestion.
    fn take_pending_ingest(&mut self) -> Option<(String, Vec<u8>)>;

    /// Service non-blocking SoftAP / DHCP / DNS / streaming HTTP I/O onto `storage`.
    /// Returns `true` when visible status changed and the E-Ink screen should refresh.
    fn poll_io(&mut self, _storage: &mut dyn StorageBackend, _now_ms: u64) -> bool {
        false
    }

    /// Take the storage path (e.g. `/books/novel.epub`) of a file fully streamed to SD.
    fn take_uploaded_path(&mut self) -> Option<String> {
        None
    }

    /// Take an updated wall-clock timestamp received from a paired client browser.
    fn take_synced_datetime(&mut self) -> Option<DateTime> {
        None
    }
}

pub trait PowerControl {
    fn enter_deep_sleep(&mut self);
    fn set_imu_flick(&mut self, enabled: bool);
    fn set_rotated_180(&mut self, rotated: bool);
    fn controller_name(&self) -> &'static str;

    /// Drop high-voltage EPD charge pump (`POF 0x02`) after ~2.5s static display hold.
    fn set_panel_hv_standby(&mut self, _standby: bool) {}

    /// Gate microSD power rail (`GPIO 13 = LOW`) when no block read/write is pending.
    fn set_sd_power_save(&mut self, _power_save: bool) {}

    /// Allow RISC-V `wfi` / Embassy light sleep while awaiting the next input or timer.
    fn enter_light_idle(&mut self, _max_ms: u32) {}
}

pub trait EntropySource {
    fn fill_bytes(&mut self, out: &mut [u8]);
}
