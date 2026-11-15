use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;

use vvbook_pal::{
    Button, DateTime, DisplaySink, InputEvent, PORTRAIT_H, PORTRAIT_W, ROW_2BPP_BYTES, RefreshMode,
    StorageBackend, Tone,
};
use vvbook_security::AtomicJournal;
use vvbook_x3_hal::epd::{NATIVE_ROW_BYTES, X3EpdPlanes, portrait_to_native};
use vvbook_x3_hal::pins::{BQ27220_I2C_ADDR, DS3231_I2C_ADDR, QMI8658_I2C_ADDR};
use vvbook_x3_hal::{
    ActivePowerGovernor, AdcButtonDecoder, Bq27220, CpuPowerMode, DelayUs, Ds3231, EpdPins,
    EpdSilicon, Fat32Volume, HalError, I2cBus, Qmi8658, SdControlPins, SpiBus, SpiSdhcCard,
    X3EpdDriver, format_fat32, probe_silicon,
};

#[derive(Default, Clone)]
struct SharedDelayLog {
    delays_us: Rc<RefCell<Vec<u32>>>,
}

impl DelayUs for SharedDelayLog {
    fn delay_us(&mut self, us: u32) {
        self.delays_us.borrow_mut().push(us);
    }
}

type SpiWriteLog = Rc<RefCell<Vec<(bool, Vec<u8>)>>>;

struct RecordingEpdSpi {
    log: SpiWriteLog, // (is_data, bytes)
    dc_data: Rc<RefCell<bool>>,
    freq_hz: Rc<RefCell<u32>>,
}

impl SpiBus for RecordingEpdSpi {
    fn set_frequency_hz(&mut self, hz: u32) {
        *self.freq_hz.borrow_mut() = hz;
    }

    fn write(&mut self, data: &[u8]) -> Result<(), HalError> {
        self.log
            .borrow_mut()
            .push((*self.dc_data.borrow(), data.to_vec()));
        Ok(())
    }

    fn transfer(&mut self, tx: &[u8], rx: &mut [u8]) -> Result<(), HalError> {
        self.write(tx)?;
        rx.fill(0xFF);
        Ok(())
    }
}

struct MockEpdPins {
    ver: [u8; 3],
    dc_data: Rc<RefCell<bool>>,
    rst_high: bool,
    cs_high: bool,
    busy_seq: RefCell<VecDeque<bool>>,
}

impl MockEpdPins {
    fn new(ver: [u8; 3], dc_data: Rc<RefCell<bool>>) -> Self {
        Self {
            ver,
            dc_data,
            rst_high: true,
            cs_high: true,
            busy_seq: RefCell::new(VecDeque::new()),
        }
    }
}

impl EpdPins for MockEpdPins {
    fn set_cs(&mut self, high: bool) {
        self.cs_high = high;
    }

    fn set_dc(&mut self, data_mode: bool) {
        *self.dc_data.borrow_mut() = data_mode;
    }

    fn set_rst(&mut self, high: bool) {
        self.rst_high = high;
    }

    fn is_busy(&self) -> bool {
        self.busy_seq.borrow_mut().pop_front().unwrap_or(false)
    }

    fn read_0x70_3wire(&mut self) -> [u8; 3] {
        self.ver
    }
}

