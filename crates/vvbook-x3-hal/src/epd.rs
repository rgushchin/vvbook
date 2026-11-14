use alloc::vec;
use alloc::vec::Vec;
use vvbook_pal::{
    DisplaySink, PORTRAIT_H, PORTRAIT_W, ROW_2BPP_BYTES, RefreshMode, Tone,
};

use crate::pins::{EPD_NATIVE_W, EPD_PLANE_BYTES};
use crate::{DelayUs, EpdPins, HalError, SpiBus};

/// Native controller row stride in bytes (`792 / 8 = 99`).
pub const NATIVE_ROW_BYTES: usize = EPD_NATIVE_W / 8;

/// E-Ink timing controller silicon variants shipped in Xteink X3 hardware revisions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EpdSilicon {
    Uc8253,
    Uc8279,
}

impl EpdSilicon {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Uc8253 => "UC8253 (X3)",
            Self::Uc8279 => "UC8279 (X3)",
        }
    }
}

/// Detects panel controller silicon from the 3-byte `0x70` 3-wire register response.
pub const fn probe_silicon(ver: [u8; 3]) -> EpdSilicon {
    match ver[2] {
        // Stock X3 V6.3.15 & CrossInk XteinkDetect table: 0x66 plus known UC8279 LUT_VER IDs.
        0x66 | 0x02 | 0x03 | 0x41 | 0x42 | 0x67 | 0x68 | 0x69 => EpdSilicon::Uc8279,
        _ => EpdSilicon::Uc8253,
    }
}

/// Maps portrait `(px, py)` inside `528x792` to native landscape `(nx, ny)` inside `792x528`
/// corresponding to top-to-bottom row streaming (`0..528`).
#[inline]
pub const fn portrait_to_native(px: usize, py: usize, rotated_180: bool) -> (usize, usize) {
    if rotated_180 {
        (PORTRAIT_H - 1 - py, PORTRAIT_W - 1 - px)
    } else {
        (py, px)
    }
}

const UC8253_PTL_WINDOW: [u8; 9] = [0x00, 0x00, 0x03, 0x17, 0x00, 0x00, 0x02, 0x0F, 0x01];
const UC8279_PTL_WINDOW: [u8; 9] = [0x00, 0x00, 0x03, 0x17, 0x00, 0x00, 0x02, 0x0F, 0x01];

const fn lut42(prefix: &[u8]) -> [u8; 42] {
    let mut out = [0u8; 42];
    let mut i = 0;
    while i < prefix.len() && i < 42 {
        out[i] = prefix[i];
        i += 1;
    }
    out
}

const UC8253_LUT_FULL_GC: [(u8, [u8; 42]); 5] = [
    (0x20, lut42(&[0x00, 0x18, 0x04, 0x0E, 0x0A, 0x01, 0x00, 0x0A, 0x00, 0x00, 0x00, 0x01])),
    (0x21, lut42(&[0x4A, 0x18, 0x04, 0x0E, 0x0A, 0x01, 0x00, 0x0A, 0x00, 0x00, 0x00, 0x01])),
    (0x22, lut42(&[0x0A, 0x18, 0x04, 0x0E, 0x0A, 0x01, 0x00, 0x0A, 0x00, 0x00, 0x00, 0x01])),
    (0x23, lut42(&[0x04, 0x18, 0x04, 0x0E, 0x0A, 0x01, 0x40, 0x0A, 0x00, 0x00, 0x00, 0x01])),
    (0x24, lut42(&[0x84, 0x18, 0x04, 0x0E, 0x0A, 0x01, 0x40, 0x0A, 0x00, 0x00, 0x00, 0x01])),
];

const UC8253_LUT_HALF_SCRUB: [(u8, [u8; 42]); 5] = [
    (0x20, lut42(&[0x00, 0x06, 0x01, 0x06, 0x06, 0x01, 0x00, 0x04, 0x01, 0x01, 0x00, 0x01])),
    (0x21, lut42(&[0xAA, 0x06, 0x01, 0x06, 0x06, 0x01, 0xA0, 0x04, 0x01, 0x01, 0x00, 0x01])),
    (0x22, lut42(&[0xAA, 0x06, 0x01, 0x06, 0x06, 0x01, 0xA0, 0x04, 0x01, 0x01, 0x00, 0x01])),
    (0x23, lut42(&[0x55, 0x06, 0x01, 0x06, 0x06, 0x01, 0x50, 0x04, 0x01, 0x01, 0x00, 0x01])),
    (0x24, lut42(&[0x55, 0x06, 0x01, 0x06, 0x06, 0x01, 0x50, 0x04, 0x01, 0x01, 0x00, 0x01])),
];

