use std::cell::{Cell, RefCell};
use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::rc::Rc;

use vvbook_format::InputFormat;
use vvbook_pal::{
    BatteryStatus, Button, DateTime, DeviceSensors, DisplaySink, EntropySource, InputEvent,
    NetworkPortal, PORTRAIT_H, PORTRAIT_W, PortalStatus, PowerControl, ROW_2BPP_BYTES, RefreshMode,
    StorageBackend, StorageError,
};
use vvbook_userspace::{FolioApp, ReaderOverlay, Screen};
use vvbook_x3_hal::epd::{X3EpdPlanes, portrait_to_native};
use vvbook_x3_hal::net_portal::{AP_SSID, AP_URL, StreamingHttpConn, VvDropServerState};
use vvbook_x3_hal::pins::{BQ27220_I2C_ADDR, DS3231_I2C_ADDR, QMI8658_I2C_ADDR};
use vvbook_x3_hal::sd_fat32::{BlockDevice, Fat32Volume, format_fat32};
use vvbook_x3_hal::{
    AdcButtonDecoder, Bq27220, DelayUs, Ds3231, EpdPins, HalError, I2cBus, Qmi8658, SpiBus,
    X3EpdDriver,
};

use crate::{build_zip_bytes, build_zip_bytes_raw, encode_cp1251};

const SECTOR_SIZE: usize = 512;
const DEFAULT_SD_SECTORS: u32 = 131_072; // 64 MiB FAT32 microSD image
const MAX_READ_ALL_BYTES: u64 = 160 * 1024; // Exact mirror of firmware Fat32Handle::read_all limit
const FRAME_HISTORY_CAP: usize = 64;

/// Pre-allocated raw 512-byte sector storage for the simulated SDHC card.
/// Allocated on host RAM outside the ESP32-C3 320 KB SRAM arena guard.
pub struct PrebuiltSdImage {
    pub sectors: Vec<[u8; SECTOR_SIZE]>,
}

impl PrebuiltSdImage {
    pub fn empty(sector_count: u32) -> Self {
        Self {
            sectors: vec![[0u8; SECTOR_SIZE]; sector_count as usize],
        }
    }

    pub fn sector_count(&self) -> u32 {
        self.sectors.len() as u32
    }
}

/// Shared cycle-, rail-, and bus-level hardware state of the simulated Xteink X3 board.
#[derive(Clone, Debug)]
pub struct SimBoardState {
    pub sd_power_en: bool,
    pub sd_power_save_hint: bool,
    pub deep_sleeping: bool,
    pub panel_hv_standby: bool,
    pub rotated_180: bool,
    pub sd_rail_faults: u32,
    pub oversize_read_all_rejections: u32,
    pub uptime_us: u64,
    pub spi_sd_us: u64,
    pub spi_epd_us: u64,
    pub epd_busy_us: u64,
    pub sd_sectors_read: u64,
    pub sd_sectors_written: u64,
}

impl SimBoardState {
    fn new() -> Self {
        Self {
            sd_power_en: true,
            sd_power_save_hint: true,
            deep_sleeping: false,
            panel_hv_standby: false,
            rotated_180: false,
            sd_rail_faults: 0,
            oversize_read_all_rejections: 0,
            uptime_us: 0,
            spi_sd_us: 0,
            spi_epd_us: 0,
            epd_busy_us: 0,
            sd_sectors_read: 0,
            sd_sectors_written: 0,
        }
    }
}

/// Snapshot of simulated Xteink X3 hardware telemetry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SimHwStats {
    pub sd_power_en: bool,
    pub deep_sleeping: bool,
    pub panel_hv_standby: bool,
    pub rotated_180: bool,
    pub sd_rail_faults: u32,
    pub oversize_read_all_rejections: u32,
    pub uptime_us: u64,
    pub spi_sd_us: u64,
    pub spi_epd_us: u64,
    pub epd_busy_us: u64,
    pub sd_sectors_read: u64,
    pub sd_sectors_written: u64,
}

impl From<&SimBoardState> for SimHwStats {
    fn from(s: &SimBoardState) -> Self {
        Self {
            sd_power_en: s.sd_power_en,
            deep_sleeping: s.deep_sleeping,
            panel_hv_standby: s.panel_hv_standby,
            rotated_180: s.rotated_180,
            sd_rail_faults: s.sd_rail_faults,
            oversize_read_all_rejections: s.oversize_read_all_rejections,
            uptime_us: s.uptime_us,
            spi_sd_us: s.spi_sd_us,
            spi_epd_us: s.spi_epd_us,
            epd_busy_us: s.epd_busy_us,
            sd_sectors_read: s.sd_sectors_read,
            sd_sectors_written: s.sd_sectors_written,
        }
    }
}

/// Simulated SDHC block device backing `Fat32Volume` with SPI timing & `GPIO 13` rail checks.
pub struct SimSdhcCard {
    image: Rc<RefCell<PrebuiltSdImage>>,
    board: Rc<RefCell<SimBoardState>>,
}

impl SimSdhcCard {
    fn new(image: Rc<RefCell<PrebuiltSdImage>>, board: Rc<RefCell<SimBoardState>>) -> Self {
        Self { image, board }
    }
}

impl BlockDevice for SimSdhcCard {
    fn read_sector(&mut self, lba: u32, buf: &mut [u8; SECTOR_SIZE]) -> Result<(), HalError> {
        {
            let mut b = self.board.borrow_mut();
            if !b.sd_power_en {
                b.sd_rail_faults = b.sd_rail_faults.saturating_add(1);
                return Err(HalError::Bus);
            }
            b.spi_sd_us = b.spi_sd_us.saturating_add(320);
            b.uptime_us = b.uptime_us.saturating_add(320);
            b.sd_sectors_read = b.sd_sectors_read.saturating_add(1);
        }
        let img = self.image.borrow();
        let idx = lba as usize;
        if idx >= img.sectors.len() {
            return Err(HalError::Bus);
        }
        buf.copy_from_slice(&img.sectors[idx]);
        Ok(())
    }

    fn write_sector(&mut self, lba: u32, data: &[u8; SECTOR_SIZE]) -> Result<(), HalError> {
        {
            let mut b = self.board.borrow_mut();
            if !b.sd_power_en {
                b.sd_rail_faults = b.sd_rail_faults.saturating_add(1);
                return Err(HalError::Bus);
            }
            b.spi_sd_us = b.spi_sd_us.saturating_add(850);
            b.uptime_us = b.uptime_us.saturating_add(850);
            b.sd_sectors_written = b.sd_sectors_written.saturating_add(1);
        }
        let mut img = self.image.borrow_mut();
        let idx = lba as usize;
        if idx >= img.sectors.len() {
            return Err(HalError::WriteFault);
        }
        img.sectors[idx].copy_from_slice(data);
        Ok(())
    }

    fn sector_count(&self) -> u32 {
        self.image.borrow().sector_count()
    }

    fn set_power_save(&mut self, _save: bool) {
        // Real hardware keeps GPIO 13 awake during normal OS operation.
    }
}

/// Exact firmware `Fat32Handle` mirror enforcing `MAX_READ_ALL_BYTES = 160 KB`.
pub struct X3TwinFat32Storage {
    vol: Rc<RefCell<Fat32Volume<SimSdhcCard>>>,
    board: Rc<RefCell<SimBoardState>>,
}

impl StorageBackend for X3TwinFat32Storage {
    fn exists(&self, path: &str) -> bool {
        self.vol.borrow().exists(path)
    }

    fn file_len(&self, path: &str) -> Result<u64, StorageError> {
        self.vol.borrow().file_len(path)
    }

    fn read_all(&self, path: &str) -> Result<Vec<u8>, StorageError> {
        let len = self.vol.borrow().file_len(path)?;
        if len > MAX_READ_ALL_BYTES {
            let mut b = self.board.borrow_mut();
            b.oversize_read_all_rejections = b.oversize_read_all_rejections.saturating_add(1);
            return Err(StorageError::OutOfSpace);
        }
        self.vol.borrow().read_all(path)
    }

    fn read_range(&self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize, StorageError> {
        self.vol.borrow().read_range(path, offset, buf)
    }

    fn write_all(&mut self, path: &str, data: &[u8]) -> Result<(), StorageError> {
        self.vol.borrow_mut().write_all(path, data)
    }

    fn write_atomic(&mut self, path: &str, data: &[u8]) -> Result<(), StorageError> {
        self.vol.borrow_mut().write_atomic(path, data)
    }

    fn append(&mut self, path: &str, data: &[u8]) -> Result<(), StorageError> {
        self.vol.borrow_mut().append(path, data)
    }

    fn remove(&mut self, path: &str) -> Result<(), StorageError> {
        self.vol.borrow_mut().remove(path)
    }

    fn mkdir_all(&mut self, path: &str) -> Result<(), StorageError> {
        self.vol.borrow_mut().mkdir_all(path)
    }

    fn list_dir(&self, path: &str) -> Result<Vec<vvbook_pal::DirEntry>, StorageError> {
        self.vol.borrow().list_dir(path)
    }
}

/// Fixed-size ESP32-C3 USB-Serial/JTAG EP1 64-byte FIFO model (zero heap growth).
pub struct SimUsbSerialJtag {
    ep1_fifo: [u8; 64],
    fifo_len: usize,
    rx_ring: [u8; 1024],
    rx_head: usize,
    rx_len: usize,
    cable_connected: bool,
    usb_host_online: bool,
    transmitted_bytes: u64,
    dropped_bytes: u64,
    timeout_stalls: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SimUsbStats {
    pub cable_connected: bool,
    pub host_online: bool,
    pub transmitted_bytes: u64,
    pub dropped_bytes: u64,
    pub timeout_stalls: u32,
    pub fifo_used: usize,
}

impl SimUsbSerialJtag {
    fn new() -> Self {
        Self {
            ep1_fifo: [0u8; 64],
            fifo_len: 0,
            rx_ring: [0u8; 1024],
            rx_head: 0,
            rx_len: 0,
            cable_connected: true,
            usb_host_online: true,
            transmitted_bytes: 0,
            dropped_bytes: 0,
            timeout_stalls: 0,
        }
    }