#[test]
fn epd_silicon_probe_plane_flip_and_pof_sleep() {
    assert_eq!(probe_silicon([0x01, 0x02, 0x66]), EpdSilicon::Uc8279);
    assert_eq!(probe_silicon([0x00, 0x01, 0x02]), EpdSilicon::Uc8279);
    assert_eq!(probe_silicon([0x01, 0x02, 0x00]), EpdSilicon::Uc8253);
    assert_eq!(probe_silicon([0xFF, 0xFF, 0xFF]), EpdSilicon::Uc8253);
    assert_eq!(probe_silicon([0xFF, 0x66, 0xFF]), EpdSilicon::Uc8253);

    for (ver, expected) in [([0x00, 0x00, 0xFF], EpdSilicon::Uc8253), ([0x11, 0x22, 0x66], EpdSilicon::Uc8279)] {
        let log = Rc::new(RefCell::new(Vec::new()));
        let dc_data = Rc::new(RefCell::new(false));
        let freq_hz = Rc::new(RefCell::new(0));
        let spi = RecordingEpdSpi {
            log: Rc::clone(&log),
            dc_data: Rc::clone(&dc_data),
            freq_hz: Rc::clone(&freq_hz),
        };
        let pins = MockEpdPins::new(ver, dc_data);
        let mut drv = X3EpdDriver::new(spi, pins, SharedDelayLog::default()).unwrap();
        assert_eq!(drv.silicon(), expected);
        assert!(!drv.is_hv_awake());

        // Render Grayscale4 strip at y=0 and bottom strip near y=791 verifying all 4 corners
        let mut strip0 = vec![0xFFu8; PORTRAIT_W / 4]; // white row 0
        strip0[0] = ((Tone::Ink as u8) << 6) | 0x3F; // (0,0) = Ink (00)
        strip0[PORTRAIT_W / 4 - 1] = 0xFC | (Tone::Charcoal as u8); // (527,0) = Charcoal (01)

        let mut strip_end = vec![0xFFu8; PORTRAIT_W / 4];
        strip_end[0] = ((Tone::Silver as u8) << 6) | 0x3F; // (0,791) = Silver (10)

        log.borrow_mut().clear();
        drv.begin_frame(RefreshMode::Grayscale4);
        drv.write_strip_2bpp(0, 1, &strip0);
        drv.write_strip_2bpp(PORTRAIT_H - 1, 1, &strip_end);
        drv.end_frame(RefreshMode::Grayscale4);
        assert!(drv.is_hv_awake());

        let lsb_plane = drv.planes().gray_lsb_plane.as_deref().unwrap();
        let (nx0, ny0) = portrait_to_native(0, 0, false);
        assert_eq!((nx0, ny0), (0, 0));
        assert!(!X3EpdPlanes::get_native_bit(lsb_plane, nx0, ny0));
        assert!(!X3EpdPlanes::get_native_bit(&drv.planes().new_plane, nx0, ny0));

        let (nx1, ny1) = portrait_to_native(PORTRAIT_W - 1, 0, false);
        assert_eq!((nx1, ny1), (0, PORTRAIT_W - 1));
        assert!(X3EpdPlanes::get_native_bit(lsb_plane, nx1, ny1));  // LSB=1 (Charcoal)
        assert!(!X3EpdPlanes::get_native_bit(&drv.planes().new_plane, nx1, ny1)); // MSB=0 (Charcoal)

        let (nxb, nyb) = portrait_to_native(0, PORTRAIT_H - 1, false);
        assert_eq!((nxb, nyb), (PORTRAIT_H - 1, 0));
        assert!(!X3EpdPlanes::get_native_bit(lsb_plane, nxb, nyb)); // LSB=0 (Silver)
        assert!(X3EpdPlanes::get_native_bit(&drv.planes().new_plane, nxb, nyb));  // MSB=1 (Silver)

        assert_eq!(portrait_to_native(0, 0, true), (PORTRAIT_H - 1, PORTRAIT_W - 1));
        assert_eq!(drv.planes().new_plane.len(), NATIVE_ROW_BYTES * 528);

        let cmds: Vec<u8> = log
            .borrow()
            .iter()
            .filter(|(is_data, _)| !*is_data)
            .map(|(_, b)| b[0])
            .collect();
        if expected == EpdSilicon::Uc8279 {
            assert_eq!(
                cmds,
                vec![0x91, 0x90, 0x10, 0x11, 0x13, 0x11, 0x50, 0x20, 0x24, 0x22, 0x23, 0x21, 0x04, 0x12, 0x92]
            );
        } else {
            assert_eq!(
                cmds,
                vec![
                    0x00, 0x91, 0x90, 0x10, 0x92, 0x91, 0x90, 0x13, 0x92, 0x50, 0x20, 0x21, 0x22,
                    0x23, 0x24, 0x91, 0x90, 0x04, 0x12, 0x92
                ]
            );
        }

        // Check POF (0x02) HV standby and DSLP (0x07 0xA5)
        log.borrow_mut().clear();
        drv.set_hv_standby(true).unwrap();
        assert!(!drv.is_hv_awake());
        let cmds: Vec<u8> = log
            .borrow()
            .iter()
            .filter(|(is_data, _)| !*is_data)
            .map(|(_, b)| b[0])
            .collect();
        assert_eq!(cmds, vec![0x02]);

        drv.set_hv_standby(false).unwrap();
        assert!(drv.is_hv_awake());
        drv.enter_deep_sleep().unwrap();
        assert!(!drv.is_hv_awake());
    }
}

#[test]
fn epd_busy_two_phase_waits_for_falling_then_rising_edge() {
    let log = Rc::new(RefCell::new(Vec::new()));
    let dc_data = Rc::new(RefCell::new(false));
    let freq_hz = Rc::new(RefCell::new(0));
    let spi = RecordingEpdSpi {
        log: Rc::clone(&log),
        dc_data: Rc::clone(&dc_data),
        freq_hz,
    };
    let pins = MockEpdPins::new([0x00, 0x00, 0xFF], dc_data);
    let mut drv = X3EpdDriver::new(spi, pins, SharedDelayLog::default()).unwrap();

    // Simulate X3TwoPhase across both PON (0x04) and DRF (0x12):
    // BUSY stays idle (false) briefly, drops active-busy (true), then completes (false).
    drv.pins_mut()
        .busy_seq
        .borrow_mut()
        .extend([false, true, true, false, false, false, true, true, true, false]);

    log.borrow_mut().clear();
    let buf = vec![0xFFu8; PORTRAIT_W / 8 * PORTRAIT_H];
    assert!(drv.present_1bpp(RefreshMode::FullGc, &buf));
    assert!(
        drv.pins_mut().busy_seq.borrow().is_empty(),
        "two-phase BUSY wait must consume the full HIGH -> LOW -> HIGH transition"
    );
    let cmds: Vec<u8> = log
        .borrow()
        .iter()
        .filter(|(is_data, _)| !*is_data)
        .map(|(_, b)| b[0])
        .collect();
    assert_eq!(
        cmds,
        vec![
            // FullGc pass (DTM1 white + DATA_STOP, DTM2 target + DATA_STOP, PON, DRF)
            0x50, 0x20, 0x21, 0x22, 0x23, 0x24, 0x10, 0x11, 0x13, 0x11, 0x04, 0x12,
            // DTM1 sync with current frame + X3 post-FullGc fast settle pass + DTM1 sync
            0x10, 0x11, 0x50, 0x20, 0x21, 0x22, 0x23, 0x24, 0x13, 0x11, 0x12, 0x10, 0x11
        ]
    );
    assert!(drv.planes().gray_lsb_plane.is_none());
}

