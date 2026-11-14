#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod buttons;
pub mod epd;
pub mod net_portal;
pub mod pins;
pub mod sd_fat32;
pub mod sensors;

pub use buttons::{AdcButtonDecoder, WakePowerGuard};
pub use epd::{EpdSilicon, X3EpdDriver, X3EpdPlanes, probe_silicon};
pub use net_portal::{
    AP_IPV4, AP_SSID, AP_URL, DhcpServer, DnsResponder, HTTP_PORT, HTTP_SLOT_COUNT,
    SmoltcpPortalEngine, StreamingHttpConn, VVDROP_WEB_HTML, VvDropServerState,
    sanitize_book_filename,
};
pub use sd_fat32::{BlockDevice, Fat32Volume, SpiSdhcCard, format_fat32};
pub use sensors::{Bq27220, Ds3231, Qmi8658};

/// Errors produced by hardware bus transactions on Xteink X3.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HalError {
    Bus,
    Timeout,
    BadResponse,
    CardAbsent,
    WriteFault,
    UnsupportedCard,
}

/// Safe SPI bus abstraction shared by the EPD controller and microSD slot.
pub trait SpiBus {
    fn set_frequency_hz(&mut self, hz: u32);
    fn write(&mut self, data: &[u8]) -> Result<(), HalError>;
    fn transfer(&mut self, tx: &[u8], rx: &mut [u8]) -> Result<(), HalError>;
}

/// Dedicated GPIO/control lines for UC8253 / UC8279 E-Ink panels.
pub trait EpdPins {
    fn set_cs(&mut self, high: bool);
    fn set_dc(&mut self, data_mode: bool);
    fn set_rst(&mut self, high: bool);
    fn is_busy(&self) -> bool;
    /// Reads the 3-byte silicon version register (`0x70`) over 3-wire SPI.
    fn read_0x70_3wire(&mut self) -> [u8; 3];
}

/// Safe I2C controller abstraction shared by BQ27220, DS3231, and QMI8658.
pub trait I2cBus {
    fn write(&mut self, addr: u8, bytes: &[u8]) -> Result<(), HalError>;
    fn write_read(&mut self, addr: u8, write: &[u8], read: &mut [u8]) -> Result<(), HalError>;
}

/// Power-gating and chip-select lines for the microSD card slot (`GPIO 13` / `GPIO 12`).
pub trait SdControlPins {
    fn set_power_en(&mut self, on: bool);
    fn set_cs(&mut self, high: bool);
}

/// Microsecond blocking delay provider.
pub trait DelayUs {
    fn delay_us(&mut self, us: u32);
}