    fn push_rx_byte(&mut self, b: u8) {
        let idx = (self.rx_head + self.rx_len) % self.rx_ring.len();
        self.rx_ring[idx] = b;
        if self.rx_len < self.rx_ring.len() {
            self.rx_len += 1;
        } else {
            self.rx_head = (self.rx_head + 1) % self.rx_ring.len();
        }
    }

    fn drain_fifo_to_host(&mut self) {
        if !self.cable_connected || self.fifo_len == 0 {
            return;
        }
        for i in 0..self.fifo_len {
            self.push_rx_byte(self.ep1_fifo[i]);
        }
        self.transmitted_bytes = self.transmitted_bytes.saturating_add(self.fifo_len as u64);
        self.fifo_len = 0;
    }

    pub fn set_cable_connected(&mut self, connected: bool) {
        self.cable_connected = connected;
        if connected {
            self.drain_fifo_to_host();
        }
    }

    fn write_byte(&mut self, b: u8, board: &mut SimBoardState) {
        let ep1_free = self.fifo_len < 64;
        if ep1_free {
            self.usb_host_online = true;
            self.ep1_fifo[self.fifo_len] = b;
            self.fifo_len += 1;
            if b == b'\n' || self.fifo_len == 64 {
                self.drain_fifo_to_host();
            }
            return;
        }
        if !self.usb_host_online {
            self.dropped_bytes = self.dropped_bytes.saturating_add(1);
            return;
        }
        // Firmware waits up to 1.5 ms once before declaring the host disconnected.
        board.uptime_us = board.uptime_us.saturating_add(1_500);
        self.timeout_stalls = self.timeout_stalls.saturating_add(1);
        self.usb_host_online = false;
        self.dropped_bytes = self.dropped_bytes.saturating_add(1);
    }

    pub fn write_line(&mut self, s: &str, board: &mut SimBoardState) {
        for &b in s.as_bytes() {
            if b == b'\n' {
                self.write_byte(b'\r', board);
            }
            self.write_byte(b, board);
        }
        if self.usb_host_online && self.fifo_len < 64 {
            self.drain_fifo_to_host();
        }
    }

    pub fn stats(&self) -> SimUsbStats {
        SimUsbStats {
            cable_connected: self.cable_connected,
            host_online: self.usb_host_online,
            transmitted_bytes: self.transmitted_bytes,
            dropped_bytes: self.dropped_bytes,
            timeout_stalls: self.timeout_stalls,
            fifo_used: self.fifo_len,
        }
    }
}

/// Simulated microsecond delay advancing virtual board time without sleeping host threads.
pub struct SimDelay {
    board: Rc<RefCell<SimBoardState>>,
}

impl DelayUs for SimDelay {
    fn delay_us(&mut self, us: u32) {
        let mut b = self.board.borrow_mut();
        b.uptime_us = b.uptime_us.saturating_add(u64::from(us));
    }
}

struct SimEpdPins {
    cs_high: bool,
    dc_data: bool,
    rst_high: bool,
    busy_polls: Cell<u8>,
    next_mode_us: Cell<u64>,
    last_mode: Cell<RefreshMode>,
}

impl SimEpdPins {
    fn new() -> Self {
        Self {
            cs_high: true,
            dc_data: true,
            rst_high: true,
            busy_polls: Cell::new(0),
            next_mode_us: Cell::new(260_000),
            last_mode: Cell::new(RefreshMode::FullGc),
        }
    }
}

struct SimEpdSpi {
    board: Rc<RefCell<SimBoardState>>,
    pins: Rc<RefCell<SimEpdPins>>,
}

#[derive(Clone)]
struct SimEpdPinsHandle(Rc<RefCell<SimEpdPins>>);

impl EpdPins for SimEpdPinsHandle {
    fn set_cs(&mut self, high: bool) {
        self.0.borrow_mut().cs_high = high;
    }

    fn set_dc(&mut self, data_mode: bool) {
        self.0.borrow_mut().dc_data = data_mode;
    }

    fn set_rst(&mut self, high: bool) {
        self.0.borrow_mut().rst_high = high;
    }

    fn is_busy(&self) -> bool {
        let p = self.0.borrow();
        let left = p.busy_polls.get();
        if left > 0 {
            p.busy_polls.set(left - 1);
            true
        } else {
            false
        }
    }

    fn read_0x70_3wire(&mut self) -> [u8; 3] {
        [0x00, 0x00, 0x00] // EpdSilicon::Uc8253
    }
}

impl SpiBus for SimEpdSpi {
    fn set_frequency_hz(&mut self, _hz: u32) {}