#[test]
fn epd_fast_tile_strip_matches_rotated_and_single_row_exact() {
    let make_drv = || {
        let log = Rc::new(RefCell::new(Vec::new()));
        let dc_data = Rc::new(RefCell::new(false));
        let freq_hz = Rc::new(RefCell::new(0));
        let spi = RecordingEpdSpi {
            log,
            dc_data: Rc::clone(&dc_data),
            freq_hz,
        };
        let pins = MockEpdPins::new([0x00, 0x00, 0xFF], dc_data);
        X3EpdDriver::new(spi, pins, SharedDelayLog::default()).unwrap()
    };

    let mut drv_fast = make_drv();
    let mut drv_ref = make_drv();

    // Build deterministic full 2-bpp frame covering all 4 tones (Ink=0, Charcoal=1, Silver=2, Paper=3)
    let mut full_2bpp = vec![0u8; ROW_2BPP_BYTES * PORTRAIT_H];
    let mut seed: u32 = 0x1357_9BDF;
    for b in &mut full_2bpp {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        *b = (seed >> 24) as u8;
    }

    // Fast path: 64-row strips (last strip = 24 rows, also multiple of 8)
    drv_fast.begin_frame(RefreshMode::FastDu);
    let mut y = 0usize;
    while y < PORTRAIT_H {
        let rows = (PORTRAIT_H - y).min(64);
        let slice = &full_2bpp[y * ROW_2BPP_BYTES..(y + rows) * ROW_2BPP_BYTES];
        drv_fast.write_strip_2bpp(y, rows, slice);
        y += rows;
    }

    // Reference path: 1-row strips (`avail_rows = 1` is not multiple of 8 -> hits per-pixel fallback)
    drv_ref.begin_frame(RefreshMode::FastDu);
    for row in 0..PORTRAIT_H {
        let slice = &full_2bpp[row * ROW_2BPP_BYTES..(row + 1) * ROW_2BPP_BYTES];
        drv_ref.write_strip_2bpp(row, 1, slice);
    }

    assert_eq!(drv_fast.planes().new_plane, drv_ref.planes().new_plane);
}

#[test]
fn adc_buttons_debounce_longpress_and_180_rotation() {
    let mut dec = AdcButtonDecoder::new();
    let idle = 3100;

    // Exact Xteink X3 unit-calibrated centers:
    // Chin (GPIO 1): Back=2710mV, Confirm=2065mV, Left=1146mV, Right=0mV
    assert_eq!(dec.classify_raw(2710, idle, false), Some(Button::Back));
    assert_eq!(dec.classify_raw(2065, idle, false), Some(Button::Confirm));
    assert_eq!(dec.classify_raw(1146, idle, false), Some(Button::Left));
    assert_eq!(dec.classify_raw(0, idle, false), Some(Button::Right));
    // Side rocker (GPIO 2): Up=1714mV, Down=0mV
    assert_eq!(dec.classify_raw(idle, 1714, false), Some(Button::Up));
    assert_eq!(dec.classify_raw(idle, 0, false), Some(Button::Down));
    assert_eq!(dec.classify_raw(idle, idle, false), None);

    // 10ms spike rejected by 25ms debounce
    assert_eq!(dec.sample(10, 2065, idle, false), None);
    assert_eq!(dec.sample(20, idle, idle, false), None);

    // Hold Confirm (ADC_A ~2065 mV) across 25ms debounce and 650ms LongPress
    assert_eq!(dec.sample(100, 2065, idle, false), None);
    assert_eq!(dec.sample(126, 2065, idle, false), Some(InputEvent::Press(Button::Confirm)));
    assert_eq!(dec.sample(780, 2065, idle, false), Some(InputEvent::LongPress(Button::Confirm)));
    assert_eq!(dec.sample(850, 2065, idle, false), None); // emitted once

    // Release
    assert_eq!(dec.sample(900, idle, idle, false), None);
    assert_eq!(dec.sample(930, idle, idle, false), Some(InputEvent::Release(Button::Confirm)));

    // Power + rotation test for directional keys and 4-button chin inversion
    dec.set_rotated_180(true);
    assert_eq!(dec.classify_raw(2710, idle, false), Some(Button::Right));   // Back -> Right
    assert_eq!(dec.classify_raw(2065, idle, false), Some(Button::Left));    // Confirm -> Left
    assert_eq!(dec.classify_raw(1146, idle, false), Some(Button::Confirm)); // Left -> Confirm
    assert_eq!(dec.classify_raw(0, idle, false), Some(Button::Back));       // Right -> Back
    assert_eq!(dec.classify_raw(idle, 1714, false), Some(Button::Down));    // Up -> Down
    assert_eq!(dec.classify_raw(idle, 0, false), Some(Button::Up));         // Down -> Up
    assert_eq!(dec.classify_raw(idle, idle, true), Some(Button::Power));
}

struct MockI2c {
    temp_dk: u16,
    mv: u16,
    ma: i16,
    soc: u16,
    rtc_regs: [u8; 7],
    imu_ctrl7: u8,
    imu_reads: u32,
    imu_accel: [i16; 3],
}

