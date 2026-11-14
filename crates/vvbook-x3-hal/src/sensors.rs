use vvbook_pal::{BatteryStatus, DateTime, InputEvent};

use crate::pins::{BQ27220_I2C_ADDR, DS3231_I2C_ADDR, QMI8658_I2C_ADDR};
use crate::{DelayUs, HalError, I2cBus};

pub const BQ27220_INTER_CMD_US: u32 = 66;

/// TI BQ27220 Li-Ion fuel gauge (`0x55`) with mandatory >= 66 µs inter-command settling delay.
pub struct Bq27220<D> {
    delay: D,
}

impl<D: DelayUs> Bq27220<D> {
    pub fn new(delay: D) -> Self {
        Self { delay }
    }

    fn read_u16_le<I: I2cBus>(&mut self, i2c: &mut I, reg: u8) -> Result<u16, HalError> {
        let mut buf = [0u8; 2];
        i2c.write_read(BQ27220_I2C_ADDR, &[reg], &mut buf)?;
        self.delay.delay_us(BQ27220_INTER_CMD_US);
        Ok(u16::from_le_bytes(buf))
    }

    pub fn read_battery<I: I2cBus>(&mut self, i2c: &mut I) -> Result<BatteryStatus, HalError> {
        let mv = self.read_u16_le(i2c, 0x08)?;
        let ma_raw = self.read_u16_le(i2c, 0x0C)? as i16;
        let soc = self.read_u16_le(i2c, 0x2C)?.min(100) as u8;
        Ok(BatteryStatus {
            percent: soc,
            millivolts: mv,
            current_ma: ma_raw,
            charging: ma_raw > 15,
        })
    }

    /// Reads battery temperature in tenths of a degree Celsius (`0.1°C`).
    pub fn read_temperature_c_x10<I: I2cBus>(&mut self, i2c: &mut I) -> Result<i16, HalError> {
        let raw_dk = self.read_u16_le(i2c, 0x06)?;
        Ok((raw_dk as i32 - 2731) as i16)
    }
}

/// Maxim DS3231 real-time clock (`0x68`) with BCD decoding/encoding.
pub struct Ds3231;

impl Ds3231 {
    #[inline]
    pub const fn bcd_to_bin(val: u8) -> u8 {
        (val & 0x0F) + ((val >> 4) * 10)
    }

    #[inline]
    pub const fn bin_to_bcd(val: u8) -> u8 {
        ((val / 10) << 4) | (val % 10)
    }

    pub fn read_datetime<I: I2cBus>(i2c: &mut I) -> Result<DateTime, HalError> {
        let mut regs = [0u8; 7];
        i2c.write_read(DS3231_I2C_ADDR, &[0x00], &mut regs)?;
        Ok(DateTime {
            second: Self::bcd_to_bin(regs[0] & 0x7F).min(59),
            minute: Self::bcd_to_bin(regs[1] & 0x7F).min(59),
            hour: Self::bcd_to_bin(regs[2] & 0x3F).min(23),
            day: Self::bcd_to_bin(regs[4] & 0x3F).clamp(1, 31),
            month: Self::bcd_to_bin(regs[5] & 0x1F).clamp(1, 12),
            year: 2000 + u16::from(Self::bcd_to_bin(regs[6])),
        })
    }

    pub fn write_datetime<I: I2cBus>(i2c: &mut I, dt: DateTime) -> Result<(), HalError> {
        let yr = dt.year.saturating_sub(2000).min(99) as u8;
        let frame = [
            0x00,
            Self::bin_to_bcd(dt.second.min(59)),
            Self::bin_to_bcd(dt.minute.min(59)),
            Self::bin_to_bcd(dt.hour.min(23)),
            1, // day-of-week
            Self::bin_to_bcd(dt.day.clamp(1, 31)),
            Self::bin_to_bcd(dt.month.clamp(1, 12)),
            Self::bin_to_bcd(yr),
        ];
        i2c.write(DS3231_I2C_ADDR, &frame)
    }
}

/// QST QMI8658 6-axis IMU (`0x6B`) gated strictly off outside the Reader.
#[derive(Clone, Debug)]
pub struct Qmi8658 {
    enabled: bool,
    last_flick_ms: u64,
    rotated_180: bool,
}

impl Default for Qmi8658 {
    fn default() -> Self {
        Self::new()
    }
}

impl Qmi8658 {
    pub const fn new() -> Self {
        Self {
            enabled: false,
            last_flick_ms: 0,
            rotated_180: false,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_enabled<I: I2cBus>(&mut self, i2c: &mut I, enabled: bool) -> Result<(), HalError> {
        if enabled {
            // CTRL1: AUTO_INC (0x40), oscillator awake; CTRL2: 4g, ~62.5 Hz ODR; CTRL7: accel only
            i2c.write(QMI8658_I2C_ADDR, &[0x02, 0x40])?;
            i2c.write(QMI8658_I2C_ADDR, &[0x03, 0x25])?;
            i2c.write(QMI8658_I2C_ADDR, &[0x08, 0x01])?;
        } else {
            // Power-down accel/gyro (CTRL7 = 0x00) and gate internal oscillator (CTRL1 = 0x41)
            i2c.write(QMI8658_I2C_ADDR, &[0x08, 0x00])?;
            i2c.write(QMI8658_I2C_ADDR, &[0x02, 0x41])?;
        }
        self.enabled = enabled;
        Ok(())
    }

    /// Reads 3-axis acceleration (`0x35..=0x3A`) ONLY when `enabled == true`.
    /// Returns wrist-flick page turns (`>= 600 ms` cooldown) or `OrientationFlip`.
    pub fn poll_event<I: I2cBus>(
        &mut self,
        i2c: &mut I,
        now_ms: u64,
    ) -> Result<Option<InputEvent>, HalError> {
        if !self.enabled {
            return Ok(None);
        }
        let mut raw = [0u8; 6];
        i2c.write_read(QMI8658_I2C_ADDR, &[0x35], &mut raw)?;
        let ax = i16::from_le_bytes([raw[0], raw[1]]);
        let ay = i16::from_le_bytes([raw[2], raw[3]]);

        // 4g scale => ~8192 LSB/g. Wrist flick along lateral X axis >= 0.75g (~6144 LSB)
        if now_ms.saturating_sub(self.last_flick_ms) >= 600 {
            if ax > 6_200 {
                self.last_flick_ms = now_ms;
                return Ok(Some(if self.rotated_180 {
                    InputEvent::TiltFlickPrev
                } else {
                    InputEvent::TiltFlickNext
                }));
            } else if ax < -6_200 {
                self.last_flick_ms = now_ms;
                return Ok(Some(if self.rotated_180 {
                    InputEvent::TiltFlickNext
                } else {
                    InputEvent::TiltFlickPrev
                }));
            }
        }

        // Upside-down portrait hysteresis on vertical Y axis
        if !self.rotated_180 && ay > 6_800 {
            self.rotated_180 = true;
            return Ok(Some(InputEvent::OrientationFlip(true)));
        } else if self.rotated_180 && ay < -6_800 {
            self.rotated_180 = false;
            return Ok(Some(InputEvent::OrientationFlip(false)));
        }

        Ok(None)
    }
}