    fn write(&mut self, data: &[u8]) -> Result<(), HalError> {
        let mut b = self.board.borrow_mut();
        if !b.sd_power_en {
            // Shared SPI2 bus: dropping SD_POWER_EN clamps shared SPI2 transactions when unplugged.
            b.sd_rail_faults = b.sd_rail_faults.saturating_add(1);
            return Err(HalError::Bus);
        }
        let xfer_us = (data.len() as u64).saturating_add(1) / 2;
        b.spi_epd_us = b.spi_epd_us.saturating_add(xfer_us);
        b.uptime_us = b.uptime_us.saturating_add(xfer_us);

        let pins = self.pins.borrow();
        if !pins.dc_data && !pins.cs_high && !data.is_empty() {
            match data[0] {
                0x12 => {
                    pins.busy_polls.set(2);
                    let settle_us = pins.next_mode_us.get();
                    b.epd_busy_us = b.epd_busy_us.saturating_add(settle_us);
                    b.uptime_us = b.uptime_us.saturating_add(settle_us);
                }
                0x04 | 0x02 => {
                    pins.busy_polls.set(1);
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn transfer(&mut self, tx: &[u8], rx: &mut [u8]) -> Result<(), HalError> {
        self.write(tx)?;
        rx.fill(0xFF);
        Ok(())
    }
}

type TwinEpd = X3EpdDriver<SimEpdSpi, SimEpdPinsHandle, SimDelay>;

/// DisplaySink bridging `FolioApp` directly to the real `X3EpdDriver` and frame ring buffer.
pub struct X3TwinEpdSink {
    epd: Rc<RefCell<TwinEpd>>,
    pins: Rc<RefCell<SimEpdPins>>,
    usb: Rc<RefCell<SimUsbSerialJtag>>,
    board: Rc<RefCell<SimBoardState>>,
    frame_ring: [RefreshMode; FRAME_HISTORY_CAP],
    frame_count: usize,
}

impl X3TwinEpdSink {
    fn mode_busy_us(mode: RefreshMode) -> u64 {
        match mode {
            RefreshMode::FastDu => 260_000,
            RefreshMode::HalfScrub => 480_000,
            RefreshMode::FullGc => 950_000,
            RefreshMode::Grayscale4 => 720_000,
        }
    }

    fn decode_panel_2bpp(&self) -> Vec<u8> {
        let last_mode = self.pins.borrow().last_mode.get();
        let gs4 = matches!(last_mode, RefreshMode::Grayscale4);
        let epd = self.epd.borrow();
        let planes = epd.planes();
        let mut out = vec![0xFFu8; PORTRAIT_H * ROW_2BPP_BYTES];
        for py in 0..PORTRAIT_H {
            for bx in 0..ROW_2BPP_BYTES {
                let mut packed = 0u8;
                let base_px = bx * 4;
                for sub in 0..4 {
                    let (nx, ny) = portrait_to_native(base_px + sub, py, false);
                    let bits = if gs4 {
                        let lsb_plane = planes.gray_lsb_plane.as_deref().unwrap_or(&planes.new_plane);
                        let lsb = X3EpdPlanes::get_native_bit(lsb_plane, nx, ny) as u8;
                        let msb = X3EpdPlanes::get_native_bit(&planes.new_plane, nx, ny) as u8;
                        (msb << 1) | lsb
                    } else if X3EpdPlanes::get_native_bit(&planes.new_plane, nx, ny) {
                        3
                    } else {
                        0
                    };
                    packed |= (bits & 0x03) << (6 - sub * 2);
                }
                out[py * ROW_2BPP_BYTES + bx] = packed;
            }
        }
        out
    }

    fn count_dark_panel_pixels(&self) -> usize {
        let last_mode = self.pins.borrow().last_mode.get();
        let gs4 = matches!(last_mode, RefreshMode::Grayscale4);
        let epd = self.epd.borrow();
        let planes = epd.planes();
        let lsb_plane = planes.gray_lsb_plane.as_deref().unwrap_or(&planes.new_plane);
        let mut dark = 0usize;
        for ny in 0..PORTRAIT_W {
            for nx in 0..PORTRAIT_H {
                let msb_white = X3EpdPlanes::get_native_bit(&planes.new_plane, nx, ny);
                let lsb_white = !gs4 || X3EpdPlanes::get_native_bit(lsb_plane, nx, ny);
                if !msb_white || !lsb_white {
                    dark += 1;
                }
            }
        }
        dark
    }
}

impl DisplaySink for X3TwinEpdSink {
    fn begin_frame(&mut self, mode: RefreshMode) {
        let p = self.pins.borrow();
        p.next_mode_us.set(Self::mode_busy_us(mode));
        p.last_mode.set(mode);
        drop(p);
        self.epd.borrow_mut().begin_frame(mode);
    }

    fn write_strip_2bpp(&mut self, y_start: usize, rows: usize, packed: &[u8]) {
        self.epd.borrow_mut().write_strip_2bpp(y_start, rows, packed);
    }

    fn end_frame(&mut self, mode: RefreshMode) {
        self.epd.borrow_mut().end_frame(mode);
        let idx = self.frame_count % FRAME_HISTORY_CAP;
        self.frame_ring[idx] = mode;
        self.frame_count = self.frame_count.saturating_add(1);

        let mut msg = [0u8; 64];
        let mut w = StackBuf {
            buf: &mut msg,
            len: 0,
        };
        let _ = writeln!(w, "[VVBOOK-OS] FRAME mode={mode:?} n={}", self.frame_count);
        let text = core::str::from_utf8(&w.buf[..w.len]).unwrap_or("");
        let mut usb = self.usb.borrow_mut();
        let mut board = self.board.borrow_mut();
        usb.write_line(text, &mut board);
    }

    fn present_1bpp(&mut self, mode: RefreshMode, packed_1bpp: &[u8]) -> bool {
        let ok = self.epd.borrow_mut().present_1bpp(mode, packed_1bpp);
        if ok {
            let idx = self.frame_count % FRAME_HISTORY_CAP;
            self.frame_ring[idx] = mode;
            self.frame_count = self.frame_count.saturating_add(1);
        }
        ok
    }
}

struct StackBuf<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl core::fmt::Write for StackBuf<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        let avail = self.buf.len().saturating_sub(self.len);
        let n = bytes.len().min(avail);
        self.buf[self.len..self.len + n].copy_from_slice(&bytes[..n]);
        self.len += n;
        Ok(())
    }
}

/// Simulated I2C bus implementing BQ27220 (`0x55`), DS3231 (`0x68`), and QMI8658 (`0x6B`).
#[derive(Clone)]
pub struct SimI2cBus {
    board: Rc<RefCell<SimBoardState>>,
    battery_mv: u16,
    battery_ma: i16,
    battery_soc: u16,
    temp_dk: u16,
    clock: DateTime,
    clock_base_uptime_us: u64,
    qmi_ctrl1: u8,
    qmi_ctrl2: u8,
    qmi_ctrl7: u8,
}

impl SimI2cBus {
    fn new(board: Rc<RefCell<SimBoardState>>) -> Self {
        Self {
            board,
            battery_mv: 4120,
            battery_ma: -24,
            battery_soc: 88,
            temp_dk: 2966, // 23.5 C
            clock: DateTime {
                year: 2026,
                month: 11,
                day: 12,
                hour: 22,
                minute: 10,
                second: 0,
            },
            clock_base_uptime_us: 0,
            qmi_ctrl1: 0x41,
            qmi_ctrl2: 0x25,
            qmi_ctrl7: 0x00,
        }
    }
}

impl I2cBus for SimI2cBus {
    fn write(&mut self, addr: u8, bytes: &[u8]) -> Result<(), HalError> {
        let mut b = self.board.borrow_mut();
        b.uptime_us = b.uptime_us.saturating_add(120);
        if addr == QMI8658_I2C_ADDR && bytes.len() >= 2 {
            match bytes[0] {
                0x02 => self.qmi_ctrl1 = bytes[1],
                0x03 => self.qmi_ctrl2 = bytes[1],
                0x08 => self.qmi_ctrl7 = bytes[1],
                _ => {}
            }
        } else if addr == DS3231_I2C_ADDR && bytes.len() >= 8 && bytes[0] == 0x00 {
            self.clock = DateTime {
                second: Ds3231::bcd_to_bin(bytes[1] & 0x7F).min(59),
                minute: Ds3231::bcd_to_bin(bytes[2] & 0x7F).min(59),
                hour: Ds3231::bcd_to_bin(bytes[3] & 0x3F).min(23),
                day: Ds3231::bcd_to_bin(bytes[5] & 0x3F).clamp(1, 31),
                month: Ds3231::bcd_to_bin(bytes[6] & 0x1F).clamp(1, 12),
                year: 2000 + u16::from(Ds3231::bcd_to_bin(bytes[7])),
            };
            self.clock_base_uptime_us = b.uptime_us;
        }
        Ok(())
    }

    fn write_read(&mut self, addr: u8, write: &[u8], read: &mut [u8]) -> Result<(), HalError> {
        {
            let mut b = self.board.borrow_mut();
            b.uptime_us = b.uptime_us.saturating_add(150);
        }
        let reg = write.first().copied().unwrap_or(0);
        match addr {
            BQ27220_I2C_ADDR => {
                let val = match reg {
                    0x06 => self.temp_dk,
                    0x08 => self.battery_mv,
                    0x0C => self.battery_ma as u16,
                    0x2C => self.battery_soc,
                    _ => 0,
                };
                let le = val.to_le_bytes();
                for (i, dst) in read.iter_mut().enumerate() {
                    *dst = *le.get(i).unwrap_or(&0);
                }
                Ok(())
            }
            DS3231_I2C_ADDR => {
                let elapsed_s = self
                    .board
                    .borrow()
                    .uptime_us
                    .saturating_sub(self.clock_base_uptime_us)
                    / 1_000_000;
                let secs = self.clock.second as u32 + elapsed_s as u32;
                let total_mins = self.clock.minute as u32 + (secs / 60);
                let cur_sec = (secs % 60) as u8;
                let cur_min = (total_mins % 60) as u8;
                let cur_hr = ((self.clock.hour as u32 + (total_mins / 60)) % 24) as u8;
                let yr = self.clock.year.saturating_sub(2000).min(99) as u8;
                let regs = [
                    Ds3231::bin_to_bcd(cur_sec),
                    Ds3231::bin_to_bcd(cur_min),
                    Ds3231::bin_to_bcd(cur_hr),
                    4,
                    Ds3231::bin_to_bcd(self.clock.day),
                    Ds3231::bin_to_bcd(self.clock.month),
                    Ds3231::bin_to_bcd(yr),
                ];
                let start = reg as usize;
                for (i, dst) in read.iter_mut().enumerate() {
                    *dst = *regs.get(start + i).unwrap_or(&0);
                }
                Ok(())
            }
            QMI8658_I2C_ADDR => {
                read.fill(0);
                Ok(())
            }
            _ => Err(HalError::Bus),
        }
    }
}

/// Hardware sensor suite driving real `Bq27220` & `Ds3231` over `SimI2cBus`.
pub struct X3TwinSensors {
    i2c: Rc<RefCell<SimI2cBus>>,
    bq: Bq27220<SimDelay>,
    board: Rc<RefCell<SimBoardState>>,
}

impl DeviceSensors for X3TwinSensors {
    fn battery(&mut self) -> BatteryStatus {
        let mut i2c = self.i2c.borrow_mut();
        self.bq.read_battery(&mut *i2c).unwrap_or(BatteryStatus {
            percent: 88,
            millivolts: 4120,
            current_ma: -24,
            charging: false,
        })
    }

    fn now(&mut self) -> DateTime {
        let mut i2c = self.i2c.borrow_mut();
        Ds3231::read_datetime(&mut *i2c).unwrap_or(i2c.clock)
    }

    fn set_datetime(&mut self, dt: DateTime) -> bool {
        let mut i2c = self.i2c.borrow_mut();
        Ds3231::write_datetime(&mut *i2c, dt).is_ok()
    }

    fn uptime_ms(&self) -> u64 {
        self.board.borrow().uptime_us / 1_000
    }

    fn temperature_c_x10(&mut self) -> i16 {
        let mut i2c = self.i2c.borrow_mut();
        self.bq.read_temperature_c_x10(&mut *i2c).unwrap_or(235)
    }
}

/// Hardware power controller driving real `X3EpdDriver`, `Fat32Volume`, and `Qmi8658`.
pub struct X3TwinPowerControl {
    epd: Rc<RefCell<TwinEpd>>,
    sd_vol: Rc<RefCell<Fat32Volume<SimSdhcCard>>>,
    imu: Qmi8658,
    i2c: Rc<RefCell<SimI2cBus>>,
    board: Rc<RefCell<SimBoardState>>,
    ctrl_name: &'static str,
}

impl PowerControl for X3TwinPowerControl {
    fn enter_deep_sleep(&mut self) {
        let _ = self.epd.borrow_mut().enter_deep_sleep();
        self.sd_vol.borrow_mut().set_power_save(true);
        let mut i2c = self.i2c.borrow_mut();
        let _ = self.imu.set_enabled(&mut *i2c, false);
        let mut b = self.board.borrow_mut();
        b.sd_power_en = false;
        b.deep_sleeping = true;
    }

    fn set_imu_flick(&mut self, enabled: bool) {
        let mut i2c = self.i2c.borrow_mut();
        let _ = self.imu.set_enabled(&mut *i2c, enabled);
    }

    fn set_rotated_180(&mut self, rotated: bool) {
        self.epd.borrow_mut().set_rotated_180(rotated);
        self.board.borrow_mut().rotated_180 = rotated;
    }

    fn controller_name(&self) -> &'static str {
        self.ctrl_name
    }

    fn set_panel_hv_standby(&mut self, standby: bool) {
        let _ = self.epd.borrow_mut().set_hv_standby(standby);
        self.board.borrow_mut().panel_hv_standby = standby;
    }

    fn set_sd_power_save(&mut self, power_save: bool) {
        // Keep GPIO 13 continuous while awake to protect shared SPI2 EPD + microSD bus.
        self.board.borrow_mut().sd_power_save_hint = power_save;
    }

    fn enter_light_idle(&mut self, max_ms: u32) {
        let mut b = self.board.borrow_mut();
        b.uptime_us = b.uptime_us.saturating_add(u64::from(max_ms.min(2)) * 1_000);
    }
}

pub struct SimPortal {
    state: VvDropServerState,
    conn: StreamingHttpConn,
}

impl SimPortal {
    fn new() -> Self {
        Self {
            state: VvDropServerState::new([0u8; 32], 1, 0x5656_464F),
            conn: StreamingHttpConn::new(false),
        }
    }
}

impl NetworkPortal for SimPortal {
    fn start(&mut self, pin: &str) {
        self.state.start(pin, 1);
        self.conn.reset();
    }