impl I2cBus for MockI2c {
    fn write(&mut self, addr: u8, bytes: &[u8]) -> Result<(), HalError> {
        match addr {
            DS3231_I2C_ADDR if bytes.len() >= 8 && bytes[0] == 0x00 => {
                self.rtc_regs.copy_from_slice(&bytes[1..8]);
                Ok(())
            }
            QMI8658_I2C_ADDR if bytes.len() == 2 && bytes[0] == 0x08 => {
                self.imu_ctrl7 = bytes[1];
                Ok(())
            }
            QMI8658_I2C_ADDR => Ok(()),
            _ => Err(HalError::Bus),
        }
    }

    fn write_read(&mut self, addr: u8, write: &[u8], read: &mut [u8]) -> Result<(), HalError> {
        match addr {
            BQ27220_I2C_ADDR => {
                let val = match write[0] {
                    0x06 => self.temp_dk,
                    0x08 => self.mv,
                    0x0C => self.ma as u16,
                    0x2C => self.soc,
                    _ => return Err(HalError::BadResponse),
                };
                read[..2].copy_from_slice(&val.to_le_bytes());
                Ok(())
            }
            DS3231_I2C_ADDR => {
                read.copy_from_slice(&self.rtc_regs[..read.len()]);
                Ok(())
            }
            QMI8658_I2C_ADDR => {
                self.imu_reads += 1;
                let mut raw = [0u8; 6];
                raw[0..2].copy_from_slice(&self.imu_accel[0].to_le_bytes());
                raw[2..4].copy_from_slice(&self.imu_accel[1].to_le_bytes());
                raw[4..6].copy_from_slice(&self.imu_accel[2].to_le_bytes());
                read.copy_from_slice(&raw[..read.len()]);
                Ok(())
            }
            _ => Err(HalError::Bus),
        }
    }
}

#[test]
fn sensors_bq27220_ds3231_qmi8658_protocol() {
    let delays = Rc::new(RefCell::new(Vec::new()));
    let mut bq = Bq27220::new(SharedDelayLog {
        delays_us: Rc::clone(&delays),
    });
    let mut i2c = MockI2c {
        temp_dk: 2965, // 23.4 C
        mv: 4120,
        ma: 350,
        soc: 88,
        rtc_regs: [0; 7],
        imu_ctrl7: 0xFF,
        imu_reads: 0,
        imu_accel: [7_200, 0, 8_192],
    };

    let bat = bq.read_battery(&mut i2c).unwrap();
    assert_eq!(bat.percent, 88);
    assert_eq!(bat.millivolts, 4120);
    assert_eq!(bat.current_ma, 350);
    assert!(bat.charging);
    assert_eq!(bq.read_temperature_c_x10(&mut i2c).unwrap(), 234);
    assert!(delays.borrow().iter().all(|&d| d >= 66));
    assert_eq!(delays.borrow().len(), 4);

    let dt = DateTime {
        year: 2026,
        month: 11,
        day: 12,
        hour: 21,
        minute: 45,
        second: 9,
    };
    Ds3231::write_datetime(&mut i2c, dt).unwrap();
    assert_eq!(Ds3231::read_datetime(&mut i2c).unwrap(), dt);

    let mut imu = Qmi8658::new();
    imu.set_enabled(&mut i2c, false).unwrap();
    assert_eq!(i2c.imu_ctrl7, 0x00);
    assert_eq!(imu.poll_event(&mut i2c, 1_000).unwrap(), None);
    assert_eq!(i2c.imu_reads, 0);

    imu.set_enabled(&mut i2c, true).unwrap();
    assert_eq!(i2c.imu_ctrl7, 0x01);
    assert_eq!(imu.poll_event(&mut i2c, 1_000).unwrap(), Some(InputEvent::TiltFlickNext));
    // Cooldown prevents double trigger within 600ms
    assert_eq!(imu.poll_event(&mut i2c, 1_200).unwrap(), None);
}

#[derive(Clone)]
struct SimSdPins {
    power_on: Rc<RefCell<bool>>,
    cs_high: Rc<RefCell<bool>>,
}

impl SdControlPins for SimSdPins {
    fn set_power_en(&mut self, on: bool) {
        *self.power_on.borrow_mut() = on;
    }

    fn set_cs(&mut self, high: bool) {
        *self.cs_high.borrow_mut() = high;
    }
}

enum SdSpiState {
    Idle,
    WriteData { lba: u32, buf: Vec<u8> },
}

/// Wire-accurate SPI SDHC simulator (supports 16 GB sparse block map and enforces `GPIO 13` power!).
struct SimSdhcSpi {
    power_on: Rc<RefCell<bool>>,
    cs_high: Rc<RefCell<bool>>,
    initialized: bool,
    acmd_next: bool,
    cmd_buf: Vec<u8>,
    tx_queue: VecDeque<u8>,
    state: SdSpiState,
    sectors: BTreeMap<u32, [u8; 512]>,
}

impl SimSdhcSpi {
    fn new(power_on: Rc<RefCell<bool>>, cs_high: Rc<RefCell<bool>>) -> Self {
        Self {
            power_on,
            cs_high,
            initialized: false,
            acmd_next: false,
            cmd_buf: Vec::new(),
            tx_queue: VecDeque::new(),
            state: SdSpiState::Idle,
            sectors: BTreeMap::new(),
        }
    }

