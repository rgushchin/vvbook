/// Xteink X3 (ESP32-C3) verified hardware pin map and I2C addresses.
pub const SPI_SCLK: u8 = 8;
pub const SPI_MOSI: u8 = 10;
pub const SPI_MISO: u8 = 7;

pub const EPD_CS: u8 = 21;
pub const EPD_DC: u8 = 4;
pub const EPD_RST: u8 = 5;
pub const EPD_BUSY: u8 = 6;

pub const SD_CS: u8 = 12;
pub const SD_POWER_EN: u8 = 13;

pub const I2C_SDA: u8 = 20;
pub const I2C_SCL: u8 = 0;

pub const ADC_BTN_A: u8 = 1;
pub const ADC_BTN_B: u8 = 2;
pub const PWR_BTN: u8 = 3;

pub const BQ27220_I2C_ADDR: u8 = 0x55;
pub const DS3231_I2C_ADDR: u8 = 0x68;
pub const QMI8658_I2C_ADDR: u8 = 0x6B;

pub const EPD_NATIVE_W: usize = 792;
pub const EPD_NATIVE_H: usize = 528;
pub const EPD_PLANE_BYTES: usize = (EPD_NATIVE_W / 8) * EPD_NATIVE_H; // 99 * 528 = 52,272 bytes
