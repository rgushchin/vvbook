#![no_std]
#![no_main]

extern crate alloc;

use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use core::fmt::{self, Write};
use core::panic::PanicInfo;
use core::ptr;

use esp_alloc as _;
use esp_hal::gpio::RtcPinWithResistors;
use esp_hal::peripherals::{GPIO3, LPWR, RNG, TIMG0, WIFI};
use esp_hal::rng::Rng as EspHalRng;
use esp_hal::rtc_cntl::{
    Rtc, Rwdt, RwdtStage, RwdtStageAction, reset_reason,
    sleep::{RtcSleepConfig, RtcioWakeupSource, TimerWakeupSource, WakeupLevel},
    wakeup_cause,
};
use esp_hal::system::{Cpu, software_reset};
use esp_hal::time::Duration as HalDuration;
use esp_hal::timer::timg::TimerGroup;
use esp_wifi::wifi::{
    AccessPointConfiguration, AuthMethod, Configuration, WifiController, WifiDevice,
};
use vvbook_pal::{
    BatteryStatus, DateTime, DeviceSensors, EntropySource, InputEvent, NetworkPortal,
    PortalStatus, PowerControl, RefreshMode, StorageBackend,
};
use vvbook_userspace::{FolioApp, Screen};
use vvbook_x3_hal::pins::{
    EPD_BUSY, EPD_CS, EPD_DC, EPD_RST, I2C_SCL, I2C_SDA, PWR_BTN, SD_CS, SD_POWER_EN, SPI_MISO,
    SPI_MOSI, SPI_SCLK,
};
use vvbook_x3_hal::{
    AP_SSID, AP_URL, AdcButtonDecoder, Bq27220, DelayUs, DhcpServer, Ds3231, EpdPins, Fat32Volume,
    HalError, I2cBus, Qmi8658, SdControlPins, SmoltcpPortalEngine, SpiBus, SpiSdhcCard,
    VvDropServerState, WakePowerGuard, X3EpdDriver,
};

// ============================================================================
// 1. ESP-IDF App Descriptor & Heap Stats
// ============================================================================

#[repr(C)]
pub struct EspAppDesc {
    pub magic_word: u32,
    pub secure_version: u32,
    pub reserv1: [u32; 2],
    pub version: [u8; 32],
    pub project_name: [u8; 32],
    pub time: [u8; 16],
    pub date: [u8; 16],
    pub idf_ver: [u8; 32],
    pub app_elf_sha256: [u8; 32],
    pub min_efuse_blk_rev_full: u16,
    pub max_efuse_blk_rev_full: u16,
    pub mmu_page_size: u8,
    pub reserv3: [u8; 3],
    pub reserv2: [u32; 18],
}

const fn pad_str<const N: usize>(s: &[u8]) -> [u8; N] {
    let mut out = [0u8; N];
    let mut i = 0;
    while i < s.len() && i + 1 < N {
        out[i] = s[i];
        i += 1;
    }
    out
}

const _: [(); 256] = [(); core::mem::size_of::<EspAppDesc>()];

#[used]
#[unsafe(link_section = ".rodata_desc")]
pub static ESP_APP_DESC: EspAppDesc = EspAppDesc {
    magic_word: 0xABCD_5432,
    secure_version: 0,
    reserv1: [0; 2],
    version: pad_str(b"0.6.3"),
    project_name: pad_str(b"vvbook-folio-os"),
    time: pad_str(b"00:00:00"),
    date: pad_str(b"Nov 13 2026"),
    idf_ver: pad_str(b"5.5.5"),
    app_elf_sha256: [0; 32],
    min_efuse_blk_rev_full: 0x0000,
    max_efuse_blk_rev_full: 0x00C7,
    mmu_page_size: 0x10,
    reserv3: [0; 3],
    reserv2: [0; 18],
};

fn heap_stats() -> (usize, usize) {
    let free = esp_alloc::HEAP.free();
    let total = free + esp_alloc::HEAP.used();
    (free, total)
}

// ============================================================================
// 2. ESP32-C3 Low-Level MMIO Registers
// ============================================================================

const SYSTEM_CPU_PER_CONF_REG: usize = 0x600C_0008;
const SYSTEM_PERIP_CLK_EN0_REG: usize = 0x600C_0010;
const SYSTEM_PERIP_CLK_EN1_REG: usize = 0x600C_0014;
const SYSTEM_PERIP_RST_EN0_REG: usize = 0x600C_0018;
const SYSTEM_PERIP_RST_EN1_REG: usize = 0x600C_001C;
const SYSTEM_SYSCLK_CONF_REG: usize = 0x600C_0058;
const ANA_CONF0_REG: usize = 0x6000_E044;
const ANA_CONF1_REG: usize = 0x6000_E048;
const REGI2C_CLK_GATE_REG: usize = 0x600C_2194;
const ROM_I2C_WRITEREG_MASK: usize = 0x4000_1960;

const GPIO_OUT_W1TS_REG: usize = 0x6000_4008;
const GPIO_OUT_W1TC_REG: usize = 0x6000_400C;
const GPIO_ENABLE_W1TS_REG: usize = 0x6000_4024;
const GPIO_ENABLE_W1TC_REG: usize = 0x6000_4028;
const GPIO_IN_REG: usize = 0x6000_403C;
const GPIO_PIN0_REG: usize = 0x6000_4074;
const GPIO_FUNC_IN_SEL_CFG_BASE: usize = 0x6000_4154;
const GPIO_FUNC_OUT_SEL_CFG_BASE: usize = 0x6000_4554;
const IO_MUX_GPIO_BASE: usize = 0x6000_9004;

const TIMG0_WDTCONFIG0: usize = 0x6001_F048;
const TIMG0_WDTWPROTECT: usize = 0x6001_F064;
const TIMG1_WDTCONFIG0: usize = 0x6002_0048;
const TIMG1_WDTWPROTECT: usize = 0x6002_0064;

const RTC_CNTL_DIG_ISO_REG: usize = 0x6000_808C;
const RTC_CNTL_SW_CPU_STALL_REG: usize = 0x6000_80B4;
const RTC_CNTL_REAL_STORE4_REG: usize = 0x6000_80B8;
const RTC_CNTL_STORE6_REG: usize = 0x6000_80C0;
const RTC_CNTL_STORE7_REG: usize = 0x6000_80C4;
const RTC_CNTL_PAD_HOLD_REG: usize = 0x6000_80D0;
const RTC_CNTL_DIG_PAD_HOLD_REG: usize = 0x6000_80D4;
const RTC_CNTL_SENSOR_CTRL_REG: usize = 0x6000_811C;
const PANIC_RTC_MAGIC: u32 = 0x5656_B000;
const SLEEP_ABORT_MAGIC: u32 = 0x5656_5300;

const SYSTIMER_CONF_REG: usize = 0x6002_3000;
const SYSTIMER_UNIT0_OP_REG: usize = 0x6002_3004;
const SYSTIMER_UNIT0_VALUE_HI: usize = 0x6002_3040;
const SYSTIMER_UNIT0_VALUE_LO: usize = 0x6002_3044;

const SPI2_CMD_REG: usize = 0x6002_4000;
const SPI2_CTRL_REG: usize = 0x6002_4008;
const SPI2_CLOCK_REG: usize = 0x6002_400C;
const SPI2_USER_REG: usize = 0x6002_4010;
const SPI2_USER1_REG: usize = 0x6002_4014;
const SPI2_MS_DLEN_REG: usize = 0x6002_401C;
const SPI2_MISC_REG: usize = 0x6002_4020;
const SPI2_SLAVE_REG: usize = 0x6002_4030;
const SPI2_W0_REG: usize = 0x6002_4098;
const SPI2_DMA_CONF_REG: usize = 0x6002_40E0;
const SPI2_CLK_GATE_REG: usize = 0x6002_40E8;

const APB_SARADC_CTRL_REG: usize = 0x6004_0000;
const APB_SARADC_ONETIME_SAMPLE: usize = 0x6004_0020;
const APB_SARADC_1_DATA_STATUS: usize = 0x6004_002C;
const APB_SARADC_INT_RAW: usize = 0x6004_0044;
const APB_SARADC_INT_CLR: usize = 0x6004_004C;
const APB_SARADC_CLKM_CONF_REG: usize = 0x6004_0054;

const RNG_DATA_REG: usize = 0x6002_60B0;

const USB_SERIAL_JTAG_EP1_REG: usize = 0x6004_3000;
const USB_SERIAL_JTAG_EP1_CONF_REG: usize = 0x6004_3004;
const USB_SERIAL_JTAG_FRAM_NUM_REG: usize = 0x6004_3024;

struct UsbSerialLog;

static mut USB_HOST_ONLINE: bool = true;