const UC8253_LUT_FAST_DU: [(u8, [u8; 42]); 5] = [
    (0x20, lut42(&[0x00, 0x04, 0x02, 0x04, 0x04, 0x01, 0x00, 0x04, 0x01, 0x00, 0x00, 0x01])),
    (0x21, lut42(&[0x20, 0x04, 0x02, 0x04, 0x04, 0x01, 0x00, 0x04, 0x01, 0x00, 0x00, 0x01])),
    (0x22, lut42(&[0xAA, 0x04, 0x02, 0x04, 0x04, 0x01, 0x80, 0x04, 0x01, 0x00, 0x00, 0x01])),
    (0x23, lut42(&[0x55, 0x04, 0x02, 0x04, 0x04, 0x01, 0x40, 0x04, 0x01, 0x00, 0x00, 0x01])),
    (0x24, lut42(&[0x10, 0x04, 0x02, 0x04, 0x04, 0x01, 0x00, 0x04, 0x01, 0x00, 0x00, 0x01])),
];

const UC8253_LUT_GRAY4: [(u8, [u8; 42]); 5] = [
    (0x20, lut42(&[0x00, 0x08, 0x0B, 0x02, 0x03, 0x01, 0x00, 0x0C, 0x02, 0x07, 0x02, 0x01, 0x00, 0x01, 0x00, 0x02, 0x00, 0x01])),
    (0x21, lut42(&[0x00, 0x08, 0x0B, 0x02, 0x03, 0x01, 0x4A, 0x0C, 0x02, 0x07, 0x02, 0x01, 0x88, 0x01, 0x00, 0x02, 0x00, 0x01])),
    (0x22, lut42(&[0x80, 0x08, 0x0B, 0x02, 0x03, 0x01, 0x62, 0x0C, 0x02, 0x07, 0x02, 0x01, 0x00, 0x01, 0x00, 0x02, 0x00, 0x01])),
    (0x23, lut42(&[0x88, 0x08, 0x0B, 0x02, 0x03, 0x01, 0x60, 0x0C, 0x02, 0x07, 0x02, 0x01, 0x00, 0x01, 0x00, 0x02, 0x00, 0x01])),
    (0x24, lut42(&[0xA8, 0x08, 0x0B, 0x02, 0x03, 0x01, 0x44, 0x0C, 0x02, 0x07, 0x02, 0x01, 0x04, 0x01, 0x00, 0x02, 0x00, 0x01])),
];

const fn lut49(prefix: &[u8]) -> [u8; 49] {
    let mut out = [0u8; 49];
    let mut i = 0;
    while i < prefix.len() && i < 49 {
        out[i] = prefix[i];
        i += 1;
    }
    out
}

const UC8279_LUT_FULL_GC: [(u8, [u8; 42]); 5] = [
    (0x20, lut42(&[0x01, 0x1A, 0x1A, 0x01, 0x00, 0x01, 0x01])),
    (0x21, lut42(&[0x01, 0x5A, 0x9A, 0x01, 0x00, 0x01, 0x01])),
    (0x22, lut42(&[0x01, 0x1A, 0x9A, 0x01, 0x00, 0x01, 0x01])),
    (0x23, lut42(&[0x01, 0x1A, 0x5A, 0x01, 0x00, 0x01, 0x01])),
    (0x24, lut42(&[0x01, 0x9A, 0x5A, 0x01, 0x00, 0x01, 0x01])),
];

const UC8279_LUT_HALF_SCRUB: [(u8, [u8; 42]); 5] = [
    (0x20, lut42(&[0x01, 0x07, 0x01, 0x06, 0x06, 0x01, 0x01, 0x01, 0x06, 0x01, 0x00, 0x00, 0x01, 0x00])),
    (0x21, lut42(&[0x01, 0x07, 0x81, 0x06, 0x06, 0x01, 0x01, 0x01, 0x06, 0x01, 0x00, 0x00, 0x01, 0x00])),
    (0x22, lut42(&[0x01, 0x87, 0x81, 0x86, 0x86, 0x01, 0x01, 0x01, 0x86, 0x01, 0x00, 0x00, 0x01, 0x00])),
    (0x23, lut42(&[0x01, 0x47, 0x41, 0x46, 0x46, 0x01, 0x01, 0x01, 0x46, 0x01, 0x00, 0x00, 0x01, 0x00])),
    (0x24, lut42(&[0x01, 0x07, 0x01, 0x06, 0x06, 0x01, 0x01, 0x01, 0x06, 0x01, 0x00, 0x00, 0x01, 0x00])),
];

const UC8279_LUT_FAST_DU: [(u8, [u8; 42]); 5] = [
    (0x20, lut42(&[0x01, 0x06, 0x01, 0x06, 0x06, 0x01, 0x01, 0x01, 0x02, 0x04, 0x00, 0x00, 0x01, 0x01])),
    (0x21, lut42(&[0x01, 0x06, 0x81, 0x06, 0x06, 0x01, 0x01, 0x01, 0x02, 0x04, 0x00, 0x00, 0x01, 0x01])),
    (0x22, lut42(&[0x01, 0x86, 0x81, 0x86, 0x86, 0x01, 0x01, 0x01, 0x82, 0x84, 0x00, 0x00, 0x01, 0x01])),
    (0x23, lut42(&[0x01, 0x46, 0x41, 0x46, 0x46, 0x01, 0x01, 0x01, 0x42, 0x44, 0x00, 0x00, 0x01, 0x01])),
    (0x24, lut42(&[0x01, 0x06, 0x01, 0x06, 0x06, 0x01, 0x01, 0x01, 0x02, 0x44, 0x00, 0x00, 0x01, 0x01])),
];