    fn feed_byte(&mut self, b: u8) -> u8 {
        if !*self.power_on.borrow() {
            self.initialized = false;
            self.cmd_buf.clear();
            self.tx_queue.clear();
            self.state = SdSpiState::Idle;
            return 0xFF;
        }
        if *self.cs_high.borrow() {
            return 0xFF;
        }

        if let Some(resp) = self.tx_queue.pop_front() {
            return resp;
        }

        if let SdSpiState::WriteData { lba, ref mut buf } = self.state {
            if buf.is_empty() && b == 0xFF {
                return 0xFF;
            }
            buf.push(b);
            // 1 token (0xFE) + 512 bytes data + 2 bytes CRC = 515 bytes
            if buf.len() == 515 {
                let mut sec = [0u8; 512];
                sec.copy_from_slice(&buf[1..513]);
                self.sectors.insert(lba, sec);
                self.state = SdSpiState::Idle;
                self.tx_queue.push_back(0x05); // data accepted
                self.tx_queue.push_back(0x00); // busy 1 byte
                self.tx_queue.push_back(0xFF); // ready
            }
            return 0xFF;
        }

        if self.cmd_buf.is_empty() {
            if b == 0xFF {
                return 0xFF;
            }
            self.cmd_buf.push(b);
            return 0xFF;
        }

        self.cmd_buf.push(b);
        if self.cmd_buf.len() < 6 {
            return 0xFF;
        }

        let cmd = self.cmd_buf[0] & 0x3F;
        let arg = u32::from_be_bytes([
            self.cmd_buf[1],
            self.cmd_buf[2],
            self.cmd_buf[3],
            self.cmd_buf[4],
        ]);
        self.cmd_buf.clear();

        if self.acmd_next {
            self.acmd_next = false;
            if cmd == 41 {
                self.initialized = true;
                self.tx_queue.push_back(0x00);
                return 0xFF;
            }
        }

        match cmd {
            0 => {
                self.initialized = false;
                self.tx_queue.push_back(0x01);
            }
            8 => {
                self.tx_queue.extend([0x01, 0x00, 0x00, 0x01, 0xAA]);
            }
            55 => {
                self.acmd_next = true;
                self.tx_queue.push_back(0x01);
            }
            58 => {
                // CCS = 1 (SDHC)
                self.tx_queue.extend([0x00, 0xC0, 0xFF, 0x80, 0x00]);
            }
            17 => {
                if !self.initialized {
                    self.tx_queue.push_back(0x05);
                } else {
                    self.tx_queue.push_back(0x00);
                    self.tx_queue.push_back(0xFE);
                    let sec = self.sectors.get(&arg).copied().unwrap_or([0u8; 512]);
                    self.tx_queue.extend(sec);
                    self.tx_queue.extend([0xAA, 0x55]); // CRC16
                }
            }
            24 => {
                if !self.initialized {
                    self.tx_queue.push_back(0x05);
                } else {
                    self.tx_queue.push_back(0x00);
                    self.state = SdSpiState::WriteData {
                        lba: arg,
                        buf: Vec::new(),
                    };
                }
            }
            _ => self.tx_queue.push_back(0x05),
        }
        0xFF
    }
}

impl SpiBus for SimSdhcSpi {
    fn set_frequency_hz(&mut self, _hz: u32) {}

    fn write(&mut self, data: &[u8]) -> Result<(), HalError> {
        for &b in data {
            self.feed_byte(b);
        }
        Ok(())
    }

    fn transfer(&mut self, tx: &[u8], rx: &mut [u8]) -> Result<(), HalError> {
        for (i, &b) in tx.iter().enumerate() {
            rx[i] = self.feed_byte(b);
        }
        Ok(())
    }
}

#[test]
fn sdhc_16gb_fat32_vfs_long_cyrillic_and_power_gate_reinit() {
    let power_on = Rc::new(RefCell::new(false));
    let cs_high = Rc::new(RefCell::new(true));
    let spi = SimSdhcSpi::new(Rc::clone(&power_on), Rc::clone(&cs_high));
    let pins = SimSdPins {
        power_on: Rc::clone(&power_on),
        cs_high,
    };

    // 16 GB SDHC geometry = 31,250,000 sectors (32 KB clusters = 64 sectors/cluster)
    let total_sectors = 31_250_000u32;
    let mut card = SpiSdhcCard::new(spi, pins, SharedDelayLog::default(), total_sectors);
    assert!(!card.is_powered());

    format_fat32(&mut card, total_sectors, b"VVBOOK_16GB").unwrap();
    assert!(card.is_sdhc());
    assert_eq!(card.power_on_cycles, 1);

    let mut vol = Fat32Volume::mount(card).unwrap();
    vol.mkdir_all("/.vvbook/layout").unwrap();
    vol.mkdir_all("/books").unwrap();

    // Multi-cluster payload (72 KB across three 32 KB clusters) with Cyrillic long filename
    let book_path = "/books/Сказка_о_царе_Салтане.fb2.zip";
    let payload: Vec<u8> = (0..72_000usize).map(|i| ((i * 37 + 11) & 0xFF) as u8).collect();
    vol.write_atomic(book_path, &payload).unwrap();

    // Gate GPIO 13 OFF -> verify SPI card loses power & init state
    vol.set_power_save(true);
    vol.with_device(|c| {
        assert!(!c.is_powered());
        assert!(!c.is_initialized());
        assert_eq!(c.power_off_cycles, 1);
    });
    assert!(!*power_on.borrow());

    // Now issue read_range & list_dir WITHOUT manual wake: card must power on and re-init cleanly!
    let mut slice = [0u8; 256];
    let n = vol.read_range(book_path, 40_000, &mut slice).unwrap();
    assert_eq!(n, 256);
    assert_eq!(slice.as_slice(), &payload[40_000..40_256]);
    vol.with_device(|c| {
        assert!(c.is_powered());
        assert!(c.is_initialized());
        assert_eq!(c.power_on_cycles, 2);
    });

    let books = vol.list_dir("/books").unwrap();
    assert_eq!(books.len(), 1);
    assert_eq!(books[0].name, "Сказка_о_царе_Салтане.fb2.zip");
    assert_eq!(books[0].size, 72_000);

    // AtomicJournal dual-slot persistence & file append/remove on 16 GB FAT32
    AtomicJournal::commit(&mut vol, "/.vvbook/state", b"folio-v1").unwrap();
    AtomicJournal::commit(&mut vol, "/.vvbook/state", b"folio-v2").unwrap();
    assert_eq!(
        AtomicJournal::load(&vol, "/.vvbook/state").unwrap(),
        b"folio-v2"
    );

    vol.write_all("/books/note.txt", b"hello ").unwrap();
    vol.append("/books/note.txt", b"world").unwrap();
    assert_eq!(vol.read_all("/books/note.txt").unwrap(), b"hello world");
    vol.remove("/books/note.txt").unwrap();
    assert!(!vol.exists("/books/note.txt"));
}