    fn stop(&mut self) {
        self.state.stop();
        self.conn.reset();
    }

    fn status(&mut self) -> PortalStatus {
        PortalStatus {
            active: self.state.active,
            ssid: String::from(AP_SSID),
            url: String::from(AP_URL),
            pin: String::from(core::str::from_utf8(&self.state.pin).unwrap_or("00000000")),
            clients_connected: self.state.clients_connected,
            rx_bytes: self.state.rx_bytes,
            total_bytes: self.state.total_bytes,
            last_received_file: self.state.last_received_file.clone(),
            busy_message: self.state.busy_message.clone(),
        }
    }

    fn take_pending_ingest(&mut self) -> Option<(String, Vec<u8>)> {
        None
    }

    fn take_uploaded_path(&mut self) -> Option<String> {
        if self.state.completed_uploads.is_empty() {
            None
        } else {
            Some(self.state.completed_uploads.remove(0))
        }
    }

    fn take_synced_datetime(&mut self) -> Option<DateTime> {
        self.state.synced_datetime.take()
    }
}

pub struct SimEntropy(u64);

impl EntropySource for SimEntropy {
    fn fill_bytes(&mut self, out: &mut [u8]) {
        for b in out.iter_mut() {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            *b = (self.0 >> 32) as u8;
        }
    }
}

/// Complete cycle-, sector-, and byte-faithful hardware twin of the Xteink X3 (ESP32-C3).
pub struct X3HardwareTwin {
    pub app: FolioApp,
    sd_backing: Rc<RefCell<PrebuiltSdImage>>,
    board: Rc<RefCell<SimBoardState>>,
    usb: Rc<RefCell<SimUsbSerialJtag>>,
    storage: X3TwinFat32Storage,
    epd_sink: X3TwinEpdSink,
    sensors: X3TwinSensors,
    power: X3TwinPowerControl,
    portal: SimPortal,
    entropy: SimEntropy,
    buttons: AdcButtonDecoder,
    event_scratch: Vec<InputEvent>,
}

impl X3HardwareTwin {
    pub fn from_sd_image(image: PrebuiltSdImage) -> Self {
        let board = Rc::new(RefCell::new(SimBoardState::new()));
        let sd_backing = Rc::new(RefCell::new(image));
        let card = SimSdhcCard::new(Rc::clone(&sd_backing), Rc::clone(&board));
        let vol = Fat32Volume::mount(card).expect("FAT32 volume mount on SimSdhcCard");
        let sd_vol = Rc::new(RefCell::new(vol));

        let pins = Rc::new(RefCell::new(SimEpdPins::new()));
        let spi = SimEpdSpi {
            board: Rc::clone(&board),
            pins: Rc::clone(&pins),
        };
        let epd_drv = X3EpdDriver::new(
            spi,
            SimEpdPinsHandle(Rc::clone(&pins)),
            SimDelay {
                board: Rc::clone(&board),
            },
        )
        .expect("UC8253 EPD init on X3HardwareTwin");
        let ctrl_name = epd_drv.silicon().name();
        let epd = Rc::new(RefCell::new(epd_drv));
        let usb = Rc::new(RefCell::new(SimUsbSerialJtag::new()));
        let i2c = Rc::new(RefCell::new(SimI2cBus::new(Rc::clone(&board))));

        let storage = X3TwinFat32Storage {
            vol: Rc::clone(&sd_vol),
            board: Rc::clone(&board),
        };
        let epd_sink = X3TwinEpdSink {
            epd: Rc::clone(&epd),
            pins: Rc::clone(&pins),
            usb: Rc::clone(&usb),
            board: Rc::clone(&board),
            frame_ring: [RefreshMode::FullGc; FRAME_HISTORY_CAP],
            frame_count: 0,
        };
        let sensors = X3TwinSensors {
            i2c: Rc::clone(&i2c),
            bq: Bq27220::new(SimDelay {
                board: Rc::clone(&board),
            }),
            board: Rc::clone(&board),
        };
        let power = X3TwinPowerControl {
            epd: Rc::clone(&epd),
            sd_vol,
            imu: Qmi8658::new(),
            i2c,
            board: Rc::clone(&board),
            ctrl_name,
        };

        Self {
            app: FolioApp::new(),
            sd_backing,
            board,
            usb,
            storage,
            epd_sink,
            sensors,
            power,
            portal: SimPortal::new(),
            entropy: SimEntropy(0x5854_4549_4E4B_5833),
            buttons: AdcButtonDecoder::new(),
            event_scratch: Vec::with_capacity(4),
        }
    }

    /// Consumes the twin, dropping firmware state inside any active heap guard while returning the
    /// raw 64 MiB flash vector back to host scope.
    pub fn into_sd_image(self) -> PrebuiltSdImage {
        let sectors = std::mem::take(&mut self.sd_backing.borrow_mut().sectors);
        drop(self);
        PrebuiltSdImage { sectors }
    }

    pub fn boot(&mut self) {
        self.app.boot(
            &mut self.epd_sink,
            &mut self.storage,
            &mut self.sensors,
            &mut self.power,
            &mut self.portal,
            &mut self.entropy,
        );
        self.log_usb("[VVBOOK-OS] BOOT READY\n");
    }

    fn log_usb(&self, msg: &str) {
        let mut usb = self.usb.borrow_mut();
        let mut board = self.board.borrow_mut();
        usb.write_line(msg, &mut board);
    }

    pub fn set_usb_cable(&mut self, connected: bool) {
        self.usb.borrow_mut().set_cable_connected(connected);
    }

    pub fn usb_stats(&self) -> SimUsbStats {
        self.usb.borrow().stats()
    }

    pub fn hw_stats(&self) -> SimHwStats {
        SimHwStats::from(&*self.board.borrow())
    }

    pub fn frame_count(&self) -> usize {
        self.epd_sink.frame_count
    }

    pub fn frame_mode_at(&self, index: usize) -> Option<RefreshMode> {
        if index >= self.epd_sink.frame_count
            || self.epd_sink.frame_count.saturating_sub(index) > FRAME_HISTORY_CAP
        {
            return None;
        }
        Some(self.epd_sink.frame_ring[index % FRAME_HISTORY_CAP])
    }

    pub fn decode_panel_2bpp(&self) -> Vec<u8> {
        self.epd_sink.decode_panel_2bpp()
    }

    pub fn count_dark_panel_pixels(&self) -> usize {
        self.epd_sink.count_dark_panel_pixels()
    }

    fn button_voltages(btn: Button) -> (u16, u16, bool) {
        match btn {
            Button::Back => (2650, 3100, false),
            Button::Confirm => (1950, 3100, false),
            Button::Left => (1050, 3100, false),
            Button::Right => (250, 3100, false),
            Button::Up => (3100, 1800, false),
            Button::Down => (3100, 400, false),
            Button::Power => (3100, 3100, true),
        }
    }