impl UsbSerialLog {
    fn write_byte(b: u8) {
        if (mmio_read(USB_SERIAL_JTAG_EP1_CONF_REG) & 2) != 0 {
            unsafe { USB_HOST_ONLINE = true };
            mmio_write(USB_SERIAL_JTAG_EP1_REG, u32::from(b));
            if b == b'\n' || (mmio_read(USB_SERIAL_JTAG_EP1_CONF_REG) & 2) == 0 {
                mmio_write(USB_SERIAL_JTAG_EP1_CONF_REG, 1);
            }
            return;
        }
        if unsafe { !USB_HOST_ONLINE } {
            return;
        }
        mmio_write(USB_SERIAL_JTAG_EP1_CONF_REG, 1);
        let t0 = EspSysTimer::now_us();
        while EspSysTimer::now_us().saturating_sub(t0) < 1_500 {
            if (mmio_read(USB_SERIAL_JTAG_EP1_CONF_REG) & 2) != 0 {
                mmio_write(USB_SERIAL_JTAG_EP1_REG, u32::from(b));
                if b == b'\n' || (mmio_read(USB_SERIAL_JTAG_EP1_CONF_REG) & 2) == 0 {
                    mmio_write(USB_SERIAL_JTAG_EP1_CONF_REG, 1);
                }
                return;
            }
        }
        unsafe { USB_HOST_ONLINE = false };
    }

    fn flush() {
        if unsafe { USB_HOST_ONLINE } && (mmio_read(USB_SERIAL_JTAG_EP1_CONF_REG) & 2) != 0 {
            mmio_write(USB_SERIAL_JTAG_EP1_CONF_REG, 1);
        }
    }
}

impl Write for UsbSerialLog {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for &b in s.as_bytes() {
            if b == b'\n' {
                Self::write_byte(b'\r');
            }
            Self::write_byte(b);
        }
        Self::flush();
        Ok(())
    }
}

macro_rules! ulog {
    ($($arg:tt)*) => {{
        let _ = writeln!(UsbSerialLog, $($arg)*);
    }};
}

#[inline]
fn mmio_read(addr: usize) -> u32 {
    unsafe { ptr::read_volatile(addr as *const u32) }
}

#[inline]
fn mmio_write(addr: usize, val: u32) {
    unsafe { ptr::write_volatile(addr as *mut u32, val) }
}

fn disable_timg_watchdogs() {
    mmio_write(TIMG0_WDTWPROTECT, 0x50D8_3AA1);
    mmio_write(TIMG0_WDTCONFIG0, 0);
    mmio_write(TIMG1_WDTWPROTECT, 0x50D8_3AA1);
    mmio_write(TIMG1_WDTCONFIG0, 0);
}

fn arm_rwdt(rtc: &mut Rtc<'_>) {
    rtc.rwdt.enable();
    rtc.rwdt
        .set_stage_action(RwdtStage::Stage0, RwdtStageAction::ResetSystem);
    rtc.rwdt
        .set_timeout(RwdtStage::Stage0, HalDuration::from_millis(12_000));
    rtc.rwdt.feed();
}

#[inline]
fn feed_watchdog() {
    Rwdt::new().feed();
}

fn unhold_all_pads() {
    mmio_write(RTC_CNTL_PAD_HOLD_REG, 0);
    mmio_write(RTC_CNTL_DIG_PAD_HOLD_REG, 0);
    // Clear dg_pad_autohold_en (bit 11) and dg_pad_force_hold (bit 15); pulse clr_dg_pad_autohold (bit 10).
    let iso = mmio_read(RTC_CNTL_DIG_ISO_REG) & !((1 << 11) | (1 << 15));
    mmio_write(RTC_CNTL_DIG_ISO_REG, iso | (1 << 10));
}

fn wait_for_power_release_ms(max_ms: u64) {
    let wait_t0 = EspSysTimer::now_ms();
    let mut high_since: Option<u64> = None;
    while EspSysTimer::now_ms().saturating_sub(wait_t0) < max_ms {
        feed_watchdog();
        let now = EspSysTimer::now_ms();
        if gpio_get(PWR_BTN) {
            let t0 = *high_since.get_or_insert(now);
            if now.saturating_sub(t0) >= 30 {
                break;
            }
        } else {
            high_since = None;
        }
        EspSysTimer.delay_us(5_000);
    }
}

fn enter_hardware_deep_sleep(
    rtc: &mut Rtc<'_>,
    pwr_pin: &mut GPIO3<'static>,
    hold_epd_rst: bool,
    extra_timer: Option<&TimerWakeupSource>,
) -> ! {
    // Keep DIG_PAD_HOLD = 0 and dg_pad_autohold_en = 0 (esp-hal 1.0.0-rc.0's
    // isolate_digital_gpio() clears fun_ie/fun_wpu on GPIO0..5 if enabled).
    mmio_write(RTC_CNTL_DIG_PAD_HOLD_REG, 0);
    mmio_write(
        RTC_CNTL_DIG_ISO_REG,
        mmio_read(RTC_CNTL_DIG_ISO_REG) & !((1 << 11) | (1 << 15)),
    );
    if hold_epd_rst {
        mmio_write(
            RTC_CNTL_PAD_HOLD_REG,
            mmio_read(RTC_CNTL_PAD_HOLD_REG) | (1 << EPD_RST),
        );
    }

    rtc.rwdt.disable();
    UsbSerialLog::flush();

    let wake_level = if gpio_get(PWR_BTN) {
        WakeupLevel::Low
    } else {
        WakeupLevel::High
    };
    let wakeup_pins: &mut [(&mut dyn RtcPinWithResistors, WakeupLevel)] =
        &mut [(pwr_pin, wake_level)];
    let rtcio = RtcioWakeupSource::new(wakeup_pins);
    let cfg = RtcSleepConfig::deep();
    // Disable CPU interrupts right before committing RTC deep sleep so no
    // background TIMG0/radio ISR can preempt the sleep state machine.
    // SAFETY: ESP32-C3 `rtc.sleep` writes `STATE0.sleep_en` directly without `wfi`,
    // and `software_reset()` below resets `mstatus` if sleep ever rejects.
    unsafe {
        core::arch::asm!("csrci mstatus, 8", options(nomem, nostack));
    }
    match extra_timer {
        Some(timer) => rtc.sleep(&cfg, &[&rtcio, timer]),
        None => rtc.sleep(&cfg, &[&rtcio]),
    }

    // sleep() only returns if the SoC rejected sleep entry (SLP_REJECT).
    // Never spin in WFI or run with sleeping peripherals: record and reboot cleanly.
    let pin_lvl = u32::from(gpio_get(PWR_BTN));
    mmio_write(RTC_CNTL_STORE7_REG, SLEEP_ABORT_MAGIC | pin_lvl);
    mmio_write(RTC_CNTL_PAD_HOLD_REG, 0);
    mmio_write(RTC_CNTL_DIG_PAD_HOLD_REG, 0);
    software_reset();
}