#[test]
fn fat32_streaming_append_multi_cluster_and_mbr_slot_two() {
    use vvbook_x3_hal::BlockDevice;

    let power_on = Rc::new(RefCell::new(false));
    let cs_high = Rc::new(RefCell::new(true));
    let spi = SimSdhcSpi::new(Rc::clone(&power_on), Rc::clone(&cs_high));
    let pins = SimSdPins {
        power_on,
        cs_high,
    };

    let total_sectors = 131_072u32; // 4 KB clusters (8 sectors/cluster)
    let mut card = SpiSdhcCard::new(spi, pins, SharedDelayLog::default(), total_sectors);
    format_fat32(&mut card, total_sectors, b"VVBOOK_TEST").unwrap();

    // Move MBR partition 0 entry into MBR partition slot 1 (446 + 16) to test all-4-slot probing
    let mut mbr = [0u8; 512];
    card.read_sector(0, &mut mbr).unwrap();
    let entry0 = mbr[446..462].to_vec();
    mbr[446..462].fill(0);
    mbr[462..478].copy_from_slice(&entry0);
    card.write_sector(0, &mbr).unwrap();

    let mut vol = Fat32Volume::mount(card).unwrap();
    let mut expected = Vec::new();
    for step in 0..20usize {
        let len = if step % 2 == 0 { 777 } else { 4096 };
        let chunk: Vec<u8> = (0..len)
            .map(|i| (((step * 131) + i * 17) & 0xFF) as u8)
            .collect();
        vol.append("/stream.bin", &chunk).unwrap();
        expected.extend_from_slice(&chunk);
    }
    assert_eq!(vol.file_len("/stream.bin").unwrap(), expected.len() as u64);
    let mut actual = vec![0u8; expected.len()];
    let mut off = 0usize;
    while off < expected.len() {
        let take = (expected.len() - off).min(2048);
        let n = vol.read_range("/stream.bin", off as u64, &mut actual[off..off + take]).unwrap();
        assert_eq!(n, take);
        off += n;
    }
    assert_eq!(actual, expected);
}