const UC8279_LUT_GRAY4: [(u8, [u8; 49]); 5] = [
    (0x20, lut49(&[0x01, 0x10, 0x06, 0x02, 0x03, 0x01, 0x01, 0x01, 0x01, 0x01, 0x02, 0x02, 0x01, 0x01, 0x01, 0x14, 0x03, 0x02, 0x03, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x07, 0x01, 0x01, 0x01])),
    (0x24, lut49(&[0x01, 0x90, 0x86, 0x02, 0x03, 0x01, 0x01, 0x01, 0x01, 0x01, 0x02, 0x02, 0x01, 0x01, 0x01, 0x54, 0x83, 0x02, 0x83, 0x01, 0x01, 0x01, 0x81, 0x41, 0x41, 0x47, 0x01, 0x01, 0x01])),
    (0x22, lut49(&[0x01, 0x90, 0x06, 0x82, 0x83, 0x01, 0x01, 0x01, 0x01, 0x41, 0x42, 0x42, 0x01, 0x01, 0x01, 0x54, 0x83, 0x42, 0x83, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x07, 0x01, 0x01, 0x01])),
    (0x23, lut49(&[0x01, 0x90, 0x06, 0x82, 0x83, 0x01, 0x01, 0x01, 0x81, 0x41, 0x42, 0x42, 0x01, 0x01, 0x01, 0x54, 0x83, 0x02, 0x03, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x07, 0x01, 0x01, 0x01])),
    (0x21, lut49(&[0x01, 0x10, 0x06, 0x82, 0x83, 0x01, 0x01, 0x01, 0x81, 0x01, 0x42, 0x42, 0x01, 0x01, 0x01, 0x54, 0x83, 0x82, 0x83, 0x01, 0x01, 0x01, 0x81, 0x81, 0x81, 0x87, 0x01, 0x01, 0x01])),
];

/// Single permanent 1-bpp native bitplane (`52,272` bytes) for B/W operation, plus an
/// on-demand LSB plane (`gray_lsb_plane`) allocated only while framing `RefreshMode::Grayscale4`.
/// Saves `52,272` bytes (`51 KB`) of ESP32-C3 heap during reading, scanning, and Wi-Fi transfer.
pub struct X3EpdPlanes {
    pub new_plane: Vec<u8>,
    pub gray_lsb_plane: Option<Vec<u8>>,
}

impl Default for X3EpdPlanes {
    fn default() -> Self {
        Self::new()
    }
}

impl X3EpdPlanes {
    pub fn new() -> Self {
        Self {
            new_plane: vec![0xFF; EPD_PLANE_BYTES],
            gray_lsb_plane: None,
        }
    }

    pub fn fill_all(&mut self, byte_val: u8) {
        self.new_plane.fill(byte_val);
        if let Some(ref mut lsb) = self.gray_lsb_plane {
            lsb.fill(byte_val);
        }
    }

    pub fn release_gray_lsb(&mut self) {
        self.gray_lsb_plane = None;
    }

    #[inline]
    fn set_bit(plane: &mut [u8], nx: usize, ny: usize, white: bool) {
        let byte_idx = ny * NATIVE_ROW_BYTES + (nx >> 3);
        let mask = 0x80u8 >> (nx & 7);
        if white {
            plane[byte_idx] |= mask;
        } else {
            plane[byte_idx] &= !mask;
        }
    }

    #[inline]
    pub fn get_native_bit(plane: &[u8], nx: usize, ny: usize) -> bool {
        let byte_idx = ny * NATIVE_ROW_BYTES + (nx >> 3);
        let mask = 0x80u8 >> (nx & 7);
        (plane[byte_idx] & mask) != 0
    }
}

/// Portable Xteink X3 E-Ink driver supporting UC8253 & UC8279 panels, 4-tone grayscale,
/// differential B/W page turns, `POF (0x02)` HV standby, and `DSLP (0x07)` deep sleep.
pub struct X3EpdDriver<SPI, PINS, D> {
    spi: SPI,
    pins: PINS,
    delay: D,
    silicon: EpdSilicon,
    planes: X3EpdPlanes,
    active_mode: RefreshMode,
    rotated_180: bool,
    hv_awake: bool,
    sleeping: bool,
    first_refresh: bool,
    last_was_gray4: bool,
    async_refresh: bool,
    refresh_in_flight: bool,
    pending_ptout: bool,
    pending_sync_dtm1: bool,
    pending_full_settle: bool,
}