    fn sample_buttons_at(&mut self, delta_ms: u64, mv_a: u16, mv_b: u16, pwr: bool) {
        {
            let mut b = self.board.borrow_mut();
            b.uptime_us = b.uptime_us.saturating_add(delta_ms * 1_000);
        }
        let now_ms = self.board.borrow().uptime_us / 1_000;
        self.event_scratch.clear();
        self.buttons
            .poll_into(now_ms, mv_a, mv_b, pwr, &mut self.event_scratch);
        for i in 0..self.event_scratch.len() {
            let ev = self.event_scratch[i];
            self.app.handle_event(
                ev,
                &mut self.epd_sink,
                &mut self.storage,
                &mut self.sensors,
                &mut self.power,
                &mut self.portal,
                &mut self.entropy,
            );
        }
    }

    fn wake_if_sleeping(&mut self) {
        let mut b = self.board.borrow_mut();
        if b.deep_sleeping {
            b.sd_power_en = true;
            b.deep_sleeping = false;
        }
    }

    pub fn press_button(&mut self, btn: Button) {
        self.wake_if_sleeping();
        let (mv_a, mv_b, pwr) = Self::button_voltages(btn);
        self.sample_buttons_at(1, mv_a, mv_b, pwr);
        self.sample_buttons_at(20, mv_a, mv_b, pwr);
        self.sample_buttons_at(1, 3100, 3100, false);
        self.sample_buttons_at(20, 3100, 3100, false);
        self.log_usb("[VVBOOK-OS] BTN PRESS\n");
    }

    pub fn long_press_button(&mut self, btn: Button) {
        self.wake_if_sleeping();
        let (mv_a, mv_b, pwr) = Self::button_voltages(btn);
        self.sample_buttons_at(1, mv_a, mv_b, pwr);
        self.sample_buttons_at(20, mv_a, mv_b, pwr);
        self.sample_buttons_at(700, mv_a, mv_b, pwr);
        self.sample_buttons_at(1, 3100, 3100, false);
        self.sample_buttons_at(20, 3100, 3100, false);
        self.log_usb("[VVBOOK-OS] BTN LONG\n");
    }

    pub fn advance_idle_ms(&mut self, delta_ms: u64) {
        {
            let mut b = self.board.borrow_mut();
            b.uptime_us = b.uptime_us.saturating_add(delta_ms * 1_000);
        }
        self.app.tick(
            &mut self.epd_sink,
            &mut self.storage,
            &mut self.sensors,
            &mut self.power,
            &mut self.portal,
        );
    }

    pub fn ensure_home(&mut self) {
        match self.app.screen {
            Screen::Home => {}
            Screen::SleepPoster => {
                self.press_button(Button::Confirm);
                if self.app.screen != Screen::Home {
                    self.ensure_home();
                }
            }
            Screen::Reader => {
                if self.app.reader_overlay == ReaderOverlay::TocDrawer {
                    self.press_button(Button::Back);
                }
                if self.app.reader_overlay == ReaderOverlay::AaHud {
                    self.press_button(Button::Back);
                }
                self.press_button(Button::Back);
            }
            Screen::Library | Screen::VvDrop | Screen::PocketPip | Screen::Chess | Screen::Settings => {
                for _ in 0..4 {
                    if self.app.screen == Screen::Home {
                        break;
                    }
                    self.press_button(Button::Back);
                }
            }
        }
    }

    pub fn open_home_item(&mut self, target_cursor: usize) {
        self.ensure_home();
        while self.app.home_cursor != target_cursor {
            self.press_button(Button::Down);
        }
        self.press_button(Button::Confirm);
    }

    pub fn prefetched_turn_budget(&self, max_turns: usize) -> usize {
        if self.app.active_direct.is_some() {
            return self
                .app
                .active_pages
                .len()
                .saturating_sub(1 + self.app.current_page_num)
                .min(max_turns);
        }
        let total_blocks = self
            .app
            .active_index
            .as_ref()
            .map(|i| i.block_count())
            .unwrap_or(0);
        let start_b = self
            .app
            .active_pages
            .first()
            .map(|a| a.block_idx as usize)
            .unwrap_or(0);
        let window_end = (start_b + 16).min(total_blocks);
        let mut count = 0usize;
        for p in 1..self.app.active_pages.len().min(max_turns + 1) {
            let needed_end = self
                .app
                .active_pages
                .get(p + 1)
                .map(|a| (a.block_idx as usize) + 1)
                .unwrap_or(total_blocks)
                .min(total_blocks);
            if needed_end <= window_end {
                count += 1;
            } else {
                break;
            }
        }
        count
    }

    pub fn select_library_index(&mut self, target_idx: usize) {
        if self.app.screen != Screen::Library {
            self.open_home_item(1);
        }
        let n = self.app.catalog.len();
        assert!(target_idx < n, "catalog index {target_idx} out of bounds ({n})");
        let fwd = (target_idx + n - self.app.lib_cursor) % n;
        let bwd = (self.app.lib_cursor + n - target_idx) % n;
        if fwd <= bwd {
            for _ in 0..fwd {
                self.press_button(Button::Down);
            }
        } else {
            for _ in 0..bwd {
                self.press_button(Button::Up);
            }
        }
    }

    pub fn open_library_index(&mut self, target_idx: usize) {
        self.select_library_index(target_idx);
        self.press_button(Button::Confirm);
    }

    pub fn sensors_now(&mut self) -> DateTime {
        self.sensors.now()
    }

    pub fn storage_exists(&self, path: &str) -> bool {
        self.storage.exists(path)
    }

    pub fn storage_file_len(&self, path: &str) -> Option<u64> {
        self.storage.file_len(path).ok()
    }

    pub fn storage_read_slice(&self, path: &str, offset: u64, dst: &mut [u8]) -> Option<usize> {
        self.storage.read_range(path, offset, dst).ok()
    }

    pub fn vvdrop_http_exchange(&mut self, chunks: &[&[u8]], resp_out: &mut [u8]) -> usize {
        self.portal.state.clients_connected = self.portal.state.clients_connected.max(1);
        let mut total_tx = 0usize;
        for &chunk in chunks {
            let mut off = 0usize;
            while off < chunk.len() {
                if !self.portal.conn.wants_rx() {
                    break;
                }
                let consumed =
                    self.portal
                        .conn
                        .on_rx(&chunk[off..], &mut self.portal.state, &mut self.storage);
                if consumed == 0 {
                    break;
                }
                off += consumed;
                while self.portal.conn.has_tx() {
                    let room = resp_out.len().saturating_sub(total_tx);
                    if room == 0 {
                        break;
                    }
                    let wrote = self
                        .portal
                        .conn
                        .peek_tx(&mut resp_out[total_tx..total_tx + room]);
                    if wrote == 0 {
                        break;
                    }
                    self.portal.conn.advance_tx(wrote);
                    total_tx += wrote;
                }
            }
        }
        while self.portal.conn.has_tx() {
            let room = resp_out.len().saturating_sub(total_tx);
            if room == 0 {
                break;
            }
            let wrote = self
                .portal
                .conn
                .peek_tx(&mut resp_out[total_tx..total_tx + room]);
            if wrote == 0 {
                break;
            }
            self.portal.conn.advance_tx(wrote);
            total_tx += wrote;
        }
        if self.portal.conn.should_close() {
            self.portal.conn.reset();
        }
        total_tx
    }

    pub fn uptime_ms(&self) -> u64 {
        self.board.borrow().uptime_us / 1_000
    }

    pub fn portal_is_active(&self) -> bool {
        self.portal.state.active
    }

    pub fn sample_raw_with_wake_guard(
        &mut self,
        delta_ms: u64,
        mv_a: u16,
        mv_b: u16,
        raw_pwr_pressed: bool,
        guard: &mut vvbook_x3_hal::WakePowerGuard,
    ) {
        let next_ms = (self.board.borrow().uptime_us + delta_ms * 1_000) / 1_000;
        let filtered_pwr = guard.filter_pwr_pressed(next_ms, raw_pwr_pressed);
        self.sample_buttons_at(delta_ms, mv_a, mv_b, filtered_pwr);
    }
}

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn fill_prng_bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut st = seed;
    let mut out = vec![0u8; len];
    let mut i = 0usize;
    while i < len {
        let v = splitmix64(&mut st).to_le_bytes();
        let take = (len - i).min(8);
        out[i..i + take].copy_from_slice(&v[..take]);
        i += take;
    }
    out
}

fn encode_base64(raw: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(raw.len().div_ceil(3) * 4);
    for chunk in raw.chunks(3) {
        let b0 = chunk[0] as usize;
        let b1 = *chunk.get(1).unwrap_or(&0) as usize;
        let b2 = *chunk.get(2).unwrap_or(&0) as usize;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[(triple >> 18) & 0x3F] as char);
        out.push(TABLE[(triple >> 12) & 0x3F] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(triple >> 6) & 0x3F] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[triple & 0x3F] as char
        } else {
            '='
        });
    }
    out
}