#[test]
fn active_power_governor_holdoff_wifi_io_and_predebounce_touch() {
    let mut gov = ActivePowerGovernor::new(100);
    assert_eq!(gov.current_mode(), CpuPowerMode::Turbo160MHz);
    assert_eq!(gov.current_mode().awake_mhz(), 160);
    assert_eq!(gov.current_mode().idle_apb_mhz(), 80);
    assert!(!gov.current_mode().uses_wfi());

    // Stay in Turbo throughout the 250 ms boot UI holdoff window.
    assert_eq!(gov.select_mode(349, false, false), CpuPowerMode::Turbo160MHz);
    assert_eq!(gov.eco_entries(), 0);

    // Wi-Fi or pending continuous work keeps Turbo pinned even past 250 ms.
    assert_eq!(gov.select_mode(350, true, false), CpuPowerMode::Turbo160MHz);
    assert_eq!(gov.select_mode(350, false, true), CpuPowerMode::Turbo160MHz);
    assert_eq!(gov.eco_entries(), 0);

    // Once holdoff expires and Wi-Fi is off -> Eco XTAL + WFI.
    let eco = gov.select_mode(350, false, false);
    assert_eq!(eco, CpuPowerMode::EcoXtal40MHz);
    assert_eq!(eco.awake_mhz(), 40);
    assert_eq!(eco.idle_apb_mhz(), 20);
    assert!(eco.uses_wfi());
    assert_eq!(gov.eco_entries(), 1);
    assert_eq!(gov.select_mode(5_000, false, false), CpuPowerMode::EcoXtal40MHz);
    assert_eq!(gov.eco_entries(), 1);

    // Raw touch/UI input re-boosts immediately to 160 MHz for UI_HOLDOFF_MS (250 ms).
    gov.note_input(5_050, false);
    assert_eq!(gov.current_mode(), CpuPowerMode::Turbo160MHz);
    assert_eq!(gov.turbo_boosts(), 1);
    assert_eq!(gov.boost_until_ms(), 5_300);
    assert_eq!(gov.select_mode(5_299, false, false), CpuPowerMode::Turbo160MHz);
    assert_eq!(gov.select_mode(5_300, false, false), CpuPowerMode::EcoXtal40MHz);
    assert_eq!(gov.eco_entries(), 2);

    // Clean Reading mode input scales back down after READER_HOLDOFF_MS (50 ms).
    gov.note_input(6_000, true);
    assert_eq!(gov.current_mode(), CpuPowerMode::Turbo160MHz);
    assert_eq!(gov.turbo_boosts(), 2);
    assert_eq!(gov.boost_until_ms(), 6_050);
    assert_eq!(gov.select_mode(6_049, false, false), CpuPowerMode::Turbo160MHz);
    assert_eq!(gov.select_mode(6_050, false, false), CpuPowerMode::EcoXtal40MHz);
    assert_eq!(gov.eco_entries(), 3);

    // Entering clean Reading mode clamps any leftover 250 ms UI holdoff down to 50 ms.
    gov.note_input(7_000, false);
    assert_eq!(gov.boost_until_ms(), 7_250);
    gov.note_input(7_020, true);
    assert_eq!(gov.boost_until_ms(), 7_070);
    assert_eq!(gov.select_mode(7_069, false, false), CpuPowerMode::Turbo160MHz);
    assert_eq!(gov.select_mode(7_070, false, false), CpuPowerMode::EcoXtal40MHz);
    assert_eq!(gov.eco_entries(), 4);

    // AdcButtonDecoder::has_active_or_pending triggers on very first raw dip before 14 ms debounce.
    let mut dec = AdcButtonDecoder::new();
    let mut out = Vec::new();
    dec.poll_into(1_000, 3100, 3100, false, &mut out);
    assert!(!dec.has_active_or_pending());
    dec.poll_into(1_001, 250, 3100, false, &mut out); // raw Right dip
    assert!(dec.has_active_or_pending());
    assert!(out.is_empty()); // still pre-debounce
    dec.poll_into(1_020, 250, 3100, false, &mut out);
    assert!(dec.has_active_or_pending());
    assert_eq!(out, vec![InputEvent::Press(Button::Right)]);
    out.clear();
    dec.poll_into(1_021, 3100, 3100, false, &mut out);
    assert!(dec.has_active_or_pending()); // held stable while release debounces
    dec.poll_into(1_040, 3100, 3100, false, &mut out);
    assert!(!dec.has_active_or_pending());
    assert_eq!(out, vec![InputEvent::Release(Button::Right)]);
}

#[test]
fn adc_buttons_suppress_screen_transition_hold_and_ladder_slew() {
    let mut dec = AdcButtonDecoder::new();
    let idle = 3100;
    let mut out = Vec::new();

    // 1. User presses Confirm (2065 mV) to open a book, then keeps holding Confirm
    // past the 650 ms long-press threshold after screen transition suppression.
    dec.poll_into(0, 2065, idle, false, &mut out);
    dec.poll_into(20, 2065, idle, false, &mut out);
    assert_eq!(out, vec![InputEvent::Press(Button::Confirm)]);
    out.clear();

    dec.suppress_until_idle(20, 320);
    for t in (100..=800).step_by(20) {
        dec.poll_into(t, 2065, idle, false, &mut out);
    }
    assert!(
        out.is_empty(),
        "held Confirm across screen transition leaked events: {out:?}"
    );

    // Late release at t=820 followed by a release bounce at t=860 (within 80 ms):
    // must be swallowed cleanly without synthesizing a Confirm press.
    dec.poll_into(820, idle, idle, false, &mut out);
    dec.poll_into(840, idle, idle, false, &mut out);
    dec.poll_into(860, 2065, idle, false, &mut out);
    dec.poll_into(880, 2065, idle, false, &mut out);
    dec.poll_into(900, idle, idle, false, &mut out);
    dec.poll_into(920, idle, idle, false, &mut out);
    assert!(out.is_empty(), "post-release bounce leaked events: {out:?}");

    // Fresh deliberate Confirm press once settled (past t=880+80 -> e.g. t=1020) works normally.
    dec.poll_into(1_020, 2065, idle, false, &mut out);
    dec.poll_into(1_040, 2065, idle, false, &mut out);
    assert_eq!(out, vec![InputEvent::Press(Button::Confirm)]);
    out.clear();
    dec.poll_into(1_060, idle, idle, false, &mut out);
    dec.poll_into(1_080, idle, idle, false, &mut out);
    assert_eq!(out, vec![InputEvent::Release(Button::Confirm)]);
    out.clear();

    // 2. Return-to-Idle gate: slow resistor-ladder release from Left (1146 mV) through
    // Confirm (2065 mV) -> Back (2710 mV) -> Idle (3100 mV) emits ONLY Release(Left).
    dec.poll_into(2_000, 1146, idle, false, &mut out);
    dec.poll_into(2_020, 1146, idle, false, &mut out);
    assert_eq!(out, vec![InputEvent::Press(Button::Left)]);
    out.clear();

    dec.poll_into(2_040, 2065, idle, false, &mut out);
    dec.poll_into(2_060, 2065, idle, false, &mut out);
    dec.poll_into(2_080, 2710, idle, false, &mut out);
    dec.poll_into(2_100, 2710, idle, false, &mut out);
    dec.poll_into(2_120, idle, idle, false, &mut out);
    dec.poll_into(2_140, idle, idle, false, &mut out);
    assert_eq!(
        out,
        vec![InputEvent::Release(Button::Left)],
        "ladder slew synthesized spurious key press: {out:?}"
    );
}