fn usb_host_sof_active() -> bool {
    let f0 = mmio_read(USB_SERIAL_JTAG_FRAM_NUM_REG) & 0x07FF;
    let t0 = EspSysTimer::now_us();
    while EspSysTimer::now_us().saturating_sub(t0) < 6_000 {
        if (mmio_read(USB_SERIAL_JTAG_FRAM_NUM_REG) & 0x07FF) != f0 {
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

fn confirm_or_back_steadily_held() -> bool {
    for _ in 0..16 {
        feed_watchdog();
        if !gpio_get(PWR_BTN) || !(1850..=2880).contains(&read_adc1_mv(1)) {
            return false;
        }
        EspSysTimer.delay_us(10_000);
    }
    true
}

fn enable_periph_clocks() {
    let bits0 = (1 << 6) | (1 << 7) | (1 << 28); // SPI2 | I2C_EXT0 | APB_SARADC
    mmio_write(SYSTEM_PERIP_CLK_EN0_REG, mmio_read(SYSTEM_PERIP_CLK_EN0_REG) | bits0);
    mmio_write(SYSTEM_PERIP_RST_EN0_REG, mmio_read(SYSTEM_PERIP_RST_EN0_REG) | bits0);
    mmio_write(SYSTEM_PERIP_RST_EN0_REG, mmio_read(SYSTEM_PERIP_RST_EN0_REG) & !bits0);

    let bits1 = 1 << 10; // TSENS / SARADC analog clock gate
    mmio_write(SYSTEM_PERIP_CLK_EN1_REG, mmio_read(SYSTEM_PERIP_CLK_EN1_REG) | bits1);
    mmio_write(SYSTEM_PERIP_RST_EN1_REG, mmio_read(SYSTEM_PERIP_RST_EN1_REG) | bits1);
    mmio_write(SYSTEM_PERIP_RST_EN1_REG, mmio_read(SYSTEM_PERIP_RST_EN1_REG) & !bits1);
}

fn decode_cpu_mhz() -> u32 {
    let sysclk = mmio_read(SYSTEM_SYSCLK_CONF_REG);
    let cpu_per = mmio_read(SYSTEM_CPU_PER_CONF_REG);
    match (sysclk >> 10) & 0x3 {
        0 => 40 / ((sysclk & 0x3FF) + 1),
        1 => {
            if (cpu_per & 0x3) >= 1 {
                160
            } else {
                80
            }
        }
        2 => 17,
        _ => 40,
    }
}

fn gpio_config_output(pin: u8, initial_high: bool) {
    mmio_write(IO_MUX_GPIO_BASE + usize::from(pin) * 4, (1 << 9) | (1 << 12));
    mmio_write(GPIO_FUNC_OUT_SEL_CFG_BASE + usize::from(pin) * 4, 128);
    if initial_high {
        mmio_write(GPIO_OUT_W1TS_REG, 1 << pin);
    } else {
        mmio_write(GPIO_OUT_W1TC_REG, 1 << pin);
    }
    mmio_write(GPIO_ENABLE_W1TS_REG, 1 << pin);
}

fn gpio_config_input(pin: u8, pull_up: bool) {
    mmio_write(GPIO_ENABLE_W1TC_REG, 1 << pin);
    let mut mux = (1 << 9) | (1 << 12); // FUN_IE | MCU_SEL=GPIO
    if pull_up {
        mux |= 1 << 8; // FUN_WPU
    }
    mmio_write(IO_MUX_GPIO_BASE + usize::from(pin) * 4, mux);
}

fn gpio_config_analog(pin: u8) {
    mmio_write(GPIO_ENABLE_W1TC_REG, 1 << pin);
    // MCU_SEL = 1 only: NO FUN_WPU, NO FUN_IE so resistor ladders read cleanly
    mmio_write(IO_MUX_GPIO_BASE + usize::from(pin) * 4, 1 << 12);
}

#[inline]
fn gpio_set(pin: u8, high: bool) {
    if high {
        mmio_write(GPIO_OUT_W1TS_REG, 1 << pin);
    } else {
        mmio_write(GPIO_OUT_W1TC_REG, 1 << pin);
    }
}

#[inline]
fn gpio_get(pin: u8) -> bool {
    (mmio_read(GPIO_IN_REG) & (1 << pin)) != 0
}

#[derive(Clone, Copy)]
struct EspSysTimer;

impl EspSysTimer {
    fn init() {
        mmio_write(SYSTIMER_CONF_REG, (1 << 30) | (1 << 31));
    }

    /// Reads 16 MHz Systimer ticks -> microseconds (`ticks / 16`).
    fn now_us() -> u64 {
        mmio_write(SYSTIMER_UNIT0_OP_REG, 1 << 30);
        for _ in 0..1_000 {
            if (mmio_read(SYSTIMER_UNIT0_OP_REG) & (1 << 29)) != 0 {
                break;
            }
        }
        let hi = u64::from(mmio_read(SYSTIMER_UNIT0_VALUE_HI) & 0xFFFFF);
        let lo = u64::from(mmio_read(SYSTIMER_UNIT0_VALUE_LO));
        ((hi << 32) | lo) / 16
    }

    fn now_ms() -> u64 {
        Self::now_us() / 1_000
    }
}

impl DelayUs for EspSysTimer {
    fn delay_us(&mut self, us: u32) {
        let start = Self::now_us();
        let target = start.saturating_add(u64::from(us));
        let mut next_feed = if us >= 5_000 {
            feed_watchdog();
            start.saturating_add(250_000)
        } else {
            u64::MAX
        };
        loop {
            let now = Self::now_us();
            if now >= target {
                break;
            }
            if now >= next_feed {
                feed_watchdog();
                next_feed = now.saturating_add(250_000);
            }
            core::hint::spin_loop();
        }
    }
}

// ============================================================================
// 4. ESP32-C3 SPI2, I2C0, ADC, RNG, and Display/SD/Power Bridges
// ============================================================================

fn spi2_update_cmd() {
    mmio_write(SPI2_CMD_REG, mmio_read(SPI2_CMD_REG) | (1 << 23));
    for _ in 0..200_000 {
        if (mmio_read(SPI2_CMD_REG) & (1 << 23)) == 0 {
            break;
        }
    }
}

fn spi2_apply_freq_hz(hz: u32) {
    let total_div = 80_000_000u32.div_ceil(hz.max(100_000)).clamp(2, 1024);
    let pre_div = total_div.div_ceil(64).clamp(1, 16);
    let n_div = (total_div / pre_div).clamp(2, 64);
    let pre = pre_div - 1;
    let n = n_div - 1;
    let h = (n_div / 2) - 1;
    let l = n;
    mmio_write(SPI2_CLOCK_REG, (pre << 18) | (n << 12) | (h << 6) | l);
    spi2_update_cmd();
}

struct Esp32Spi2;

impl Esp32Spi2 {
    fn new() -> Self {
        // Configure SPI2 pins via GPIO Matrix (FSPICLK=63, FSPID=65, FSPIQ=64 on ESP32-C3)
        gpio_config_output(SPI_SCLK, false);
        gpio_config_output(SPI_MOSI, false);
        gpio_config_input(SPI_MISO, true);
        mmio_write(GPIO_FUNC_OUT_SEL_CFG_BASE + usize::from(SPI_SCLK) * 4, 63);
        mmio_write(GPIO_FUNC_OUT_SEL_CFG_BASE + usize::from(SPI_MOSI) * 4, 65);
        mmio_write(GPIO_FUNC_IN_SEL_CFG_BASE + 64 * 4, (1 << 6) | u32::from(SPI_MISO));

        mmio_write(SPI2_DMA_CONF_REG, 0);
        mmio_write(SPI2_MISC_REG, 0);
        mmio_write(SPI2_CTRL_REG, 0);
        mmio_write(SPI2_USER1_REG, 0);
        mmio_write(SPI2_SLAVE_REG, 0);
        mmio_write(SPI2_SLAVE_REG, (1 << 29) | (1 << 30));
        mmio_write(SPI2_SLAVE_REG, 0);
        mmio_write(SPI2_CLK_GATE_REG, 0x07); // CLK_EN | MST_CLK_ACTIVE | MST_CLK_SEL(80MHz)
        mmio_write(SPI2_USER_REG, (1 << 27) | (1 << 28) | 1); // USR_MOSI | USR_MISO | DOUTDIN
        spi2_apply_freq_hz(400_000);
        Self
    }

    fn xfer_chunk(&mut self, tx: &[u8], rx: &mut [u8]) {
        let len = tx.len().max(rx.len()).min(64);
        if len == 0 {
            return;
        }
        for i in 0..16 {
            let base = i * 4;
            if base >= len {
                break;
            }
            let b0 = u32::from(*tx.get(base).unwrap_or(&0xFF));
            let b1 = u32::from(*tx.get(base + 1).unwrap_or(&0xFF));
            let b2 = u32::from(*tx.get(base + 2).unwrap_or(&0xFF));
            let b3 = u32::from(*tx.get(base + 3).unwrap_or(&0xFF));
            mmio_write(SPI2_W0_REG + base, b0 | (b1 << 8) | (b2 << 16) | (b3 << 24));
        }
        mmio_write(SPI2_MS_DLEN_REG, (len as u32 * 8) - 1);
        spi2_update_cmd();
        mmio_write(SPI2_CMD_REG, mmio_read(SPI2_CMD_REG) | (1 << 24)); // USR start
        for _ in 0..200_000 {
            if (mmio_read(SPI2_CMD_REG) & (1 << 24)) == 0 {
                break;
            }
        }

        if !rx.is_empty() {
            for i in 0..16 {
                let base = i * 4;
                if base >= rx.len() {
                    break;
                }
                let word = mmio_read(SPI2_W0_REG + base);
                for k in 0..4 {
                    if let Some(dst) = rx.get_mut(base + k) {
                        *dst = ((word >> (k * 8)) & 0xFF) as u8;
                    }
                }
            }
        }
    }

    fn write_fast(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        mmio_write(SPI2_USER_REG, 1 << 27); // USR_MOSI only
        let full_len = data.len() & !63;
        let (full, tail) = data.split_at(full_len);
        if !full.is_empty() {
            mmio_write(SPI2_MS_DLEN_REG, 511);
            spi2_update_cmd();
            for chunk in full.as_chunks::<64>().0 {
                for w in 0..16 {
                    let off = w * 4;
                    let word = u32::from_le_bytes([
                        chunk[off],
                        chunk[off + 1],
                        chunk[off + 2],
                        chunk[off + 3],
                    ]);
                    mmio_write(SPI2_W0_REG + off, word);
                }
                mmio_write(SPI2_CMD_REG, mmio_read(SPI2_CMD_REG) | (1 << 24));
                for _ in 0..200_000 {
                    if (mmio_read(SPI2_CMD_REG) & (1 << 24)) == 0 {
                        break;
                    }
                }
            }
        }
        if !tail.is_empty() {
            self.xfer_chunk(tail, &mut []);
        }
    }
}

#[derive(Clone)]
struct SharedSpi2(Rc<RefCell<Esp32Spi2>>);

impl SpiBus for SharedSpi2 {
    fn set_frequency_hz(&mut self, hz: u32) {
        spi2_apply_freq_hz(hz);
    }

    fn write(&mut self, data: &[u8]) -> Result<(), HalError> {
        self.0.borrow_mut().write_fast(data);
        Ok(())
    }

    fn transfer(&mut self, tx: &[u8], rx: &mut [u8]) -> Result<(), HalError> {
        let mut bus = self.0.borrow_mut();
        mmio_write(SPI2_USER_REG, (1 << 27) | (1 << 28) | 1); // Restore full-duplex for MISO reads
        let total = tx.len().min(rx.len());
        let mut pos = 0;
        while pos < total {
            let take = (total - pos).min(64);
            bus.xfer_chunk(&tx[pos..pos + take], &mut rx[pos..pos + take]);
            pos += take;
        }
        Ok(())
    }
}

struct EspEpdPins;

impl EspEpdPins {
    fn init() -> Self {
        gpio_config_output(EPD_CS, true);
        gpio_config_output(EPD_DC, false);
        gpio_config_output(EPD_RST, true);
        gpio_config_input(EPD_BUSY, false);
        Self
    }
}

impl EpdPins for EspEpdPins {
    fn set_cs(&mut self, high: bool) {
        gpio_set(EPD_CS, high);
    }

    fn set_dc(&mut self, data_mode: bool) {
        gpio_set(EPD_DC, data_mode);
    }

    fn set_rst(&mut self, high: bool) {
        gpio_set(EPD_RST, high);
    }

    fn is_busy(&self) -> bool {
        // Active-LOW BUSY on Xteink X3 (`0` = busy, `1` = idle)
        !gpio_get(EPD_BUSY)
    }

    fn read_0x70_3wire(&mut self) -> [u8; 3] {
        let mut out = [0u8; 3];
        gpio_config_output(SPI_SCLK, false);
        gpio_config_output(SPI_MOSI, false);
        gpio_set(EPD_CS, false);
        gpio_set(EPD_DC, false);
        EspSysTimer.delay_us(2);
        for i in (0..8).rev() {
            gpio_set(SPI_SCLK, false);
            gpio_set(SPI_MOSI, (0x70 & (1 << i)) != 0);
            EspSysTimer.delay_us(1);
            gpio_set(SPI_SCLK, true);
            EspSysTimer.delay_us(1);
        }
        gpio_set(SPI_SCLK, false);
        gpio_set(EPD_DC, true);
        EspSysTimer.delay_us(2);
        gpio_config_input(SPI_MOSI, false);
        EspSysTimer.delay_us(2);
        for byte in &mut out {
            let mut val = 0u8;
            for bit in (0..8).rev() {
                gpio_set(SPI_SCLK, false);
                EspSysTimer.delay_us(1);
                gpio_set(SPI_SCLK, true);
                EspSysTimer.delay_us(1);
                if gpio_get(SPI_MOSI) {
                    val |= 1 << bit;
                }
            }
            gpio_set(SPI_SCLK, false);
            EspSysTimer.delay_us(1);
            *byte = val;
        }
        gpio_set(EPD_CS, true);
        gpio_config_output(SPI_MOSI, false);
        mmio_write(GPIO_FUNC_OUT_SEL_CFG_BASE + usize::from(SPI_SCLK) * 4, 63);
        mmio_write(GPIO_FUNC_OUT_SEL_CFG_BASE + usize::from(SPI_MOSI) * 4, 65);
        out
    }
}

struct EspSdPins;

impl SdControlPins for EspSdPins {
    fn set_power_en(&mut self, on: bool) {
        gpio_set(SD_POWER_EN, on);
    }

    fn set_cs(&mut self, high: bool) {
        gpio_set(SD_CS, high);
    }
}

#[derive(Clone)]
struct BitBangI2c;

impl BitBangI2c {
    fn init() -> Self {
        for pin in [I2C_SDA, I2C_SCL] {
            mmio_write(
                IO_MUX_GPIO_BASE + usize::from(pin) * 4,
                (1 << 8) | (1 << 9) | (1 << 12),
            );
            mmio_write(GPIO_FUNC_OUT_SEL_CFG_BASE + usize::from(pin) * 4, 128);
            mmio_write(GPIO_PIN0_REG + usize::from(pin) * 4, 1 << 2);
            mmio_write(GPIO_OUT_W1TS_REG, 1 << pin);
            mmio_write(GPIO_ENABLE_W1TS_REG, 1 << pin);
        }
        for _ in 0..9 {
            gpio_set(I2C_SCL, false);
            EspSysTimer.delay_us(6);
            gpio_set(I2C_SCL, true);
            EspSysTimer.delay_us(6);
        }
        Self::stop();
        Self
    }

    fn scl_high_wait() {
        gpio_set(I2C_SCL, true);
        for _ in 0..500 {
            if gpio_get(I2C_SCL) {
                break;
            }
            EspSysTimer.delay_us(1);
        }
        EspSysTimer.delay_us(6);
    }

    fn start() {
        gpio_set(I2C_SDA, true);
        Self::scl_high_wait();
        gpio_set(I2C_SDA, false);
        EspSysTimer.delay_us(6);
        gpio_set(I2C_SCL, false);
        EspSysTimer.delay_us(6);
    }

    fn stop() {
        gpio_set(I2C_SDA, false);
        EspSysTimer.delay_us(6);
        Self::scl_high_wait();
        gpio_set(I2C_SDA, true);
        EspSysTimer.delay_us(6);
    }

    fn write_byte(b: u8) -> bool {
        for i in (0..8).rev() {
            gpio_set(I2C_SDA, ((b >> i) & 1) != 0);
            EspSysTimer.delay_us(2);
            Self::scl_high_wait();
            gpio_set(I2C_SCL, false);
            EspSysTimer.delay_us(4);
        }
        gpio_set(I2C_SDA, true);
        EspSysTimer.delay_us(2);
        Self::scl_high_wait();
        let ack = !gpio_get(I2C_SDA);
        gpio_set(I2C_SCL, false);
        EspSysTimer.delay_us(4);
        ack
    }

    fn read_byte(ack: bool) -> u8 {
        gpio_set(I2C_SDA, true);
        let mut v = 0u8;
        for _ in 0..8 {
            EspSysTimer.delay_us(2);
            Self::scl_high_wait();
            v = (v << 1) | u8::from(gpio_get(I2C_SDA));
            gpio_set(I2C_SCL, false);
            EspSysTimer.delay_us(4);
        }
        gpio_set(I2C_SDA, !ack);
        EspSysTimer.delay_us(2);
        Self::scl_high_wait();
        gpio_set(I2C_SCL, false);
        gpio_set(I2C_SDA, true);
        EspSysTimer.delay_us(4);
        v
    }
}

impl I2cBus for BitBangI2c {
    fn write(&mut self, addr: u8, bytes: &[u8]) -> Result<(), HalError> {
        Self::start();
        if !Self::write_byte(addr << 1) {
            Self::stop();
            return Err(HalError::Bus);
        }
        for &b in bytes {
            if !Self::write_byte(b) {
                Self::stop();
                return Err(HalError::Bus);
            }
        }
        Self::stop();
        Ok(())
    }

    fn write_read(&mut self, addr: u8, write: &[u8], read: &mut [u8]) -> Result<(), HalError> {
        if !write.is_empty() {
            Self::start();
            if !Self::write_byte(addr << 1) {
                Self::stop();
                return Err(HalError::Bus);
            }
            for &b in write {
                if !Self::write_byte(b) {
                    Self::stop();
                    return Err(HalError::Bus);
                }
            }
        }
        Self::start();
        if !Self::write_byte((addr << 1) | 1) {
            Self::stop();
            return Err(HalError::Bus);
        }
        let n = read.len();
        for (i, dst) in read.iter_mut().enumerate() {
            *dst = Self::read_byte(i + 1 < n);
        }
        Self::stop();
        Ok(())
    }
}

fn regi2c_saradc_write_mask(reg: u8, msb: u8, lsb: u8, data: u8) {
    mmio_write(ANA_CONF0_REG, mmio_read(ANA_CONF0_REG) & !(1 << 18));
    mmio_write(ANA_CONF1_REG, mmio_read(ANA_CONF1_REG) | (1 << 16));
    let saved_gate = mmio_read(REGI2C_CLK_GATE_REG);
    mmio_write(REGI2C_CLK_GATE_REG, 4);
    let rom_fn: extern "C" fn(u8, u8, u8, u8, u8, u8) =
        unsafe { core::mem::transmute(ROM_I2C_WRITEREG_MASK) };
    rom_fn(0x69, 0, reg, msb, lsb, data);
    mmio_write(REGI2C_CLK_GATE_REG, saved_gate);
}

fn adc1_oneshot_raw_cfg(cfg: u32) -> u16 {
    mmio_write(APB_SARADC_INT_CLR, 1 << 31);
    mmio_write(APB_SARADC_ONETIME_SAMPLE, cfg);
    EspSysTimer.delay_us(5);
    mmio_write(APB_SARADC_ONETIME_SAMPLE, cfg | (1 << 29));
    for _ in 0..20_000 {
        if (mmio_read(APB_SARADC_INT_RAW) & (1 << 31)) != 0 {
            break;
        }
    }
    let val = (mmio_read(APB_SARADC_1_DATA_STATUS) & 0x0FFF) as u16;
    mmio_write(APB_SARADC_INT_CLR, 1 << 31);
    mmio_write(APB_SARADC_ONETIME_SAMPLE, 0);
    val
}

fn apply_saradc_init_code(init_code: u16) {
    regi2c_saradc_write_mask(1, 3, 0, ((init_code >> 8) & 0x0F) as u8);
    regi2c_saradc_write_mask(0, 7, 0, (init_code & 0xFF) as u8);
}

fn configure_adc1_common() {
    let sens = mmio_read(RTC_CNTL_SENSOR_CTRL_REG) & 0x07FF_FFFF;
    mmio_write(RTC_CNTL_SENSOR_CTRL_REG, sens | (4 << 27));
    mmio_write(
        APB_SARADC_CTRL_REG,
        mmio_read(APB_SARADC_CTRL_REG) | (1 << 6) | (3 << 27),
    );
    let clkm = mmio_read(APB_SARADC_CLKM_CONF_REG) & !((3 << 21) | 0xFF);
    mmio_write(APB_SARADC_CLKM_CONF_REG, clkm | (2 << 21) | (1 << 20) | 4);
    regi2c_saradc_write_mask(2, 6, 4, 1); // I2C_SARADC_SAR1_DREF = 1
}

fn init_adc1() -> u16 {
    configure_adc1_common();
    EspSysTimer.delay_us(300);

    let gnd_cfg = (1 << 31) | (0x0F << 25) | (3 << 23);
    regi2c_saradc_write_mask(7, 5, 5, 1); // ENCAL_GND = 1
    let mut lo = 0u16;
    let mut hi = 4096u16;
    while hi - lo > 1 {
        let mid = (lo + hi) / 2;
        apply_saradc_init_code(mid);
        if adc1_oneshot_raw_cfg(gnd_cfg) == 0 {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    let init_code = (lo + hi) / 2;
    apply_saradc_init_code(init_code);
    regi2c_saradc_write_mask(7, 5, 5, 0); // ENCAL_GND = 0
    EspSysTimer.delay_us(100);
    init_code
}

fn restore_adc1_after_phy(init_code: u16) {
    mmio_write(
        SYSTEM_PERIP_CLK_EN0_REG,
        mmio_read(SYSTEM_PERIP_CLK_EN0_REG) | (1 << 28),
    );
    mmio_write(
        SYSTEM_PERIP_CLK_EN1_REG,
        mmio_read(SYSTEM_PERIP_CLK_EN1_REG) | (1 << 10),
    );

    gpio_config_analog(1);
    gpio_config_analog(2);
    gpio_config_input(PWR_BTN, true);

    // Preserve PHY RF / SARADC2 clock & arbiter registers; only re-enable SARADC1 bias and init code.
    mmio_write(
        APB_SARADC_CTRL_REG,
        mmio_read(APB_SARADC_CTRL_REG) | (1 << 6) | (3 << 27),
    );
    apply_saradc_init_code(init_code);
    regi2c_saradc_write_mask(2, 6, 4, 1);
    regi2c_saradc_write_mask(7, 5, 5, 0);
    EspSysTimer.delay_us(100);
    let _ = read_adc1_mv(1);
    let _ = read_adc1_mv(2);
}

fn read_adc1_mv(channel: u8) -> u16 {
    let cfg = (1 << 31) | (u32::from(channel & 0x0F) << 25) | (3 << 23);
    let raw = u32::from(adc1_oneshot_raw_cfg(cfg));
    ((raw * 3100) / 4095) as u16
}

struct EspRng;

impl EntropySource for EspRng {
    fn fill_bytes(&mut self, out: &mut [u8]) {
        for chunk in out.chunks_mut(4) {
            let word = mmio_read(RNG_DATA_REG).to_le_bytes();
            let n = chunk.len();
            chunk.copy_from_slice(&word[..n]);
        }
    }
}

struct X3Sensors {
    i2c: BitBangI2c,
    bq: Bq27220<EspSysTimer>,
}

impl DeviceSensors for X3Sensors {
    fn battery(&mut self) -> BatteryStatus {
        self.bq.read_battery(&mut self.i2c).unwrap_or(BatteryStatus {
            percent: 100,
            millivolts: 4100,
            current_ma: 0,
            charging: false,
        })
    }

    fn now(&mut self) -> DateTime {
        Ds3231::read_datetime(&mut self.i2c).unwrap_or(DateTime {
            year: 2026,
            month: 11,
            day: 12,
            hour: 12,
            minute: 0,
            second: 0,
        })
    }

    fn set_datetime(&mut self, dt: DateTime) -> bool {
        Ds3231::write_datetime(&mut self.i2c, dt).is_ok()
    }

    fn uptime_ms(&self) -> u64 {
        EspSysTimer::now_ms()
    }

    fn temperature_c_x10(&mut self) -> i16 {
        self.bq.read_temperature_c_x10(&mut self.i2c).unwrap_or(220)
    }
}

type X3Epd = X3EpdDriver<SharedSpi2, EspEpdPins, EspSysTimer>;
type X3Card = SpiSdhcCard<SharedSpi2, EspSdPins, EspSysTimer>;

struct X3PowerControl {
    epd: Rc<RefCell<X3Epd>>,
    sd_vol: Option<Rc<RefCell<Fat32Volume<X3Card>>>>,
    i2c: BitBangI2c,
    imu: Qmi8658,
    ctrl_name: &'static str,
    hv_idle_since_ms: Option<u64>,
    rtc: Rtc<'static>,
    pwr_pin: GPIO3<'static>,
}

impl X3PowerControl {
    fn poll_hv_standby(&mut self, now_ms: u64) {
        if let Some(t0) = self.hv_idle_since_ms
            && now_ms.saturating_sub(t0) >= 27_500
        {
            self.hv_idle_since_ms = None;
            let _ = self.epd.borrow_mut().set_hv_standby(true);
        }
    }
}

impl PowerControl for X3PowerControl {
    fn enter_deep_sleep(&mut self) {
        ulog!("[VVBOOK-OS] Entering deep sleep...");
        feed_watchdog();
        let _ = self.epd.borrow_mut().enter_deep_sleep();
        feed_watchdog();
        if let Some(ref v) = self.sd_vol {
            v.borrow_mut().set_power_save(true);
        }
        let _ = self.imu.set_enabled(&mut self.i2c, false);

        // Gate SD rail off, keep EPD_RST high (prevents controller drift out of DSLP),
        // and ensure PWR_BTN pull-up is active while awaiting release.
        gpio_set(SD_CS, true);
        gpio_set(SD_POWER_EN, false);
        gpio_set(EPD_RST, true);
        gpio_config_input(PWR_BTN, true);

        wait_for_power_release_ms(6_000);
        enter_hardware_deep_sleep(&mut self.rtc, &mut self.pwr_pin, true, None);
    }

    fn set_imu_flick(&mut self, enabled: bool) {
        let _ = self.imu.set_enabled(&mut self.i2c, enabled);
    }

    fn set_rotated_180(&mut self, rotated: bool) {
        self.epd.borrow_mut().set_rotated_180(rotated);
    }

    fn controller_name(&self) -> &'static str {
        self.ctrl_name
    }

    fn set_panel_hv_standby(&mut self, standby: bool) {
        if standby {
            if self.hv_idle_since_ms.is_none() {
                self.hv_idle_since_ms = Some(EspSysTimer::now_ms());
            }
        } else {
            self.hv_idle_since_ms = None;
        }
    }

    fn set_sd_power_save(&mut self, power_save: bool) {
        // Keep GPIO13 (SD_POWER_EN) continuously HIGH while awake so the shared
        // SPI2 bus (UC8253 EPD + microSD) and battery power rail never clamp or drop
        // when USB VBUS is unplugged. Only enter_deep_sleep() gates SD_POWER_EN low.
        let _ = (&self.sd_vol, power_save);
    }

    fn enter_light_idle(&mut self, _max_ms: u32) {
        EspSysTimer.delay_us(2_000);
    }
}

// Release Ed25519 verifying key for signed `.vvfw` OTA updates over VVDrop.
const OTA_RELEASE_PUBKEY: [u8; 32] = [
    0xD7, 0x5A, 0x98, 0x01, 0x82, 0xB1, 0x0A, 0xB7,
    0xD5, 0x4B, 0xFE, 0xD3, 0xC9, 0x64, 0x07, 0x3A,
    0x0E, 0xE1, 0x72, 0xF3, 0xDA, 0xA6, 0x23, 0x25,
    0xAF, 0x02, 0x1A, 0x68, 0xF7, 0x07, 0x51, 0x1A,
];

struct WifiPeripherals {
    timg0: TIMG0<'static>,
    rng: RNG<'static>,
    wifi: WIFI<'static>,
}

struct WifiRuntime {
    controller: WifiController<'static>,
    ap_device: WifiDevice<'static>,
    engine: SmoltcpPortalEngine,
}

struct X3WifiPortal {
    adc_init_code: u16,
    periphs: Option<WifiPeripherals>,
    rt: Option<WifiRuntime>,
    state: VvDropServerState,
    dhcp: DhcpServer,
}

impl X3WifiPortal {
    fn hw_seed() -> u64 {
        let lo = u64::from(mmio_read(RNG_DATA_REG));
        let hi = u64::from(mmio_read(RNG_DATA_REG)) ^ EspSysTimer::now_us();
        lo | (hi << 32) | 1
    }

    fn new(
        timg0: TIMG0<'static>,
        rng: RNG<'static>,
        wifi: WIFI<'static>,
        adc_init_code: u16,
    ) -> Self {
        Self {
            adc_init_code,
            periphs: Some(WifiPeripherals { timg0, rng, wifi }),
            rt: None,
            state: VvDropServerState::new(OTA_RELEASE_PUBKEY, 6, Self::hw_seed()),
            dhcp: DhcpServer::new(),
        }
    }

    fn init_runtime(p: WifiPeripherals) -> Result<WifiRuntime, &'static str> {
        let timg0 = TimerGroup::new(p.timg0);
        let hal_rng = EspHalRng::new(p.rng);
        let inited = esp_wifi::init(timg0.timer0, hal_rng).map_err(|_| "Wi-Fi driver init")?;
        let inited: &'static _ = Box::leak(Box::new(inited));
        let (controller, interfaces) =
            esp_wifi::wifi::new(inited, p.wifi).map_err(|_| "Wi-Fi iface init")?;
        let mut ap_device = interfaces.ap;
        let mac = ap_device.mac_address();
        let engine =
            SmoltcpPortalEngine::new(&mut ap_device, mac, Self::hw_seed(), EspSysTimer::now_ms());

        Ok(WifiRuntime {
            controller,
            ap_device,
            engine,
        })
    }
}

impl NetworkPortal for X3WifiPortal {
    fn start(&mut self, pin: &str) {
        self.state.start(pin, Self::hw_seed());
        self.state.wpa2_authenticated_link = true;
        self.dhcp.reset();

        if self.rt.is_none()
            && let Some(p) = self.periphs.take()
        {
            match Self::init_runtime(p) {
                Ok(rt) => self.rt = Some(rt),
                Err(msg) => {
                    ulog!("[VVBOOK-OS] Wi-Fi init error: {}", msg);
                    self.state.busy_message = Some(String::from(msg));
                    restore_adc1_after_phy(self.adc_init_code);
                    return;
                }
            }
        }

        if let Some(rt) = &mut self.rt {
            if rt.controller.is_started().unwrap_or(false) {
                let _ = rt.controller.stop();
            }
            let pass = String::from(core::str::from_utf8(&self.state.pin).unwrap_or("48291037"));
            let cfg = Configuration::AccessPoint(AccessPointConfiguration {
                ssid: String::from(AP_SSID),
                channel: 6,
                auth_method: AuthMethod::WPA2Personal,
                password: pass,
                max_connections: 4,
                ..Default::default()
            });
            if rt.controller.set_configuration(&cfg).is_err() || rt.controller.start().is_err() {
                ulog!("[VVBOOK-OS] Wi-Fi AP start error");
                self.state.busy_message = Some(String::from("Wi-Fi AP start failed"));
            } else {
                let mac = rt.ap_device.mac_address();
                rt.engine.set_hardware_addr(mac);
                rt.engine.reset_sockets_only(true);
                let (heap_free, heap_max) = heap_stats();
                ulog!(
                    "[VVBOOK-OS] Wi-Fi WPA2 SoftAP ready: {} ({}) mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} heap_free={} heap_max={}",
                    AP_SSID,
                    AP_URL,
                    mac[0],
                    mac[1],
                    mac[2],
                    mac[3],
                    mac[4],
                    mac[5],
                    heap_free,
                    heap_max
                );
            }
            restore_adc1_after_phy(self.adc_init_code);
        }
    }

    fn stop(&mut self) {
        self.state.stop();
        self.dhcp.reset();
        if let Some(rt) = &mut self.rt {
            rt.engine.reset_sockets_only(true);
            let _ = rt.controller.stop();
            restore_adc1_after_phy(self.adc_init_code);
            ulog!("[VVBOOK-OS] Wi-Fi SoftAP stopped");
        }
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

    fn poll_io(&mut self, storage: &mut dyn StorageBackend, now_ms: u64) -> bool {
        feed_watchdog();
        let Some(rt) = &mut self.rt else {
            return false;
        };
        rt.engine.poll_io(
            &mut rt.ap_device,
            &mut self.state,
            &mut self.dhcp,
            storage,
            now_ms,
        )
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

// ============================================================================
// 5. Thin Borrow Shims for DisplaySink & StorageBackend -> FolioApp
// ============================================================================

struct EpdSinkHandle {
    epd: Rc<RefCell<X3Epd>>,
    t_begin_us: u64,
    wait_prev_us: u64,
}

impl EpdSinkHandle {
    fn new(epd: Rc<RefCell<X3Epd>>) -> Self {
        Self {
            epd,
            t_begin_us: 0,
            wait_prev_us: 0,
        }
    }
}

impl vvbook_pal::DisplaySink for EpdSinkHandle {
    fn begin_frame(&mut self, mode: RefreshMode) {
        feed_watchdog();
        let t0 = EspSysTimer::now_us();
        let _ = self.epd.borrow_mut().finish_pending_refresh();
        let t1 = EspSysTimer::now_us();
        self.wait_prev_us = t1.saturating_sub(t0);
        self.t_begin_us = t1;
        self.epd.borrow_mut().begin_frame(mode);
    }

    fn write_strip_2bpp(&mut self, y_start: usize, rows: usize, packed_2bpp: &[u8]) {
        self.epd.borrow_mut().write_strip_2bpp(y_start, rows, packed_2bpp);
    }

    fn end_frame(&mut self, mode: RefreshMode) {
        feed_watchdog();
        let t_strips = EspSysTimer::now_us();
        self.epd.borrow_mut().end_frame(mode);
        let t_end = EspSysTimer::now_us();
        ulog!(
            "[VVBOOK-OS] FRAME mode={:?} render={}us spi_epd={}us wait_prev={}us",
            mode,
            t_strips.saturating_sub(self.t_begin_us),
            t_end.saturating_sub(t_strips),
            self.wait_prev_us
        );
    }

    fn present_1bpp(&mut self, mode: RefreshMode, packed_1bpp: &[u8]) -> bool {
        self.epd.borrow_mut().present_1bpp(mode, packed_1bpp)
    }
}

struct Fat32Handle {
    vol: Option<Rc<RefCell<Fat32Volume<X3Card>>>>,
    deadline_ms: Cell<u64>,
    cancel_after_ms: Cell<u64>,
    cancel_armed: Cell<bool>,
    back_hits: Cell<u8>,
    aborted: Cell<bool>,
    polls: Cell<u32>,
}

impl Fat32Handle {
    fn new(vol: Option<Rc<RefCell<Fat32Volume<X3Card>>>>) -> Self {
        Self {
            vol,
            deadline_ms: Cell::new(0),
            cancel_after_ms: Cell::new(0),
            cancel_armed: Cell::new(false),
            back_hits: Cell::new(0),
            aborted: Cell::new(false),
            polls: Cell::new(0),
        }
    }

    fn begin_event(&self, now_ms: u64, budget_ms: u64) {
        self.deadline_ms.set(now_ms.saturating_add(budget_ms));
        self.cancel_after_ms.set(now_ms.saturating_add(900));
        self.cancel_armed.set(false);
        self.back_hits.set(0);
        self.aborted.set(false);
        self.polls.set(0);
    }

    fn end_event(&self) -> bool {
        self.deadline_ms.set(0);
        self.aborted.replace(false)
    }

    fn check_io_guard(&self) -> Result<(), vvbook_pal::StorageError> {
        let p = self.polls.get().wrapping_add(1);
        self.polls.set(p);
        if (p & 7) == 0 {
            feed_watchdog();
        }
        let deadline = self.deadline_ms.get();
        if deadline == 0 {
            return Ok(());
        }
        if self.aborted.get() {
            return Err(vvbook_pal::StorageError::Io);
        }
        if (p & 7) == 0 {
            let now = EspSysTimer::now_ms();
            if now >= deadline {
                self.aborted.set(true);
                return Err(vvbook_pal::StorageError::Io);
            }
            if now >= self.cancel_after_ms.get() {
                let mv = read_adc1_mv(1);
                if mv >= 3000 {
                    self.cancel_armed.set(true);
                    self.back_hits.set(0);
                } else if self.cancel_armed.get() && (2360..=2900).contains(&mv) {
                    let hits = self.back_hits.get().saturating_add(1);
                    self.back_hits.set(hits);
                    if hits >= 4 {
                        self.aborted.set(true);
                        return Err(vvbook_pal::StorageError::Io);
                    }
                } else {
                    self.back_hits.set(0);
                }
            }
        }
        Ok(())
    }
}

impl vvbook_pal::StorageBackend for Fat32Handle {
    fn exists(&self, path: &str) -> bool {
        if self.check_io_guard().is_err() {
            return false;
        }
        self.vol.as_ref().is_some_and(|v| v.borrow().exists(path))
    }

    fn file_len(&self, path: &str) -> Result<u64, vvbook_pal::StorageError> {
        self.check_io_guard()?;
        match self.vol {
            Some(ref v) => v.borrow().file_len(path),
            None => Err(vvbook_pal::StorageError::NotFound),
        }
    }

    fn read_all(&self, path: &str) -> Result<Vec<u8>, vvbook_pal::StorageError> {
        const MAX_READ_ALL_BYTES: u64 = 160 * 1024;
        self.check_io_guard()?;
        match self.vol {
            Some(ref v) => {
                let len = v.borrow().file_len(path)?;
                if len > MAX_READ_ALL_BYTES {
                    return Err(vvbook_pal::StorageError::OutOfSpace);
                }
                v.borrow().read_all(path)
            }
            None => Err(vvbook_pal::StorageError::NotFound),
        }
    }

    fn read_range(&self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize, vvbook_pal::StorageError> {
        self.check_io_guard()?;
        match self.vol {
            Some(ref v) => v.borrow().read_range(path, offset, buf),
            None => Err(vvbook_pal::StorageError::NotFound),
        }
    }

    fn write_all(&mut self, path: &str, data: &[u8]) -> Result<(), vvbook_pal::StorageError> {
        self.check_io_guard()?;
        match self.vol {
            Some(ref v) => v.borrow_mut().write_all(path, data),
            None => Err(vvbook_pal::StorageError::Io),
        }
    }

    fn write_atomic(&mut self, path: &str, data: &[u8]) -> Result<(), vvbook_pal::StorageError> {
        self.check_io_guard()?;
        match self.vol {
            Some(ref v) => v.borrow_mut().write_atomic(path, data),
            None => Err(vvbook_pal::StorageError::Io),
        }
    }

    fn append(&mut self, path: &str, data: &[u8]) -> Result<(), vvbook_pal::StorageError> {
        self.check_io_guard()?;
        match self.vol {
            Some(ref v) => v.borrow_mut().append(path, data),
            None => Err(vvbook_pal::StorageError::Io),
        }
    }

    fn remove(&mut self, path: &str) -> Result<(), vvbook_pal::StorageError> {
        self.check_io_guard()?;
        match self.vol {
            Some(ref v) => v.borrow_mut().remove(path),
            None => Err(vvbook_pal::StorageError::NotFound),
        }
    }

    fn mkdir_all(&mut self, path: &str) -> Result<(), vvbook_pal::StorageError> {
        self.check_io_guard()?;
        match self.vol {
            Some(ref v) => v.borrow_mut().mkdir_all(path),
            None => Ok(()),
        }
    }

    fn list_dir(&self, path: &str) -> Result<Vec<vvbook_pal::DirEntry>, vvbook_pal::StorageError> {
        self.check_io_guard()?;
        match self.vol {
            Some(ref v) => v.borrow().list_dir(path),
            None => Ok(Vec::new()),
        }
    }
}

// ============================================================================
// 6. Bare-Metal Entry Point & Event Loop
// ============================================================================

const STARTER_PUSHKIN_FB2: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<FictionBook>
  <description>
    <title-info>
      <author><first-name>Александр</first-name><last-name>Пушкин</last-name></author>
      <book-title>Сказка о царе Салтане</book-title>
      <lang>ru</lang>
    </title-info>
  </description>
  <body>
    <section>
      <title><p>Глава I. Три девицы под окном</p></title>
      <epigraph><p>Ветер по морю гуляет и кораблик подгоняет...</p></epigraph>
      <poem><stanza>
        <v>Три девицы под окном</v>
        <v>Пряли поздно вечерком.</v>
        <v>«Кабы я была царица, —</v>
        <v>Говорит одна девица, —</v>
        <v>То на весь крещёный мир</v>
        <v>Приготовила б я пир».</v>
      </stanza></poem>
      <p>В тот же час у ворот старого терема царь Салтан слушал тихую беседу и выбирал себе невесту. Высокий терем светился сквозь сумерки зимнего сада, а над широкой Невой поднималась серебристая луна.</p>
      <p>«Здравствуй, красная девица, — говорит он, — будь царица и роди богатыря мне к исходу сентября». Все три сестры поклонились государю и отправились во дворец готовиться к великому торжеству.</p>
    </section>
  </body>
</FictionBook>"#;

const STARTER_ALICE_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head><title>Alice's Adventures in Wonderland — Lewis Carroll</title></head>
<body>
  <h1>I. Down the Rabbit-Hole</h1>
  <p>Alice was beginning to get very <em>tired</em> of sitting by her sister on the bank, and of having nothing to do: once or twice she had peeped into the book her sister was reading, but it had no pictures or conversations in it, "and what is the use of a book," thought Alice, "without pictures or conversation?"</p>
  <p>There was nothing so <strong>very</strong> remarkable in that; nor did Alice think it so very much out of the way to hear the Rabbit say to itself, "Oh dear! Oh dear! I shall be late!" But when the Rabbit actually took a watch out of its waistcoat-pocket, Alice started to her feet and ran across the field after it.</p>
  <h2>II. The Pool of Tears</h2>
  <p>"Curiouser and curiouser!" cried Alice. "Now I am opening out like the largest telescope that ever was! Good-bye, feet!" Just at this moment her head struck against the roof of the hall.</p>
</body>
</html>"#;

const STARTER_GUIDE_MD: &[u8] = b"# VVBook Folio OS - User Guide\n\n\
Welcome to VVBook Folio OS 0.6.3, running in pure bare-metal Rust on Xteink X3.\n\n\
## Navigation\n\n\
- Side Up / Down or Chin Left / Right: move focus or turn pages.\n\
- Confirm (OK): open active book, toggle setting, or confirm.\n\
- Back: return up cleanly or undo in games.\n\
- Power: render Museum Sleep Poster and enter ultra-low-power deep sleep.\n\n\
## Wireless Transfer (VVDrop)\n\n\
Join WPA2 Wi-Fi network `VVBook-Folio` using the 8-digit on-screen key (or scan the Wi-Fi QR code) and visit `http://192.168.4.1`.\n\n\
## Books on microSD\n\n\
Drop `.epub`, `.fb2`, `.fb2.zip`, `.mobi`, `.azw3`, `.pdf`, `.md`, `.html`, or `.txt` files into `/books/` or any folder on your microSD card.\n";

fn dir_tree_has_any_book(storage: &Fat32Handle, dir: &str, depth: usize) -> bool {
    let Ok(entries) = storage.list_dir(dir) else {
        return false;
    };
    for entry in &entries {
        if entry.is_dir || entry.name.starts_with('.') {
            continue;
        }
        let child = if dir == "/" {
            alloc::format!("/{}", entry.name)
        } else {
            alloc::format!("{}/{}", dir, entry.name)
        };
        if vvbook_userspace::is_valid_library_book_path(&child) {
            return true;
        }
    }
    if depth >= 5 {
        return false;
    }
    for entry in entries {
        if !entry.is_dir || entry.name.starts_with('.') {
            continue;
        }
        let child = if dir == "/" {
            alloc::format!("/{}", entry.name)
        } else {
            alloc::format!("{}/{}", dir, entry.name)
        };
        if !vvbook_userspace::is_valid_library_dir(&entry.name) {
            continue;
        }
        if dir_tree_has_any_book(storage, &child, depth + 1) {
            return true;
        }
    }
    false
}

fn seed_starter_books_if_fresh(storage: &mut Fat32Handle) {
    if storage.exists("/.vvbook/catalog.vvc") || dir_tree_has_any_book(storage, "/", 0) {
        return;
    }
    let _ = storage.mkdir_all("/books");
    let _ = storage.write_atomic("/books/pushkin_saltan.fb2", STARTER_PUSHKIN_FB2.as_bytes());
    let _ = storage.write_atomic("/books/alice_wonderland.html", STARTER_ALICE_HTML.as_bytes());
    let _ = storage.write_atomic("/books/vvbook_folio_guide.md", STARTER_GUIDE_MD);
    ulog!("[VVBOOK-OS] Seeded 3 starter books in /books");
}

#[esp_hal::main]
fn main() -> ! {
    let peripherals = esp_hal::init(
        esp_hal::Config::default().with_cpu_clock(esp_hal::clock::CpuClock::max()),
    );
    esp_alloc::heap_allocator!(#[unsafe(link_section = ".dram2_uninit")] size: 64 * 1024);
    esp_alloc::heap_allocator!(#[unsafe(link_section = ".noinit")] size: 208 * 1024);

    // Clear any legacy 0.6.0 writes to SW_CPU_STALL or REAL_STORE4 before Rtc::new() reads STORE4.
    mmio_write(RTC_CNTL_SW_CPU_STALL_REG, 0);
    if (mmio_read(RTC_CNTL_REAL_STORE4_REG) & 0xFFFF_FF00) == PANIC_RTC_MAGIC {
        mmio_write(RTC_CNTL_REAL_STORE4_REG, 0);
    }
    unhold_all_pads();

    disable_timg_watchdogs();
    enable_periph_clocks();
    EspSysTimer::init();

    let mut rtc = Rtc::new(peripherals.LPWR);
    rtc.swd.disable(); // Keeps Super Watchdog active with hardware auto-feed (swd_auto_feed_en = true).
    arm_rwdt(&mut rtc);
    let mut pwr_pin = peripherals.GPIO3;

    let cpu_mhz = decode_cpu_mhz();

    gpio_config_output(SD_POWER_EN, true);
    gpio_config_output(SD_CS, true);
    gpio_config_output(EPD_RST, true);
    gpio_config_input(PWR_BTN, true);
    gpio_config_analog(1);
    gpio_config_analog(2);
    let init_code = init_adc1();
    let _ = read_adc1_mv(1);
    let _ = read_adc1_mv(2);
    EspSysTimer.delay_us(10_000);

    let reason = reset_reason(Cpu::ProCpu);
    let wake = wakeup_cause();
    ulog!("[VVBOOK-OS] === VVBook Folio OS 0.6.3 (ota_0 XIP) ===");
    ulog!("[VVBOOK-OS] Reset={:?} Wake={:?}", reason, wake);
    let prev_panic = mmio_read(RTC_CNTL_STORE6_REG);
    if (prev_panic & 0xFFFF_FF00) == PANIC_RTC_MAGIC {
        ulog!("[VVBOOK-OS] Recovered from panic reset #{}", prev_panic & 0xFF);
    }
    let sleep_abort = mmio_read(RTC_CNTL_STORE7_REG);
    if (sleep_abort & 0xFFFF_FF00) == SLEEP_ABORT_MAGIC {
        ulog!(
            "[VVBOOK-OS] Recovered from aborted deep sleep (pwr_lvl={})",
            sleep_abort & 0xFF
        );
        mmio_write(RTC_CNTL_STORE7_REG, 0);
    }
    ulog!("[VVBOOK-OS] CPU clock: {} MHz", cpu_mhz);
    ulog!("[VVBOOK-OS] ADC1 calibrated init_code={}", init_code);

    // Hardware USB Rescue Mode: requires an active USB SOF host AND Confirm/Back held steadily
    // for 160ms without Power held, preventing battery-only lockups from ADC noise.
    if usb_host_sof_active() && confirm_or_back_steadily_held() {
        ulog!("[VVBOOK-OS] USB Rescue Mode active (press Power or wait 120s to reboot)");
        for _ in 0..12_000 {
            feed_watchdog();
            if !gpio_get(PWR_BTN) {
                break;
            }
            EspSysTimer.delay_us(10_000);
        }
        software_reset();
    }

    let spi = SharedSpi2(Rc::new(RefCell::new(Esp32Spi2::new())));
    let i2c = BitBangI2c::init();

    let mut epd_opt = None;
    for attempt in 1..=3 {
        feed_watchdog();
        let epd_pins = EspEpdPins::init();
        match X3EpdDriver::new(spi.clone(), epd_pins, EspSysTimer) {
            Ok(d) => {
                epd_opt = Some(d);
                break;
            }
            Err(e) => {
                ulog!("[VVBOOK-OS] EPD init attempt {}/3 FAILED: {:?}", attempt, e);
                *spi.0.borrow_mut() = Esp32Spi2::new();
                EspSysTimer.delay_us(50_000);
            }
        }
    }
    let mut epd_drv = match epd_opt {
        Some(d) => d,
        None => {
            ulog!("[VVBOOK-OS] EPD init FAILED after 3 attempts; entering safe deep sleep");
            gpio_set(SD_CS, true);
            gpio_set(SD_POWER_EN, false);
            gpio_set(EPD_RST, true);
            gpio_config_input(PWR_BTN, true);
            wait_for_power_release_ms(2_000);
            let timer = TimerWakeupSource::new(core::time::Duration::from_secs(30));
            enter_hardware_deep_sleep(&mut rtc, &mut pwr_pin, false, Some(&timer));
        }
    };
    epd_drv.set_async_refresh(true);
    let ctrl_name = epd_drv.silicon().name();
    ulog!("[VVBOOK-OS] EPD init OK: {}", ctrl_name);
    let epd = Rc::new(RefCell::new(epd_drv));

    let card = SpiSdhcCard::new(spi, EspSdPins, EspSysTimer, 31_250_000);
    let sd_vol = Fat32Volume::mount(card).ok().map(|v| Rc::new(RefCell::new(v)));
    if sd_vol.is_none() {
        ulog!("[VVBOOK-OS] SD mount unavailable; running without microSD");
        gpio_set(SD_CS, true);
    } else {
        ulog!("[VVBOOK-OS] SD FAT32 mounted OK");
    }

    let mut sink = EpdSinkHandle::new(Rc::clone(&epd));
    let mut storage = Fat32Handle::new(sd_vol.as_ref().map(Rc::clone));
    if sd_vol.is_some() {
        seed_starter_books_if_fresh(&mut storage);
    }
    let mut sensors = X3Sensors {
        i2c: i2c.clone(),
        bq: Bq27220::new(EspSysTimer),
    };
    let mut power = X3PowerControl {
        epd: Rc::clone(&epd),
        sd_vol,
        i2c,
        imu: Qmi8658::new(),
        ctrl_name,
        hv_idle_since_ms: None,
        rtc,
        pwr_pin,
    };
    let mut portal = X3WifiPortal::new(
        peripherals.TIMG0,
        peripherals.RNG,
        peripherals.WIFI,
        init_code,
    );
    let mut rng = EspRng;

    let mut app = Box::new(FolioApp::new());
    run_folio_os(
        &mut app,
        &mut sink,
        &mut storage,
        &mut sensors,
        &mut power,
        &mut portal,
        &mut rng,
    )
}

#[inline(never)]
fn run_folio_os(
    app: &mut FolioApp,
    sink: &mut EpdSinkHandle,
    storage: &mut Fat32Handle,
    sensors: &mut X3Sensors,
    power: &mut X3PowerControl,
    portal: &mut X3WifiPortal,
    rng: &mut EspRng,
) -> ! {
    feed_watchdog();
    app.boot(sink, storage, sensors, power, portal, rng);
    feed_watchdog();

    let bat = sensors.battery();
    let rtc = sensors.now();
    let (heap_free, heap_max) = heap_stats();
    ulog!(
        "[VVBOOK-OS] READY v0.6.3 books={} bat={}% {}mV {}mA heap_free={} heap_max={} rtc={:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        app.catalog.len(),
        bat.percent,
        bat.millivolts,
        bat.current_ma,
        heap_free,
        heap_max,
        rtc.year,
        rtc.month,
        rtc.day,
        rtc.hour,
        rtc.minute,
        rtc.second
    );
    mmio_write(RTC_CNTL_STORE6_REG, 0);

    let mut buttons = AdcButtonDecoder::new();
    let mut pwr_guard = WakePowerGuard::new(EspSysTimer::now_ms());
    let mut events = Vec::<InputEvent>::with_capacity(4);
    let mut last_tick_ms = 0u64;
    let mut last_hb_ms = 0u64;
    let mut last_imu_ms = 0u64;
    let mut last_btn_ms = 0u64;
    let mut first_btn_poll = true;

    loop {
        feed_watchdog();
        let now_ms = EspSysTimer::now_ms();
        events.clear();
        if first_btn_poll || now_ms.saturating_sub(last_btn_ms) >= 12 {
            first_btn_poll = false;
            last_btn_ms = now_ms;
            let mv_a = read_adc1_mv(1);
            let mv_b = read_adc1_mv(2);
            let raw_pwr = !gpio_get(PWR_BTN);
            let pwr = pwr_guard.filter_pwr_pressed(now_ms, raw_pwr);
            buttons.poll_into(now_ms, mv_a, mv_b, pwr, &mut events);
        }

        if now_ms.saturating_sub(last_imu_ms) >= 40 {
            last_imu_ms = now_ms;
            if let Ok(Some(imu_ev)) = power.imu.poll_event(&mut power.i2c, now_ms) {
                if let InputEvent::OrientationFlip(rot) = imu_ev {
                    buttons.set_rotated_180(rot);
                }
                events.push(imu_ev);
            }
        }

        if !events.is_empty() {
            power.hv_idle_since_ms = None;
        }

        for ev in events.drain(..) {
            ulog!("[VVBOOK-OS] EV {:?}", ev);
            let ev_t0 = sensors.uptime_ms();
            storage.begin_event(ev_t0, 25_000);
            app.handle_event(ev, sink, storage, sensors, power, portal, rng);
            let ev_dt = sensors.uptime_ms().saturating_sub(ev_t0);
            if let Some(diag) = app.take_last_open_diag() {
                ulog!("[VVBOOK-OS] OPEN {}", diag);
            }
            if storage.end_event() {
                ulog!("[VVBOOK-OS] EV_IO_ABORT dt={}ms", ev_dt);
            }
            if ev_dt >= 50 {
                let (heap_free, heap_max) = heap_stats();
                ulog!(
                    "[VVBOOK-OS] EV_DONE dt={}ms scr={:?} heap_free={} heap_max={}",
                    ev_dt,
                    app.screen,
                    heap_free,
                    heap_max
                );
            }
        }

        if app.screen == Screen::VvDrop {
            portal.poll_io(storage, now_ms);
        }

        if now_ms.saturating_sub(last_tick_ms) >= 200 {
            last_tick_ms = now_ms;
            app.tick(sink, storage, sensors, power, portal);
        }

        power.poll_hv_standby(now_ms);

        if now_ms < 30_000 && now_ms.saturating_sub(last_hb_ms) >= 3_000 {
            last_hb_ms = now_ms;
            let hb_bat = sensors.battery();
            let (heap_free, heap_max) = heap_stats();
            ulog!(
                "[VVBOOK-OS] HEARTBEAT v0.6.3 up={}ms books={} bat={}% {}mV heap_free={} heap_max={}",
                now_ms,
                app.catalog.len(),
                hb_bat.percent,
                hb_bat.millivolts,
                heap_free,
                heap_max
            );
        }

        EspSysTimer.delay_us(if app.screen == Screen::VvDrop { 250 } else { 2_000 });
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    static mut IN_PANIC: bool = false;
    if unsafe { ptr::read_volatile(&raw const IN_PANIC) } {
        software_reset();
    }
    unsafe { ptr::write_volatile(&raw mut IN_PANIC, true) };

    ulog!("[VVBOOK-OS] PANIC: {}", info);
    UsbSerialLog::flush();

    unhold_all_pads();
    gpio_set(EPD_RST, false);
    gpio_set(SD_CS, true);
    gpio_set(SD_POWER_EN, false);

    let prev = mmio_read(RTC_CNTL_STORE6_REG);
    let retries = if (prev & 0xFFFF_FF00) == PANIC_RTC_MAGIC {
        prev & 0xFF
    } else {
        0
    };
    if retries < 3 {
        mmio_write(RTC_CNTL_STORE6_REG, PANIC_RTC_MAGIC | (retries + 1));
        EspSysTimer.delay_us(30_000);
        software_reset();
    }

    mmio_write(RTC_CNTL_STORE6_REG, 0);
    gpio_config_input(PWR_BTN, true);
    wait_for_power_release_ms(2_000);
    let mut rtc = Rtc::new(unsafe { LPWR::steal() });
    let mut pwr_pin = unsafe { GPIO3::steal() };
    enter_hardware_deep_sleep(&mut rtc, &mut pwr_pin, false, None);
}