impl<SPI, PINS, D> X3EpdDriver<SPI, PINS, D>
where
    SPI: SpiBus,
    PINS: EpdPins,
    D: DelayUs,
{
    fn pulse_reset(pins: &mut PINS, delay: &mut D) {
        pins.set_cs(true);
        pins.set_dc(true);
        pins.set_rst(true);
        delay.delay_us(10_000);
        pins.set_rst(false);
        delay.delay_us(50_000);
        pins.set_rst(true);
        delay.delay_us(50_000);
        for _ in 0..300 {
            if !pins.is_busy() {
                break;
            }
            delay.delay_us(1_000);
        }
    }

    pub fn new(spi: SPI, mut pins: PINS, mut delay: D) -> Result<Self, HalError> {
        Self::pulse_reset(&mut pins, &mut delay);

        let ver = pins.read_0x70_3wire();
        let silicon = probe_silicon(ver);

        let mut drv = Self {
            spi,
            pins,
            delay,
            silicon,
            planes: X3EpdPlanes::new(),
            active_mode: RefreshMode::FullGc,
            rotated_180: false,
            hv_awake: false,
            sleeping: false,
            first_refresh: true,
            last_was_gray4: false,
            async_refresh: false,
            refresh_in_flight: false,
            pending_ptout: false,
            pending_sync_dtm1: false,
            pending_full_settle: false,
        };
        drv.init_panel()?;
        Ok(drv)
    }

    pub fn set_async_refresh(&mut self, enabled: bool) {
        self.async_refresh = enabled;
    }

    pub fn finish_pending_refresh(&mut self) -> Result<(), HalError> {
        if !self.refresh_in_flight {
            return Ok(());
        }
        let mut finished = false;
        for _ in 0..32_000 {
            if !self.pins.is_busy() {
                finished = true;
                break;
            }
            self.delay.delay_us(250);
        }
        self.refresh_in_flight = false;
        let sync_dtm1 = core::mem::replace(&mut self.pending_sync_dtm1, false);
        let full_settle = core::mem::replace(&mut self.pending_full_settle, false);
        let ptout = core::mem::replace(&mut self.pending_ptout, false);
        if !finished {
            return Err(HalError::Timeout);
        }
        if sync_dtm1 && matches!(self.silicon, EpdSilicon::Uc8279) {
            Self::write_plane_buf(&mut self.pins, &mut self.spi, 0x10, &self.planes.new_plane)?;
        }
        if ptout {
            self.send_cmd(0x92)?;
        }
        if sync_dtm1 && matches!(self.silicon, EpdSilicon::Uc8253) {
            Self::write_plane_buf(&mut self.pins, &mut self.spi, 0x10, &self.planes.new_plane)?;
        }
        if full_settle && matches!(self.silicon, EpdSilicon::Uc8253) {
            // UC8253 on X3 garbles the first FastDu after a FullGc unless settled once cleanly.
            self.send_cmd_data(0x50, &[0x29, 0x07])?;
            self.load_luts42(&UC8253_LUT_FAST_DU)?;
            Self::write_plane_buf(&mut self.pins, &mut self.spi, 0x13, &self.planes.new_plane)?;
            self.send_cmd(0x12)?;
            self.wait_busy_two_phase(5_000)?;
            Self::write_plane_buf(&mut self.pins, &mut self.spi, 0x10, &self.planes.new_plane)?;
        }
        Ok(())
    }

    pub fn silicon(&self) -> EpdSilicon {
        self.silicon
    }

    pub fn set_rotated_180(&mut self, rotated: bool) {
        self.rotated_180 = rotated;
    }

    pub fn is_hv_awake(&self) -> bool {
        self.hv_awake
    }

    pub fn planes(&self) -> &X3EpdPlanes {
        &self.planes
    }

    pub fn into_parts(self) -> (SPI, PINS, D) {
        (self.spi, self.pins, self.delay)
    }

    pub fn spi_mut(&mut self) -> &mut SPI {
        &mut self.spi
    }

    pub fn pins_mut(&mut self) -> &mut PINS {
        &mut self.pins
    }

    /// Flushes the currently staged frame buffer and returns any HAL/timeout error explicitly.
    pub fn try_end_frame(&mut self, mode: RefreshMode) -> Result<(), HalError> {
        self.flush_mode(mode)
    }

    fn wait_busy_two_phase(&mut self, max_polls: u32) -> Result<(), HalError> {
        // X3 UC8253/UC8279 BUSY_N starts HIGH right after PON/DRF/POF, drops LOW
        // while the internal state machine works, then rises HIGH on completion.
        let mut entered_busy = self.pins.is_busy();
        if !entered_busy {
            for _ in 0..80 {
                self.delay.delay_us(250);
                if self.pins.is_busy() {
                    entered_busy = true;
                    break;
                }
            }
        }
        if !entered_busy {
            return Ok(());
        }
        let phase2_polls = max_polls.saturating_mul(4);
        for _ in 0..phase2_polls {
            if !self.pins.is_busy() {
                return Ok(());
            }
            self.delay.delay_us(250);
        }
        Err(HalError::Timeout)
    }

    fn send_cmd_bus(pins: &mut PINS, spi: &mut SPI, cmd: u8) -> Result<(), HalError> {
        pins.set_dc(false);
        pins.set_cs(false);
        let res = spi.write(&[cmd]);
        pins.set_cs(true);
        res
    }

    fn send_cmd(&mut self, cmd: u8) -> Result<(), HalError> {
        Self::send_cmd_bus(&mut self.pins, &mut self.spi, cmd)
    }

    fn send_data_bus(pins: &mut PINS, spi: &mut SPI, data: &[u8]) -> Result<(), HalError> {
        pins.set_dc(true);
        pins.set_cs(false);
        let res = spi.write(data);
        pins.set_cs(true);
        res
    }

    fn send_data(&mut self, data: &[u8]) -> Result<(), HalError> {
        Self::send_data_bus(&mut self.pins, &mut self.spi, data)
    }

    fn send_cmd_data(&mut self, cmd: u8, data: &[u8]) -> Result<(), HalError> {
        self.send_cmd(cmd)?;
        if !data.is_empty() {
            self.send_data(data)?;
        }
        Ok(())
    }

    fn load_luts42(&mut self, luts: &[(u8, [u8; 42]); 5]) -> Result<(), HalError> {
        for (cmd, lut) in luts {
            self.send_cmd_data(*cmd, lut)?;
        }
        Ok(())
    }

    fn write_plane_buf(pins: &mut PINS, spi: &mut SPI, cmd: u8, data: &[u8]) -> Result<(), HalError> {
        Self::send_cmd_bus(pins, spi, cmd)?;
        Self::send_data_bus(pins, spi, data)?;
        Self::send_cmd_bus(pins, spi, 0x11)
    }

    fn write_plane_const(&mut self, cmd: u8, fill: u8) -> Result<(), HalError> {
        let chunk = [fill; 256];
        self.send_cmd(cmd)?;
        self.pins.set_dc(true);
        self.pins.set_cs(false);
        let mut rem = EPD_PLANE_BYTES;
        let mut res = Ok(());
        while rem > 0 {
            let n = rem.min(chunk.len());
            res = self.spi.write(&chunk[..n]);
            if res.is_err() {
                break;
            }
            rem -= n;
        }
        self.pins.set_cs(true);
        res?;
        self.send_cmd(0x11)
    }

    fn write_plane_inv(pins: &mut PINS, spi: &mut SPI, cmd: u8, data: &[u8]) -> Result<(), HalError> {
        let mut chunk = [0u8; 256];
        Self::send_cmd_bus(pins, spi, cmd)?;
        pins.set_dc(true);
        pins.set_cs(false);
        let mut res = Ok(());
        for src in data.chunks(chunk.len()) {
            for (dst, &b) in chunk[..src.len()].iter_mut().zip(src.iter()) {
                *dst = !b;
            }
            res = spi.write(&chunk[..src.len()]);
            if res.is_err() {
                break;
            }
        }
        pins.set_cs(true);
        res?;
        Self::send_cmd_bus(pins, spi, 0x11)
    }

    fn init_panel(&mut self) -> Result<(), HalError> {
        self.spi.set_frequency_hz(20_000_000);
        Self::pulse_reset(&mut self.pins, &mut self.delay);
        self.sleeping = false;
        self.first_refresh = true;
        self.last_was_gray4 = false;
        self.pending_sync_dtm1 = false;
        self.pending_full_settle = false;
        match self.silicon {
            EpdSilicon::Uc8253 => {
                // Factory Xteink X3 UC8253 init (app1 @ 0x4200709e + Uc8253X3Driver::initController)
                self.send_cmd_data(0x00, &[0x3F, 0x0A])?;
                self.send_cmd_data(0x61, &[0x03, 0x18, 0x02, 0x58])?;
                self.send_cmd_data(0x65, &[0x00, 0x00, 0x00, 0x00])?;
                self.send_cmd_data(0x03, &[0x20])?;
                self.send_cmd_data(0x01, &[0x07, 0x17, 0x3F, 0x3F, 0x17])?;
                self.send_cmd_data(0x82, &[0x24])?;
                self.send_cmd_data(0x06, &[0x25, 0x25, 0x3C, 0x37])?;
                self.send_cmd_data(0x30, &[0x09])?;
                self.send_cmd_data(0xE1, &[0x02])?;
                self.planes.fill_all(0xFF);
                self.write_plane_const(0x10, 0xFF)?;
                Self::write_plane_buf(&mut self.pins, &mut self.spi, 0x13, &self.planes.new_plane)?;
                self.hv_awake = false;
            }
            EpdSilicon::Uc8279 => {
                // PSR (0x00): register LUTs enabled (`REG=1`), 792x528 window via PTIN/PTL
                self.send_cmd_data(0x00, &[0x3F, 0x4A])?;
                self.send_cmd(0x91)?;
                self.send_cmd_data(0x90, &UC8279_PTL_WINDOW)?;
                self.send_cmd_data(0x03, &[0x20])?;
                self.send_cmd_data(0x01, &[0x43, 0x00, 0x78, 0x78, 0x17])?;
                self.send_cmd_data(0x82, &[0x24])?;
                self.send_cmd_data(0x06, &[0x25, 0x25, 0x3C])?;
                self.send_cmd_data(0x30, &[0x0F])?;
                self.send_cmd_data(0xE1, &[0x02])?;
                self.hv_awake = false;
            }
        }
        Ok(())
    }

    /// Powers down the high-voltage EPD charge pumps (`POF 0x02`) while retaining display image.
    pub fn set_hv_standby(&mut self, standby: bool) -> Result<(), HalError> {
        self.finish_pending_refresh()?;
        if standby && self.hv_awake {
            self.send_cmd(0x02)?;
            self.wait_busy_two_phase(5_000)?;
            self.hv_awake = false;
            self.planes.release_gray_lsb();
        } else if !standby && !self.hv_awake {
            if self.sleeping {
                self.init_panel()?;
            }
            self.send_cmd(0x04)?;
            self.wait_busy_two_phase(5_000)?;
            self.hv_awake = true;
        }
        Ok(())
    }

    /// Enters deep panel sleep (`POF 0x02` followed by `DSLP 0x07 [0xA5]`).
    pub fn enter_deep_sleep(&mut self) -> Result<(), HalError> {
        let r1 = self.finish_pending_refresh();
        let r2 = self.set_hv_standby(true);
        if self.hv_awake {
            let _ = self.send_cmd(0x02);
            self.hv_awake = false;
        }
        self.planes.release_gray_lsb();
        let r3 = self.send_cmd_data(0x07, &[0xA5]);
        self.sleeping = true;
        r1.and(r2).and(r3)
    }

    fn flush_mode(&mut self, mode: RefreshMode) -> Result<(), HalError> {
        self.finish_pending_refresh()?;
        self.spi.set_frequency_hz(20_000_000);
        if self.sleeping {
            self.init_panel()?;
        }

        let eff_mode = match mode {
            RefreshMode::Grayscale4 if self.planes.gray_lsb_plane.is_none() => RefreshMode::HalfScrub,
            RefreshMode::FastDu if self.first_refresh => RefreshMode::FullGc,
            RefreshMode::FastDu if self.last_was_gray4 || !self.hv_awake => RefreshMode::HalfScrub,
            other => other,
        };

        match self.silicon {
            EpdSilicon::Uc8253 => {
                if matches!(eff_mode, RefreshMode::Grayscale4) {
                    // Matches Uc8253X3Driver _directGrayPass: no 0x11 inside PTIN
                    self.send_cmd_data(0x00, &[0x3F, 0x0A])?;

                    self.send_cmd(0x91)?;
                    self.send_cmd_data(0x90, &UC8253_PTL_WINDOW)?;
                    self.send_cmd(0x10)?;
                    let lsb = self
                        .planes
                        .gray_lsb_plane
                        .as_deref()
                        .unwrap_or(&self.planes.new_plane);
                    Self::send_data_bus(&mut self.pins, &mut self.spi, lsb)?;
                    self.send_cmd(0x92)?;

                    self.send_cmd(0x91)?;
                    self.send_cmd_data(0x90, &UC8253_PTL_WINDOW)?;
                    self.send_cmd(0x13)?;
                    Self::send_data_bus(&mut self.pins, &mut self.spi, &self.planes.new_plane)?;
                    self.send_cmd(0x92)?;

                    self.send_cmd_data(0x50, &[0xA9, 0x07])?;
                    self.load_luts42(&UC8253_LUT_GRAY4)?;

                    self.send_cmd(0x91)?;
                    self.send_cmd_data(0x90, &UC8253_PTL_WINDOW)?;
                    if !self.hv_awake {
                        self.set_hv_standby(false)?;
                    }
                    self.send_cmd(0x12)?;
                    self.first_refresh = false;
                    self.last_was_gray4 = true;
                    if self.async_refresh {
                        for _ in 0..60 {
                            if self.pins.is_busy() {
                                break;
                            }
                            self.delay.delay_us(250);
                        }
                        self.refresh_in_flight = true;
                        self.pending_ptout = true;
                        self.pending_sync_dtm1 = false;
                        self.pending_full_settle = false;
                    } else {
                        self.wait_busy_two_phase(8_000)?;
                        self.send_cmd(0x92)?;
                    }
                } else {
                    // Matches Uc8253X3Driver::displayStart + displayFinish (zero old_plane RAM)
                    let full_sync = matches!(eff_mode, RefreshMode::FullGc);
                    let half_sync = matches!(eff_mode, RefreshMode::HalfScrub);
                    let cdi: [u8; 2] = if half_sync { [0xA9, 0x07] } else { [0x29, 0x07] };
                    let luts = if full_sync {
                        &UC8253_LUT_FULL_GC
                    } else if half_sync {
                        &UC8253_LUT_HALF_SCRUB
                    } else {
                        &UC8253_LUT_FAST_DU
                    };
                    self.send_cmd_data(0x50, &cdi)?;
                    self.load_luts42(luts)?;

                    if full_sync {
                        self.write_plane_const(0x10, 0xFF)?;
                    }
                    Self::write_plane_buf(&mut self.pins, &mut self.spi, 0x13, &self.planes.new_plane)?;

                    if !self.hv_awake {
                        self.set_hv_standby(false)?;
                    }
                    self.send_cmd(0x12)?;
                    self.first_refresh = false;
                    self.last_was_gray4 = false;
                    self.pending_ptout = false;
                    self.pending_sync_dtm1 = true;
                    self.pending_full_settle = full_sync;
                    if self.async_refresh {
                        for _ in 0..60 {
                            if self.pins.is_busy() {
                                break;
                            }
                            self.delay.delay_us(250);
                        }
                        self.refresh_in_flight = true;
                    } else {
                        self.wait_busy_two_phase(8_000)?;
                        self.refresh_in_flight = true;
                        self.finish_pending_refresh()?;
                    }
                }
            }
            EpdSilicon::Uc8279 => {
                // Enter partial window (0x91 PTIN + 0x90 PTL)
                self.send_cmd(0x91)?;
                self.send_cmd_data(0x90, &UC8279_PTL_WINDOW)?;

                // DTM1 (0x10) followed by DSP (0x11)
                if matches!(eff_mode, RefreshMode::Grayscale4) {
                    let lsb = self
                        .planes
                        .gray_lsb_plane
                        .as_deref()
                        .unwrap_or(&self.planes.new_plane);
                    Self::write_plane_buf(&mut self.pins, &mut self.spi, 0x10, lsb)?;
                } else if matches!(eff_mode, RefreshMode::HalfScrub) {
                    Self::write_plane_inv(&mut self.pins, &mut self.spi, 0x10, &self.planes.new_plane)?;
                } else if matches!(eff_mode, RefreshMode::FullGc) || self.first_refresh {
                    self.write_plane_const(0x10, 0xFF)?;
                }

                // DTM2 (0x13) followed by DSP (0x11)
                Self::write_plane_buf(&mut self.pins, &mut self.spi, 0x13, &self.planes.new_plane)?;

                // CDI (0x50): 0x97 on first refresh after init, 0xD7 thereafter
                let cdi = if self.first_refresh { 0x97 } else { 0xD7 };
                self.send_cmd_data(0x50, &[cdi])?;

                // Load register LUT (0x20..=0x24)
                match eff_mode {
                    RefreshMode::FullGc => self.load_luts42(&UC8279_LUT_FULL_GC)?,
                    RefreshMode::HalfScrub => self.load_luts42(&UC8279_LUT_HALF_SCRUB)?,
                    RefreshMode::FastDu => self.load_luts42(&UC8279_LUT_FAST_DU)?,
                    RefreshMode::Grayscale4 => {
                        for (cmd, lut) in &UC8279_LUT_GRAY4 {
                            self.send_cmd_data(*cmd, lut)?;
                        }
                    }
                }

                if !self.hv_awake {
                    self.set_hv_standby(false)?;
                }
                // DRF (0x12), wait idle, then PTOUT (0x92)
                self.send_cmd(0x12)?;
                self.first_refresh = false;
                self.last_was_gray4 = matches!(eff_mode, RefreshMode::Grayscale4);
                self.pending_ptout = true;
                self.pending_sync_dtm1 = !self.last_was_gray4;
                self.pending_full_settle = false;
                if self.async_refresh {
                    for _ in 0..60 {
                        if self.pins.is_busy() {
                            break;
                        }
                        self.delay.delay_us(250);
                    }
                    self.refresh_in_flight = true;
                } else {
                    self.wait_busy_two_phase(8_000)?;
                    self.refresh_in_flight = true;
                    self.finish_pending_refresh()?;
                }
            }
        }
        Ok(())
    }
}