#[test]
fn fat32_remove_rejects_directory_preserves_eod_and_rejects_malformed_bpb() {
    use vvbook_pal::{StorageBackend, StorageError};
    use vvbook_x3_hal::BlockDevice;

    struct VecCard(Vec<[u8; 512]>);
    impl VecCard {
        fn new(sectors: usize) -> Self {
            Self(vec![[0u8; 512]; sectors])
        }
    }
    impl BlockDevice for VecCard {
        fn sector_count(&self) -> u32 {
            self.0.len() as u32
        }
        fn read_sector(&mut self, lba: u32, buf: &mut [u8; 512]) -> Result<(), HalError> {
            let s = self.0.get(lba as usize).ok_or(HalError::Bus)?;
            *buf = *s;
            Ok(())
        }
        fn write_sector(&mut self, lba: u32, buf: &[u8; 512]) -> Result<(), HalError> {
            let s = self.0.get_mut(lba as usize).ok_or(HalError::Bus)?;
            *s = *buf;
            Ok(())
        }
    }

    // 1. Directory removal must return Err(InvalidPath) without freeing child files.
    let total_sectors = 65_536u32;
    let mut card = VecCard::new(total_sectors as usize);
    format_fat32(&mut card, total_sectors, b"VVBOOK_TEST").unwrap();
    let mut vol = Fat32Volume::mount(card).unwrap();
    vol.mkdir_all("/books/shelf").unwrap();
    vol.write_all("/books/shelf/a.txt", b"safe-content").unwrap();
    assert_eq!(vol.remove("/books/shelf"), Err(StorageError::InvalidPath));
    assert_eq!(vol.remove("/books"), Err(StorageError::InvalidPath));
    assert_eq!(vol.read_all("/books/shelf/a.txt").unwrap(), b"safe-content");

    // 2. Inject a stale SFN entry after the root directory's 0x00 EOD marker and allocate across EOD.
    let mut dev2 = VecCard::new(total_sectors as usize);
    format_fat32(&mut dev2, total_sectors, b"VVTEST_EOD2").unwrap();
    // Locate root directory LBA from MBR -> VBR.
    let mut mbr = [0u8; 512];
    dev2.read_sector(0, &mut mbr).unwrap();
    let part_lba = u32::from_le_bytes(mbr[454..458].try_into().unwrap());
    let mut vbr = [0u8; 512];
    dev2.read_sector(part_lba, &mut vbr).unwrap();
    let rsvd = u16::from_le_bytes([vbr[14], vbr[15]]) as u32;
    let num_fats = vbr[16] as u32;
    let fat_sz32 = u32::from_le_bytes(vbr[36..40].try_into().unwrap());
    let sec_per_clus = vbr[13] as u32;
    let root_clus = u32::from_le_bytes(vbr[44..48].try_into().unwrap());
    let root_lba = part_lba + rsvd + num_fats * fat_sz32 + (root_clus - 2) * sec_per_clus;

    // Volume label occupies slot 0; plant a valid-looking stale SFN at slot 5 (offset 160), leaving slot 1 == 0x00 (EOD).
    let mut root_sec = [0u8; 512];
    dev2.read_sector(root_lba, &mut root_sec).unwrap();
    assert_eq!(root_sec[32], 0x00);
    let stale_off = 5 * 32;
    root_sec[stale_off..stale_off + 11].copy_from_slice(b"GHOST   TXT");
    root_sec[stale_off + 11] = 0x20;
    root_sec[stale_off + 26..stale_off + 28].copy_from_slice(&root_clus.to_le_bytes()[..2]);
    dev2.write_sector(root_lba, &root_sec).unwrap();

    let mut vol3 = Fat32Volume::mount(dev2).unwrap();
    assert!(!vol3.exists("/ghost.txt"));
    // Creating a long-named file needs > 4 slots (consumes slot 1..=5, crossing EOD and stale slot 5!).
    vol3.write_all("/long_filename_crossing_stale_eod.txt", b"ok").unwrap();
    let names: Vec<String> = vol3
        .list_dir("/")
        .unwrap()
        .into_iter()
        .map(|e| e.name.to_ascii_lowercase())
        .collect();
    assert!(!names.iter().any(|n| n.contains("ghost")), "stale entry resurrected: {names:?}");
    assert!(names.iter().any(|n| n == "long_filename_crossing_stale_eod.txt"));

    // 3. Overflowing FAT32 BPB fields must fail cleanly with StorageError::Corrupted.
    let mut bad_card = VecCard::new(total_sectors as usize);
    format_fat32(&mut bad_card, total_sectors, b"VVTEST_BAD1").unwrap();
    let mut bad_mbr = [0u8; 512];
    bad_card.read_sector(0, &mut bad_mbr).unwrap();
    let bad_part_lba = u32::from_le_bytes(bad_mbr[454..458].try_into().unwrap());
    let mut bad_vbr = [0u8; 512];
    bad_card.read_sector(bad_part_lba, &mut bad_vbr).unwrap();
    // fat_sz32 = 0x0200_0000 overflows `fat_sz32 * 128` in unchecked u32 arithmetic.
    bad_vbr[36..40].copy_from_slice(&0x0200_0000u32.to_le_bytes());
    bad_card.write_sector(bad_part_lba, &bad_vbr).unwrap();
    assert!(matches!(Fat32Volume::mount(bad_card), Err(StorageError::Corrupted)));
}