fn make_adversarial_epub_1100kb() -> Vec<u8> {
    let container = br#"<?xml version="1.0" encoding="utf-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#;

    let mut manifest = String::new();
    let mut spine = String::new();
    let mut ncx_nav = String::new();
    for ch in 1..=650 {
        let _ = writeln!(
            manifest,
            r#"    <opf:item guid="g-{ch}" id="ch{ch}" href="Text/Chapter%20{ch:03}.xhtml" media-type="application/xhtml+xml"/>"#
        );
        let _ = writeln!(spine, r#"    <opf:itemref idref="ch{ch}"/>"#);
        let _ = writeln!(
            ncx_nav,
            r#"    <navPoint id="np{ch}" playOrder="{ch}"><navLabel><text>Chapter {ch}</text></navLabel><content src="Text/Chapter%20{ch:03}.xhtml"/></navPoint>"#
        );
    }
    manifest.push_str(
        "    <opf:item id=\"ncx\" href=\"toc.ncx\" media-type=\"application/x-dtbncx+xml\"/>\n",
    );
    for img in 0..501 {
        let _ = writeln!(
            manifest,
            r#"    <opf:item guid="im-{img}" id="img{img}" href="images/img_{img:03}.jpg" media-type="image/jpeg"/>"#
        );
    }
    let opf = format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<opf:package xmlns:opf="http://www.idpf.org/2007/opf" version="2.0">
  <opf:metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>Dessau Geometry &amp; Type</dc:title>
    <dc:creator>L. Moholy-Nagy</dc:creator>
    <dc:language>en</dc:language>
  </opf:metadata>
  <opf:manifest>
{manifest}  </opf:manifest>
  <opf:spine toc="ncx">
{spine}  </opf:spine>
</opf:package>"#
    );
    let ncx = format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1">
  <navMap>
{ncx_nav}  </navMap>
</ncx>"#
    );

    // 40 KB inline <svg ...> + <math ...> + 3,200-word <br/> mega-paragraph in Chapter 001.
    let svg_garbage = "M10,20L40,60C80,90 ".repeat(2300);
    let mut mega_br = String::from("  <p>");
    for w in 0..3200usize {
        let _ = write!(mega_br, "proportion{w}");
        if w % 9 == 8 {
            mega_br.push_str("<br/>");
        } else {
            mega_br.push(' ');
        }
    }
    mega_br.push_str("</p>\n");
    let mut ch1_paras = String::new();
    for p in 1..=12 {
        let _ = writeln!(
            ch1_paras,
            "  <p>Paragraph {p}: Form follows mathematical proportion on bright optical paper. Across every horizontal rule, a black circle balances a rectilinear grid without ornament. Good typography requires clarity, measured leading, and restrained contrast between the ink weight and the silent margin around the chapter on a sharp monochrome display.</p>"
        );
    }
    let ch1 = format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
<head><style>.lead{{font-size:1.2em;}} </style></head>
<body>
  <h1>I. Light-Space Modulator</h1>
  <svg width="528" height="240" data-blob="{svg_garbage}"><path d="M0 0 L528 240"/></svg>
  <math><mrow><mi>x</mi><mo>=</mo><mn>42</mn></mrow></math>
{mega_br}{ch1_paras}</body>
</html>"#
    );

    let mut owned_entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(1156);
    owned_entries.push((b"META-INF/container.xml".to_vec(), container.to_vec()));
    owned_entries.push((b"OEBPS/content.opf".to_vec(), opf.into_bytes()));
    owned_entries.push((b"OEBPS/toc.ncx".to_vec(), ncx.into_bytes()));
    owned_entries.push((b"OEBPS/Text/Chapter 001.xhtml".to_vec(), ch1.into_bytes()));

    for ch in 2..=650 {
        let mut body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
<body>
  <h1>Chapter {ch} · Elementary Form</h1>
"#
        );
        for p in 1..=8 {
            let _ = writeln!(
                body,
                "  <p>Section {ch}.{p}: Constructive clarity balances black ink and quiet paper.</p>"
            );
        }
        body.push_str("</body>\n</html>");
        owned_entries.push((
            format!("OEBPS/Text/Chapter {ch:03}.xhtml").into_bytes(),
            body.into_bytes(),
        ));
    }

    for img in 0..501 {
        let bin = fill_prng_bytes(0xE90B_0000 + img as u64, 2_200);
        owned_entries.push((format!("OEBPS/images/img_{img:03}.jpg").into_bytes(), bin));
    }
    // Non-UTF8 CP866 image asset entry (total asset entries = 502, total ZIP entries >= 1,155)
    owned_entries.push((
        b"OEBPS/images/\x8e\xa1\xeb\xae\xa6\xaa\xa0.jpg".to_vec(),
        fill_prng_bytes(0xCAFE_BEEF, 2_200),
    ));

    let refs: Vec<(&[u8], &[u8])> = owned_entries
        .iter()
        .map(|(k, v)| (k.as_slice(), v.as_slice()))
        .collect();
    let zip = build_zip_bytes_raw(&refs);
    assert!(
        zip.len() >= 1_200 * 1024,
        "expected adversarial EPUB >= 1.2 MB, got {} bytes",
        zip.len()
    );
    zip
}