impl<SPI, PINS, D> DisplaySink for X3EpdDriver<SPI, PINS, D>
where
    SPI: SpiBus,
    PINS: EpdPins,
    D: DelayUs,
{
    fn begin_frame(&mut self, mode: RefreshMode) {
        self.active_mode = mode;
        if matches!(mode, RefreshMode::Grayscale4) {
            if let Some(ref mut lsb) = self.planes.gray_lsb_plane {
                lsb.fill(0xFF);
            } else {
                let mut lsb = Vec::new();
                if lsb.try_reserve_exact(EPD_PLANE_BYTES).is_ok() {
                    lsb.resize(EPD_PLANE_BYTES, 0xFF);
                    self.planes.gray_lsb_plane = Some(lsb);
                }
            }
            self.planes.new_plane.fill(0xFF);
        } else {
            self.planes.release_gray_lsb();
            self.planes.new_plane.fill(0xFF);
        }
    }

    fn write_strip_2bpp(&mut self, y_start: usize, rows: usize, packed_2bpp: &[u8]) {
        let gs4 = matches!(self.active_mode, RefreshMode::Grayscale4)
            && self.planes.gray_lsb_plane.is_some();
        let avail_rows = rows
            .min(PORTRAIT_H.saturating_sub(y_start))
            .min(packed_2bpp.len() / ROW_2BPP_BYTES);
        if !gs4 && !self.rotated_180 && (y_start & 7) == 0 && avail_rows.is_multiple_of(8) {
            let blocks = avail_rows / 8;
            for blk in 0..blocks {
                let block_row0 = blk * 8;
                let col_byte = (y_start + block_row0) >> 3;
                for byte_x in 0..ROW_2BPP_BYTES {
                    let mut out0 = 0xFFu8;
                    let mut out1 = 0xFFu8;
                    let mut out2 = 0xFFu8;
                    let mut out3 = 0xFFu8;
                    for dy_sub in 0..8 {
                        let b = packed_2bpp[(block_row0 + dy_sub) * ROW_2BPP_BYTES + byte_x];
                        // MSB of each 2-bit tone is 1 iff light (Silver=2, Paper=3); 0 iff dark (Ink=0, Charcoal=1).
                        if (b & 0xAA) != 0xAA {
                            let clear_mask = !(0x80u8 >> dy_sub);
                            if (b & 0x80) == 0 {
                                out0 &= clear_mask;
                            }
                            if (b & 0x20) == 0 {
                                out1 &= clear_mask;
                            }
                            if (b & 0x08) == 0 {
                                out2 &= clear_mask;
                            }
                            if (b & 0x02) == 0 {
                                out3 &= clear_mask;
                            }
                        }
                    }
                    let px = byte_x * 4;
                    self.planes.new_plane[px * NATIVE_ROW_BYTES + col_byte] = out0;
                    self.planes.new_plane[(px + 1) * NATIVE_ROW_BYTES + col_byte] = out1;
                    self.planes.new_plane[(px + 2) * NATIVE_ROW_BYTES + col_byte] = out2;
                    self.planes.new_plane[(px + 3) * NATIVE_ROW_BYTES + col_byte] = out3;
                }
            }
            return;
        }
        for dy in 0..avail_rows {
            let py = y_start + dy;
            let row_off = dy * ROW_2BPP_BYTES;
            if row_off + ROW_2BPP_BYTES > packed_2bpp.len() {
                break;
            }
            for byte_x in 0..ROW_2BPP_BYTES {
                let packed = packed_2bpp[row_off + byte_x];
                let base_px = byte_x * 4;
                for sub in 0..4 {
                    let shift = 6 - sub * 2;
                    let tone = Tone::from_bits(packed >> shift);
                    let px = base_px + sub;
                    let (nx, ny) = portrait_to_native(px, py, self.rotated_180);
                    if gs4 {
                        // Tone::Ink=0b00, Charcoal=0b01, Silver=0b10, Paper=0b11
                        // gray_lsb_plane (0x10 / DTM1) holds LSB, new_plane (0x13 / DTM2) holds MSB:
                        // (0,0)=Ink (0x24), (1,0)=Charcoal (0x23), (0,1)=Silver (0x22), (1,1)=Paper (0x21).
                        let bits = tone as u8;
                        if let Some(ref mut lsb) = self.planes.gray_lsb_plane {
                            X3EpdPlanes::set_bit(lsb, nx, ny, (bits & 1) != 0);
                        }
                        X3EpdPlanes::set_bit(&mut self.planes.new_plane, nx, ny, (bits & 2) != 0);
                    } else {
                        let white = !tone.is_dark();
                        X3EpdPlanes::set_bit(&mut self.planes.new_plane, nx, ny, white);
                    }
                }
            }
        }
    }

    fn end_frame(&mut self, mode: RefreshMode) {
        let _ = self.flush_mode(mode);
    }

    fn present_1bpp(&mut self, mode: RefreshMode, packed_1bpp: &[u8]) -> bool {
        let portrait_row_bytes = PORTRAIT_W / 8;
        if packed_1bpp.len() < portrait_row_bytes * PORTRAIT_H {
            return false;
        }
        self.planes.release_gray_lsb();
        self.active_mode = mode;
        for py in 0..PORTRAIT_H {
            let row_slice = &packed_1bpp[py * portrait_row_bytes..(py + 1) * portrait_row_bytes];
            for (bx, &b) in row_slice.iter().enumerate() {
                for bit in 0..8 {
                    let px = bx * 8 + bit;
                    let white = (b & (0x80 >> bit)) != 0;
                    let (nx, ny) = portrait_to_native(px, py, self.rotated_180);
                    X3EpdPlanes::set_bit(&mut self.planes.new_plane, nx, ny, white);
                }
            }
        }
        self.flush_mode(mode).is_ok()
    }
}