fn make_adversarial_fb2_zip_1100kb() -> Vec<u8> {
    let mut xml = String::from(
        r#"<?xml version="1.0" encoding="utf-8"?>
<FictionBook xmlns="http://www.gribuser.ru/xml/fictionbook/2.0">
  <description>
    <title-info>
      <author><first-name>Лев</first-name><last-name>Толстой</last-name></author>
      <book-title>Метель и свет Баухауса</book-title>
      <annotation><p>Издание с крупными встроенными иллюстрациями для стресс-теста потокового парсера.</p></annotation>
      <lang>ru</lang>
    </title-info>
  </description>
  <body>
    <section>
      <title><p>Часть первая · Зимняя дорога</p></title>
"#,
    );
    for p in 0..80 {
        let _ = writeln!(
            xml,
            "      <p>Абзац {}: Снег ложился ровным белым листом на тёмную геометрию ночного поля, и сани скользили без шума, словно перо по хорошей бумаге. Впереди виднелись редкие огни постоялого двора, где в печи потрескивали сухие поленья и закипал медный самовар, а ветер мерно перелистывал заметённые вехи вдоль широкой столбовой дороги.</p>",
            p + 1
        );
    }
    xml.push_str(
        r#"    </section>
  </body>
"#,
    );

    // >= 250 KB primary binary + additional pseudo-random binary attachments so compressed FB2.ZIP >= 1.1 MB.
    let primary_b64 = encode_base64(&fill_prng_bytes(0xFB20_0001, 200_000));
    assert!(primary_b64.len() >= 250 * 1024);
    xml.push_str(r#"  <binary id="cover.jpg" content-type="image/jpeg">"#);
    xml.push_str(&primary_b64);
    xml.push_str("</binary>\n");

    for idx in 1..=7 {
        let b64 = encode_base64(&fill_prng_bytes(0xFB20_0100 + idx, 150_000));
        let _ = writeln!(
            xml,
            r#"  <binary id="plate_{idx}.jpg" content-type="image/jpeg">{b64}</binary>"#
        );
    }
    xml.push_str("</FictionBook>");

    let zip = build_zip_bytes(&[("tolstoy_adversarial.fb2", xml.as_bytes())]);
    assert!(
        zip.len() >= 1_100 * 1024,
        "expected adversarial FB2.ZIP >= 1.1 MB, got {} bytes",
        zip.len()
    );
    zip
}

fn make_chronicles_6000_blocks_md() -> Vec<u8> {
    let mut out = String::with_capacity(350_000);
    out.push_str("# Northern Chronicles of Glass and Ink\n\n");
    out.push_str("## Chapter 1: The Brass Pendulum\n\n");
    for p in 1..=18 {
        let _ = writeln!(
            out,
            "Chronicle 1.{p}: A calm reader turns the page cleanly with deterministic memory usage and crisp serif proportions across every column. Careful kerning, generous margins, and restrained contrast honor centuries of bookcraft on bright optical paper while keeping every flash and SRAM byte strictly accounted for inside bare-metal firmware across all prefetched turns.\n"
        );
    }
    for ch in 2..=60 {
        let _ = writeln!(out, "## Chapter {ch}: Signal Watch\n");
        for p in 1..=102 {
            let _ = writeln!(
                out,
                "Entry {ch}.{p}: Lantern steady on the northern ridge.\n"
            );
        }
    }
    out.into_bytes()
}

fn make_uppercase_manifesto_html() -> Vec<u8> {
    let mut s = String::from(
        "<HTML><HEAD><TITLE>Bauhaus Weimar Typography</TITLE><STYLE>.unused{color:#000}</STYLE></HEAD><BODY>\
         <H1>TEACHING AND TYPOGRAPHY</H1>",
    );
    for i in 1..=24 {
        let _ = write!(
            s,
            "<P>Thesis {i}: Clear legibility demands elemental contrast: solid ink rule, open asymmetric space, and zero decorative clutter. Constructive clarity turns micro-E-Ink devices into calm architectural instruments for daily reading. Geometry sets weight and cadence while disciplined negative space guides the eye cleanly across every crisp monochrome spread without friction or glare.</P>"
        );
    }
    s.push_str("</BODY></HTML>");
    s.into_bytes()
}

fn make_filler_book(idx: usize, ext_kind: usize) -> (String, Vec<u8>) {
    match ext_kind % 5 {
        0 => {
            let txt = format!(
                "Volume {idx}: Essays on Calm Hardware\n\n\
                 Chapter 1: Minimalist Craft\n\n\
                 Paragraph 1 of volume {idx}. Restraint shapes reliable firmware inside 320 KB of internal SRAM.\n\n\
                 Paragraph 2 of volume {idx}. Prefetched block windows prevent needless microSD wakeups during reading.\n"
            );
            (format!("essay_{idx:02}.txt"), txt.into_bytes())
        }
        1 => {
            let md = format!(
                "# Sketchbook {idx}\n\n\
                 ## Part One\n\n\
                 Geometry organizes visual hierarchy through weight, rhythm, and deliberate blank margins.\n"
            );
            (format!("sketchbook_{idx:02}.md"), md.into_bytes())
        }
        2 => {
            let html = format!(
                "<html><head><title>Monograph {idx}</title></head><body>\
                 <h1>Monograph {idx}</h1>\
                 <p>Architectural clarity comes from eliminating borders and letting spacing carry structure.</p></body></html>"
            );
            (format!("monograph_{idx:02}.html"), html.into_bytes())
        }
        3 => {
            let fb2 = format!(
                r#"<?xml version="1.0" encoding="utf-8"?>
<FictionBook xmlns="http://www.gribuser.ru/xml/fictionbook/2.0">
  <description><title-info>
    <author><first-name>Иван</first-name><last-name>Бунин</last-name></author>
    <book-title>Очерк №{idx}</book-title><lang>ru</lang>
  </title-info></description>
  <body><section><title><p>Глава {idx}</p></title>
    <p>Тихий вечер на веранде, шелест страниц и ровный свет лампы.</p>
  </section></body>
</FictionBook>"#
            );
            (format!("ocherk_{idx:02}.fb2"), fb2.into_bytes())
        }
        _ => {
            let ch = format!(
                r#"<?xml version="1.0" encoding="utf-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><body><h1>Issue {idx}</h1><p>Serial EPUB note {idx} stored in nested folders.</p></body></html>"#
            );
            let opf = format!(
                r#"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>Bauhaus Almanac #{idx}</dc:title>
    <dc:creator>Herbert Bayer</dc:creator>
    <dc:language>en</dc:language>
  </metadata>
  <manifest><item id="c1" href="c1.xhtml" media-type="application/xhtml+xml"/></manifest>
  <spine><itemref idref="c1"/></spine>
</package>"#
            );
            let cont = br#"<?xml version="1.0" encoding="utf-8"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#;
            let zip = build_zip_bytes(&[
                ("META-INF/container.xml", cont.as_slice()),
                ("content.opf", opf.as_bytes()),
                ("c1.xhtml", ch.as_bytes()),
            ]);
            (format!("almanac_{idx:02}.epub"), zip)
        }
    }
}

/// Builds a 64 MiB FAT32 SD card image seeded with 56 books across nested folders,
/// including two >= 1.1 MB adversarial archives, a >= 6,000-block novel, CP1251 TXT, and HTML.
pub fn build_adversarial_sd_image() -> PrebuiltSdImage {
    let img = PrebuiltSdImage::empty(DEFAULT_SD_SECTORS);
    let sd_rc = Rc::new(RefCell::new(img));
    let board = Rc::new(RefCell::new(SimBoardState::new()));

    {
        let mut dev = SimSdhcCard::new(Rc::clone(&sd_rc), Rc::clone(&board));
        format_fat32(&mut dev, DEFAULT_SD_SECTORS, b"VVBOOK_X3  ").expect("format 64 MiB FAT32");
        let mut vol = Fat32Volume::mount(dev).expect("mount formatted FAT32");

        for dir in [
            "/books",
            "/Literature",
            "/Literature/English",
            "/Literature/Russian",
            "/Downloads",
            "/Downloads/SciFi",
            "/Archive",
            "/Archive/2026",
            "/Archive/2026/Essays",
            "/Notes",
        ] {
            vol.mkdir_all(dir).expect("mkdir_all nested folder");
        }

        // 5 flagship adversarial books across diverse paths & depths (max depth = 3)
        vol.write_all(
            "/Literature/English/war_and_svg_1100kb.epub",
            &make_adversarial_epub_1100kb(),
        )
        .expect("write 1.1MB epub");
        vol.write_all(
            "/Literature/Russian/tolstoy_adversarial_1100kb.fb2.zip",
            &make_adversarial_fb2_zip_1100kb(),
        )
        .expect("write 1.1MB fb2.zip");
        vol.write_all(
            "/Downloads/SciFi/chronicles_6000_blocks.md",
            &make_chronicles_6000_blocks_md(),
        )
        .expect("write 6000-block md");
        let mut chekhov_text = String::from("Антон Чехов\nДама с собачкой\n\nГлава 1\n\n");
        for p in 1..=18 {
            let _ = writeln!(
                chekhov_text,
                "Абзац {p}: Говорили, что на набережной появилось новое лицо: дама с собачкой. Дмитрий Дмитрич Гуров, проживший в Ялте уже две недели и привыкший тут, тоже стал интересоваться новыми лицами. Сидя в павильоне у Верне, он видел, как по набережной прошла молодая дама, невысокого роста блондинка, в берете; за нею бежал белый шпиц, а море у горизонта темнело ровной спокойной полосой перед закатом.\n"
            );
        }
        vol.write_all(
            "/Literature/Russian/chekhov_cp1251_full.txt",
            &encode_cp1251(&chekhov_text),
        )
        .expect("write cp1251 txt");
        vol.write_all(
            "/Archive/2026/Essays/BAUHAUS_MANIFESTO.HTML",
            &make_uppercase_manifesto_html(),
        )
        .expect("write uppercase html");

        // 51 additional books distributed across folders (5 + 51 = 56 total books)
        let folders = [
            "/books",
            "/Literature/English",
            "/Literature/Russian",
            "/Downloads/SciFi",
            "/Archive/2026/Essays",
            "/Notes",
            "",
        ];
        for i in 0..51 {
            let folder = folders[i % folders.len()];
            let (fname, data) = make_filler_book(i + 1, i);
            let full_path = if folder.is_empty() {
                format!("/{fname}")
            } else {
                format!("{folder}/{fname}")
            };
            vol.write_all(&full_path, &data)
                .expect("write filler book");
        }
    }

    let sectors = std::mem::take(&mut sd_rc.borrow_mut().sectors);
    PrebuiltSdImage { sectors }
}

fn collect_host_books(
    root: &Path,
    cur: &Path,
    depth: u8,
    out: &mut Vec<(String, Vec<u8>)>,
) -> std::io::Result<()> {
    let mut entries: Vec<_> = fs::read_dir(cur)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let p = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        if p.is_dir() {
            if depth < 5 {
                collect_host_books(root, &p, depth + 1, out)?;
            }
            continue;
        }
        if InputFormat::from_filename(&name).is_none() {
            continue;
        }
        let rel = p
            .strip_prefix(root)
            .unwrap_or(&p)
            .to_string_lossy()
            .replace('\\', "/");
        let bytes = fs::read(&p)?;
        if !bytes.is_empty() {
            out.push((format!("/{rel}"), bytes));
        }
    }
    Ok(())
}

/// Builds a FAT32 SD card image seeded with caller-supplied `(fat_path, bytes)` entries.
pub fn build_fat32_sd_image_from_slice(files: &[(&str, &[u8])]) -> Result<PrebuiltSdImage, String> {
    let total_bytes: u64 = files.iter().map(|(_, b)| b.len() as u64).sum();
    let needed_sectors = ((total_bytes / 512) * 2 + 65_536)
        .clamp(DEFAULT_SD_SECTORS as u64, 1_048_576) as u32;

    let img = PrebuiltSdImage::empty(needed_sectors);
    let sd_rc = Rc::new(RefCell::new(img));
    let board = Rc::new(RefCell::new(SimBoardState::new()));
    {
        let mut dev = SimSdhcCard::new(Rc::clone(&sd_rc), Rc::clone(&board));
        format_fat32(&mut dev, needed_sectors, b"VVBOOK_X3  ")
            .map_err(|e| format!("FAT32 format failed: {e:?}"))?;
        let mut vol = Fat32Volume::mount(dev)
            .map_err(|e| format!("FAT32 mount failed: {e:?}"))?;
        for (fat_path, data) in files {
            if let Some(slash) = fat_path.rfind('/')
                && slash > 0
            {
                vol.mkdir_all(&fat_path[..slash])
                    .map_err(|e| format!("mkdir {fat_path}: {e:?}"))?;
            }
            vol.write_all(fat_path, data)
                .map_err(|e| format!("write {fat_path}: {e:?}"))?;
        }
    }
    let sectors = std::mem::take(&mut sd_rc.borrow_mut().sectors);
    Ok(PrebuiltSdImage { sectors })
}

/// Imports a host directory tree (e.g. a real microSD dump) into a fresh FAT32 hardware image.
pub fn import_host_dir_to_fat32(dir: &Path) -> std::io::Result<(PrebuiltSdImage, usize)> {
    let mut files = Vec::new();
    collect_host_books(dir, dir, 0, &mut files)?;
    let refs: Vec<(&str, &[u8])> = files.iter().map(|(p, b)| (p.as_str(), b.as_slice())).collect();
    let img = build_fat32_sd_image_from_slice(&refs).map_err(std::io::Error::other)?;
    Ok((img, files.len()))
}

#[derive(Clone, Debug)]
pub struct BookBenchResult {
    pub path: String,
    pub total_pages: usize,
    pub cold_sd_ms: u64,
    pub prefetch_turn_sectors_delta: u64,
    pub warm_reopen_sd_ms: u64,
}

#[derive(Clone, Debug)]
pub struct X3StressReport {
    pub catalog_count: usize,
    pub boot_sd_ms: u64,
    pub books: Vec<BookBenchResult>,
    pub unplug_timeout_stalls: u32,
    pub unplug_dropped_bytes: u64,
    pub replug_host_online: bool,
    pub hw: SimHwStats,
}

fn find_catalog_index(twin: &X3HardwareTwin, suffix: &str) -> Result<usize, String> {
    twin.app
        .catalog
        .iter()
        .position(|c| c.source_path.ends_with(suffix))
        .ok_or_else(|| format!("missing expected book suffix '{suffix}' in catalog"))
}

/// Runs the end-to-end Xteink X3 hardware twin verification and latency benchmark suite.
pub fn run_x3_stress_suite(host_dir: Option<&Path>) -> Result<X3StressReport, String> {
    let (sd_img, using_host_dir) = match host_dir {
        Some(dir) => {
            let (img, count) = import_host_dir_to_fat32(dir)
                .map_err(|e| format!("failed to import {}: {e}", dir.display()))?;
            if count == 0 {
                return Err(format!("no supported books found in {}", dir.display()));
            }
            (img, true)
        }
        None => (build_adversarial_sd_image(), false),
    };

    let mut twin = X3HardwareTwin::from_sd_image(sd_img);

    let boot_sd0 = twin.hw_stats().spi_sd_us;
    twin.boot();
    let boot_sd_ms = (twin.hw_stats().spi_sd_us - boot_sd0) / 1_000;
    let catalog_count = twin.app.catalog.len();
    if catalog_count == 0 {
        return Err("catalog empty after boot".to_string());
    }

    // 1. USB cable unplug + replug FIFO saturation check
    twin.set_usb_cable(false);
    twin.open_home_item(1); // Library
    for _ in 0..4 {
        twin.press_button(Button::Down);
    }
    let unplugged = twin.usb_stats();
    if unplugged.timeout_stalls != 1 || unplugged.dropped_bytes == 0 || unplugged.host_online {
        return Err(format!(
            "USB unplug check failed: stalls={}, dropped={}, online={}",
            unplugged.timeout_stalls, unplugged.dropped_bytes, unplugged.host_online
        ));
    }
    twin.set_usb_cable(true);
    twin.press_button(Button::Up);
    let replugged = twin.usb_stats();
    if !replugged.host_online {
        return Err("USB replug did not restore host_online".to_string());
    }

    // 2. Benchmark target books (5 adversarial volumes on synthetic SD, or up to first 5 host books)
    let targets: Vec<usize> = if using_host_dir {
        (0..catalog_count.min(5)).collect()
    } else {
        vec![
            find_catalog_index(&twin, "war_and_svg_1100kb.epub")?,
            find_catalog_index(&twin, "tolstoy_adversarial_1100kb.fb2.zip")?,
            find_catalog_index(&twin, "chronicles_6000_blocks.md")?,
            find_catalog_index(&twin, "chekhov_cp1251_full.txt")?,
            find_catalog_index(&twin, "BAUHAUS_MANIFESTO.HTML")?,
        ]
    };

    let mut bench_results = Vec::with_capacity(targets.len());
    for &idx in &targets {
        let source_path = twin.app.catalog[idx].source_path.clone();
        twin.select_library_index(idx);
        let f_before = twin.frame_count();
        let sd_before = twin.hw_stats().spi_sd_us;
        twin.press_button(Button::Confirm);
        let cold_sd_ms = (twin.hw_stats().spi_sd_us - sd_before) / 1_000;
        if twin.app.screen != Screen::Reader {
            return Err(format!("failed to open {source_path} into Reader"));
        }
        if twin.frame_count() < f_before + 2
            || twin.frame_mode_at(f_before) != Some(RefreshMode::FastDu)
        {
            return Err(format!(
                "missing FastDu Typesetting placard before cold open of {source_path}"
            ));
        }
        let cold_budget_ms = 800;
        if cold_sd_ms >= cold_budget_ms {
            return Err(format!(
                "cold open SPI SD latency too high on {source_path}: {cold_sd_ms} ms (>= {cold_budget_ms} ms)"
            ));
        }
        let total_pages = twin.app.active_pages.len();

        // Flush any deferred persistence from prior navigation before measuring zero-SD page turns.
        if twin.app.pending_journal_turns > 0 {
            twin.app.persistence_dirty = true;
        }
        twin.advance_idle_ms(1_600);

        // Consecutive page turns within the 16-block prefetch window must cause ZERO SD reads.
        let turns = twin.prefetched_turn_budget(5);
        let sec0 = twin.hw_stats().sd_sectors_read;
        for _ in 0..turns {
            twin.press_button(Button::Right);
        }
        let prefetch_delta = twin.hw_stats().sd_sectors_read - sec0;
        if prefetch_delta != 0 {
            return Err(format!(
                "prefetched page turns caused {prefetch_delta} SD sector reads on {source_path}"
            ));
        }

        // Go back to Library and verify warm cached reopen latency < 80 ms SPI SD time.
        twin.press_button(Button::Back); // Back to Home
        twin.open_home_item(1); // Back to Library
        let warm_sd0 = twin.hw_stats().spi_sd_us;
        twin.open_library_index(idx);
        let warm_sd_ms = (twin.hw_stats().spi_sd_us - warm_sd0) / 1_000;
        if warm_sd_ms >= 80 {
            return Err(format!(
                "warm reopen SPI SD latency too high on {source_path}: {warm_sd_ms} ms (>= 80 ms)"
            ));
        }

        bench_results.push(BookBenchResult {
            path: source_path,
            total_pages,
            cold_sd_ms,
            prefetch_turn_sectors_delta: prefetch_delta,
            warm_reopen_sd_ms: warm_sd_ms,
        });
    }

    // 3. Stage 1 & Stage 3 battery governor check
    twin.advance_idle_ms(3_000);
    if !twin.hw_stats().panel_hv_standby {
        return Err("Stage 1 governor did not engage panel HV standby".to_string());
    }
    twin.advance_idle_ms(605_000);
    if twin.app.screen != Screen::SleepPoster || !twin.hw_stats().deep_sleeping {
        return Err("Stage 3 governor did not enter SleepPoster + deep sleep".to_string());
    }
    twin.press_button(Button::Confirm);
    if twin.hw_stats().deep_sleeping || !twin.hw_stats().sd_power_en {
        return Err("Button press did not wake device from deep sleep".to_string());
    }

    let hw = twin.hw_stats();
    if hw.sd_rail_faults != 0 {
        return Err(format!("detected {} SD rail faults", hw.sd_rail_faults));
    }
    if hw.oversize_read_all_rejections != 0 {
        return Err(format!(
            "detected {} oversize read_all rejections (>160KB)",
            hw.oversize_read_all_rejections
        ));
    }

    Ok(X3StressReport {
        catalog_count,
        boot_sd_ms,
        books: bench_results,
        unplug_timeout_stalls: unplugged.timeout_stalls,
        unplug_dropped_bytes: unplugged.dropped_bytes,
        replug_host_online: replugged.host_online,
        hw,
    })
}

pub fn format_stress_report(r: &X3StressReport) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "=== Xteink X3 Hardware Twin Stress Report ===");
    let _ = writeln!(
        out,
        "Catalog discovered : {} books | Boot SPI-SD: {} ms",
        r.catalog_count, r.boot_sd_ms
    );
    let _ = writeln!(
        out,
        "USB-Serial/JTAG    : unplug stalls={}, dropped={} B, replug online={}",
        r.unplug_timeout_stalls, r.unplug_dropped_bytes, r.replug_host_online
    );
    let _ = writeln!(
        out,
        "Hardware rails     : sd_rail_faults={}, oversize_read_all={}, sd_sectors(r={}, w={})",
        r.hw.sd_rail_faults,
        r.hw.oversize_read_all_rejections,
        r.hw.sd_sectors_read,
        r.hw.sd_sectors_written
    );
    let _ = writeln!(
        out,
        "Simulated clocks   : spi_sd={} ms, spi_epd={} ms, epd_busy={} ms, uptime={} ms",
        r.hw.spi_sd_us / 1_000,
        r.hw.spi_epd_us / 1_000,
        r.hw.epd_busy_us / 1_000,
        r.hw.uptime_us / 1_000
    );
    for b in &r.books {
        let _ = writeln!(
            out,
            "  - {:48} | {:4} pp | cold {:4} ms | warm {:2} ms | turn_sd_delta={}",
            b.path, b.total_pages, b.cold_sd_ms, b.warm_reopen_sd_ms, b.prefetch_turn_sectors_delta
        );
    }
    out
}
