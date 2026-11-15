#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::alloc::{GlobalAlloc, Layout};
use core::arch::{asm, global_asm};
use core::cell::{RefCell, UnsafeCell};
use core::fmt::{self, Write};
use core::panic::PanicInfo;
use core::ptr;

use alloc::rc::Rc;
use vvbook_pal::{
    Button, DisplaySink, InputEvent, PORTRAIT_H, PORTRAIT_W, ROW_2BPP_BYTES, RefreshMode,
    StorageBackend,
};
use vvbook_x3_hal::pins::{
    EPD_BUSY, EPD_CS, EPD_DC, EPD_RST, I2C_SCL, I2C_SDA, PWR_BTN, SD_CS, SD_POWER_EN, SPI_MISO,
    SPI_MOSI, SPI_SCLK,
};
use vvbook_x3_hal::{
    AdcButtonDecoder, Bq27220, DelayUs, Ds3231, EpdPins, Fat32Volume, HalError, I2cBus, Qmi8658,
    SdControlPins, SpiBus, SpiSdhcCard, X3EpdDriver,
};

// ============================================================================
// 1. RISC-V RAM Entry Point (no .data copy; load-ram places .data in DRAM)
// ============================================================================

global_asm!(
    r#"
    .section .text.init, "ax"
    .global _start
_start:
    .option push
    .option norelax
    la gp, __global_pointer$
    .option pop
    la sp, _stack_top

    /* Zero .bss */
    la a0, _sbss
    la a1, _ebss
1:
    bgeu a0, a1, 2f
    sw zero, 0(a0)
    addi a0, a0, 4
    j 1b
2:
    tail rust_main
"#
);

// ============================================================================
// 2. Coalescing Free-List Global Allocator (144 KB DRAM Arena)
// ============================================================================

const HEAP_BYTES: usize = 144 * 1024;

#[repr(align(16))]
struct HeapArena([u8; HEAP_BYTES]);

static mut HEAP_MEMORY: HeapArena = HeapArena([0; HEAP_BYTES]);

#[repr(C)]
struct FreeNode {
    size: usize,
    next: *mut FreeNode,
}

struct CoalescingAllocator {
    head: UnsafeCell<*mut FreeNode>,
    initialized: UnsafeCell<bool>,
}

unsafe impl Sync for CoalescingAllocator {}

#[global_allocator]
static ALLOCATOR: CoalescingAllocator = CoalescingAllocator {
    head: UnsafeCell::new(ptr::null_mut()),
    initialized: UnsafeCell::new(false),
};

impl CoalescingAllocator {
    #[inline]
    fn enter_cs() -> usize {
        let mstatus: usize;
        unsafe { asm!("csrrci {0}, mstatus, 8", out(reg) mstatus, options(nomem, nostack)) };
        mstatus
    }

    #[inline]
    fn exit_cs(mstatus: usize) {
        if (mstatus & 8) != 0 {
            unsafe { asm!("csrsi mstatus, 8", options(nomem, nostack)) };
        }
    }

    unsafe fn ensure_init(&self) -> *mut FreeNode {
        if unsafe { !*self.initialized.get() } {
            let base = unsafe { ptr::addr_of_mut!(HEAP_MEMORY.0) as *mut u8 };
            let first = base as *mut FreeNode;
            unsafe {
                (*first).size = HEAP_BYTES;
                (*first).next = ptr::null_mut();
                *self.head.get() = first;
                *self.initialized.get() = true;
            }
        }
        unsafe { *self.head.get() }
    }
}

const HDR_SIZE: usize = 16;

#[inline]
fn align_up(v: usize, align: usize) -> usize {
    (v + align - 1) & !(align - 1)
}

unsafe impl GlobalAlloc for CoalescingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let cs = Self::enter_cs();
        let align = layout.align().max(16);
        let need = align_up(layout.size().max(16), 16);

        let mut head = unsafe { self.ensure_init() };
        let mut prev: *mut FreeNode = ptr::null_mut();
        let mut cur = head;

        while !cur.is_null() {
            let cur_size = unsafe { (*cur).size };
            let base_addr = cur as usize;
            let user_addr = align_up(base_addr + HDR_SIZE, align);
            let hdr_addr = user_addr - HDR_SIZE;
            let end_addr = user_addr + need;
            let needed_from_base = end_addr - base_addr;

            if cur_size >= needed_from_base {
                let rem = cur_size - needed_from_base;
                let next_cur = unsafe { (*cur).next };
                let replacement = if rem >= 32 {
                    let tail = end_addr as *mut FreeNode;
                    unsafe {
                        (*tail).size = rem;
                        (*tail).next = next_cur;
                    }
                    tail
                } else {
                    next_cur
                };
                if prev.is_null() {
                    head = replacement;
                    unsafe {
                        *self.head.get() = head;
                    }
                } else {
                    unsafe {
                        (*prev).next = replacement;
                    }
                }
                let actual_block = if rem >= 32 {
                    needed_from_base
                } else {
                    cur_size
                };
                let prefix_pad = hdr_addr - base_addr;
                let meta_ptr = hdr_addr as *mut usize;
                unsafe {
                    *meta_ptr = actual_block - prefix_pad;
                    *meta_ptr.add(1) = base_addr;
                }
                Self::exit_cs(cs);
                return user_addr as *mut u8;
            }
            prev = cur;
            cur = unsafe { (*cur).next };
        }

        Self::exit_cs(cs);
        ptr::null_mut()
    }

    unsafe fn dealloc(&self, ptr_user: *mut u8, _layout: Layout) {
        if ptr_user.is_null() {
            return;
        }
        let cs = Self::enter_cs();
        let hdr = unsafe { ptr_user.sub(HDR_SIZE) as *mut usize };
        let payload_span = unsafe { *hdr };
        let base_addr = unsafe { *hdr.add(1) };
        let full_size = ((ptr_user as usize - HDR_SIZE) - base_addr) + payload_span;

        let node = base_addr as *mut FreeNode;
        let mut cur = unsafe { *self.head.get() };
        let mut prev: *mut FreeNode = ptr::null_mut();

        while !cur.is_null() && (cur as usize) < base_addr {
            prev = cur;
            cur = unsafe { (*cur).next };
        }

        unsafe {
            (*node).size = full_size;
            (*node).next = cur;
        }
        if prev.is_null() {
            unsafe {
                *self.head.get() = node;
            }
        } else {
            unsafe {
                (*prev).next = node;
            }
        }

        if !cur.is_null() && base_addr + full_size == cur as usize {
            unsafe {
                (*node).size += (*cur).size;
                (*node).next = (*cur).next;
            }
        }
        if !prev.is_null() && (prev as usize) + unsafe { (*prev).size } == node as usize {
            unsafe {
                (*prev).size += (*node).size;
                (*prev).next = (*node).next;
            }
        }
        Self::exit_cs(cs);
    }
}

// ============================================================================
// 3. ESP32-C3 Verified MMIO Registers
// ============================================================================

const SYSTEM_PERIP_CLK_EN0_REG: usize = 0x600C_0010;
const SYSTEM_PERIP_CLK_EN1_REG: usize = 0x600C_0014;
const SYSTEM_PERIP_RST_EN0_REG: usize = 0x600C_0018;
const SYSTEM_PERIP_RST_EN1_REG: usize = 0x600C_001C;
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

const RTC_CNTL_WDTCONFIG0: usize = 0x6000_8090;
const RTC_CNTL_WDTWPROTECT: usize = 0x6000_80A8;
const RTC_CNTL_SWD_CONF: usize = 0x6000_80AC;
const RTC_CNTL_SWD_WPROTECT: usize = 0x6000_80B0;
const RTC_CNTL_DIG_PAD_HOLD_REG: usize = 0x6000_80D4;
const RTC_CNTL_SENSOR_CTRL_REG: usize = 0x6000_811C;

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

const USB_SERIAL_JTAG_EP1_REG: usize = 0x6004_3000;
const USB_SERIAL_JTAG_EP1_CONF_REG: usize = 0x6004_3004;

/// Read a 32-bit ESP32-C3 peripheral MMIO register at `addr`.
///
/// # Safety
/// `addr` must be a valid, 4-byte-aligned ESP32-C3 peripheral register in
/// `0x6000_0000..0x6010_0000` whose volatile read is valid for the current hardware state.
#[inline]
unsafe fn mmio_read(addr: usize) -> u32 {
    debug_assert!(addr & 3 == 0 && (0x6000_0000..0x6010_0000).contains(&addr));
    unsafe { ptr::read_volatile(addr as *const u32) }
}

/// Write `val` to a 32-bit ESP32-C3 peripheral MMIO register at `addr`.
///
/// # Safety
/// `addr` must be a valid, 4-byte-aligned ESP32-C3 peripheral register in
/// `0x6000_0000..0x6010_0000` and `val` must preserve peripheral and memory safety invariants.
#[inline]
unsafe fn mmio_write(addr: usize, val: u32) {
    debug_assert!(addr & 3 == 0 && (0x6000_0000..0x6010_0000).contains(&addr));
    unsafe { ptr::write_volatile(addr as *mut u32, val) }
}

fn disable_watchdogs() {
    // SAFETY: Disabling TIMG0/TIMG1/RTC/SWD watchdogs uses documented WDT protect & config MMIO registers on single-core ESP32-C3.
    unsafe {
        mmio_write(TIMG0_WDTWPROTECT, 0x50D8_3AA1);
        mmio_write(TIMG0_WDTCONFIG0, 0);
        mmio_write(TIMG1_WDTWPROTECT, 0x50D8_3AA1);
        mmio_write(TIMG1_WDTCONFIG0, 0);

        mmio_write(RTC_CNTL_WDTWPROTECT, 0x50D8_3AA1);
        mmio_write(RTC_CNTL_WDTCONFIG0, 0);

        mmio_write(RTC_CNTL_SWD_WPROTECT, 0x8F1D_312A);
        let swd = mmio_read(RTC_CNTL_SWD_CONF);
        mmio_write(RTC_CNTL_SWD_CONF, swd | (1 << 30));
    }
}

fn enable_periph_clocks() {
    let bits0 = (1 << 6) | (1 << 7) | (1 << 28); // SPI2 | I2C_EXT0 | APB_SARADC
    let bits1 = 1 << 10; // TSENS / SARADC analog clock gate
    // SAFETY: SYSTEM_PERIP_* registers are valid ESP32-C3 clock/reset MMIO registers.
    unsafe {
        mmio_write(SYSTEM_PERIP_CLK_EN0_REG, mmio_read(SYSTEM_PERIP_CLK_EN0_REG) | bits0);
        mmio_write(SYSTEM_PERIP_RST_EN0_REG, mmio_read(SYSTEM_PERIP_RST_EN0_REG) | bits0);
        mmio_write(SYSTEM_PERIP_RST_EN0_REG, mmio_read(SYSTEM_PERIP_RST_EN0_REG) & !bits0);

        mmio_write(SYSTEM_PERIP_CLK_EN1_REG, mmio_read(SYSTEM_PERIP_CLK_EN1_REG) | bits1);
        mmio_write(SYSTEM_PERIP_RST_EN1_REG, mmio_read(SYSTEM_PERIP_RST_EN1_REG) | bits1);
        mmio_write(SYSTEM_PERIP_RST_EN1_REG, mmio_read(SYSTEM_PERIP_RST_EN1_REG) & !bits1);
    }
}

fn gpio_config_output(pin: u8, initial_high: bool) {
    // SAFETY: IO_MUX_GPIOx and GPIO_* registers are valid for valid ESP32-C3 GPIO pin indices.
    unsafe {
        mmio_write(IO_MUX_GPIO_BASE + usize::from(pin) * 4, (1 << 9) | (1 << 12));
        mmio_write(GPIO_FUNC_OUT_SEL_CFG_BASE + usize::from(pin) * 4, 128);
        if initial_high {
            mmio_write(GPIO_OUT_W1TS_REG, 1 << pin);
        } else {
            mmio_write(GPIO_OUT_W1TC_REG, 1 << pin);
        }
        mmio_write(GPIO_ENABLE_W1TS_REG, 1 << pin);
    }
}

fn gpio_config_input(pin: u8, pull_up: bool) {
    let mut mux = (1 << 9) | (1 << 12); // FUN_IE | MCU_SEL=GPIO
    if pull_up {
        mux |= 1 << 8; // FUN_WPU
    }
    // SAFETY: Valid GPIO/IO_MUX MMIO registers for input pin configuration.
    unsafe {
        mmio_write(GPIO_ENABLE_W1TC_REG, 1 << pin);
        mmio_write(IO_MUX_GPIO_BASE + usize::from(pin) * 4, mux);
    }
}

fn gpio_config_analog(pin: u8) {
    // SAFETY: Valid GPIO/IO_MUX MMIO registers for analog pin isolation.
    unsafe {
        mmio_write(GPIO_ENABLE_W1TC_REG, 1 << pin);
        // MCU_SEL = 1 only: NO FUN_WPU, NO FUN_IE so resistor ladders read cleanly
        mmio_write(IO_MUX_GPIO_BASE + usize::from(pin) * 4, 1 << 12);
    }
}

#[inline]
fn gpio_set(pin: u8, high: bool) {
    // SAFETY: GPIO_OUT_W1TS/W1TC are write-one-to-set/clear MMIO registers.
    unsafe {
        if high {
            mmio_write(GPIO_OUT_W1TS_REG, 1 << pin);
        } else {
            mmio_write(GPIO_OUT_W1TC_REG, 1 << pin);
        }
    }
}

#[inline]
fn gpio_get(pin: u8) -> bool {
    // SAFETY: GPIO_IN_REG is a read-only GPIO level MMIO register.
    (unsafe { mmio_read(GPIO_IN_REG) } & (1 << pin)) != 0
}

#[derive(Clone, Copy)]
struct EspSysTimer;

impl EspSysTimer {
    fn init() {
        // SAFETY: SYSTIMER_CONF_REG enables unit0 clock and core stall mirror.
        unsafe { mmio_write(SYSTIMER_CONF_REG, (1 << 30) | (1 << 31)) };
    }

    fn now_us() -> u64 {
        // SAFETY: SYSTIMER_UNIT0_* MMIO registers latch and read the 52-bit 16 MHz counter.
        unsafe {
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
    }

    fn now_ms() -> u64 {
        Self::now_us() / 1_000
    }
}

impl DelayUs for EspSysTimer {
    fn delay_us(&mut self, us: u32) {
        let start = Self::now_us();
        let target = start.saturating_add(u64::from(us));
        while Self::now_us() < target {
            core::hint::spin_loop();
        }
    }
}

struct UsbSerialLog;

impl UsbSerialLog {
    fn write_byte(b: u8) {
        for _ in 0..4_000 {
            // SAFETY: USB_SERIAL_JTAG_EP1* are valid MMIO registers on ESP32-C3.
            unsafe {
                if (mmio_read(USB_SERIAL_JTAG_EP1_CONF_REG) & 2) != 0 {
                    mmio_write(USB_SERIAL_JTAG_EP1_REG, u32::from(b));
                    if b == b'\n' {
                        mmio_write(USB_SERIAL_JTAG_EP1_CONF_REG, 1);
                    }
                    return;
                }
            }
        }
    }

    fn flush() {
        // SAFETY: Valid USB Serial/JTAG TX flush control register write.
        unsafe { mmio_write(USB_SERIAL_JTAG_EP1_CONF_REG, 1) };
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

// ============================================================================
// 4. SPI2, Open-Drain Bit-Bang I2C, EPD Pins, ADC1 Oneshot
// ============================================================================

fn spi2_update_cmd() {
    // SAFETY: SPI2_CMD_REG bit 23 updates clock/user registers cleanly while idle.
    unsafe {
        mmio_write(SPI2_CMD_REG, mmio_read(SPI2_CMD_REG) | (1 << 23));
        for _ in 0..200_000 {
            if (mmio_read(SPI2_CMD_REG) & (1 << 23)) == 0 {
                break;
            }
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
    // SAFETY: SPI2_CLOCK_REG is a valid SPI2 peripheral MMIO register.
    unsafe { mmio_write(SPI2_CLOCK_REG, (pre << 18) | (n << 12) | (h << 6) | l) };
    spi2_update_cmd();
}

struct Esp32Spi2;

impl Esp32Spi2 {
    fn new() -> Self {
        gpio_config_output(SPI_SCLK, false);
        gpio_config_output(SPI_MOSI, false);
        gpio_config_input(SPI_MISO, true);
        // SAFETY: GPIO matrix and SPI2 registers are valid ESP32-C3 MMIO addresses.
        unsafe {
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
        }
        spi2_apply_freq_hz(400_000);
        Self
    }

    fn xfer_chunk(&mut self, tx: &[u8], rx: &mut [u8]) {
        let len = tx.len().max(rx.len()).min(64);
        if len == 0 {
            return;
        }
        // SAFETY: `base < 64` keeps `SPI2_W0_REG + base` inside SPI2's 16-word FIFO (`W0..W15`).
        unsafe {
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
            mmio_write(SPI2_CMD_REG, mmio_read(SPI2_CMD_REG) | (1 << 24));
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
    }
}

#[derive(Clone)]
struct SharedSpi2(Rc<RefCell<Esp32Spi2>>);

impl SpiBus for SharedSpi2 {
    fn set_frequency_hz(&mut self, hz: u32) {
        spi2_apply_freq_hz(hz);
    }

    fn write(&mut self, data: &[u8]) -> Result<(), HalError> {
        let mut bus = self.0.borrow_mut();
        for chunk in data.chunks(64) {
            bus.xfer_chunk(chunk, &mut []);
        }
        Ok(())
    }

    fn transfer(&mut self, tx: &[u8], rx: &mut [u8]) -> Result<(), HalError> {
        let mut bus = self.0.borrow_mut();
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

struct EspEpdPins {
    last_0x70: [u8; 3],
}

impl EspEpdPins {
    fn init() -> Self {
        gpio_config_output(EPD_CS, true);
        gpio_config_output(EPD_DC, false);
        gpio_config_output(EPD_RST, true);
        gpio_config_input(EPD_BUSY, false);
        Self { last_0x70: [0xFF; 3] }
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
        // SAFETY: Restore GPIO matrix SPI2 SCLK/MOSI signal routing after 3-wire read.
        unsafe {
            mmio_write(GPIO_FUNC_OUT_SEL_CFG_BASE + usize::from(SPI_SCLK) * 4, 63);
            mmio_write(GPIO_FUNC_OUT_SEL_CFG_BASE + usize::from(SPI_MOSI) * 4, 65);
        }
        self.last_0x70 = out;
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
            // SAFETY: Valid IO_MUX and GPIO open-drain configuration registers for I2C pins.
            unsafe {
                mmio_write(
                    IO_MUX_GPIO_BASE + usize::from(pin) * 4,
                    (1 << 8) | (1 << 9) | (1 << 12),
                );
                mmio_write(GPIO_FUNC_OUT_SEL_CFG_BASE + usize::from(pin) * 4, 128);
                mmio_write(GPIO_PIN0_REG + usize::from(pin) * 4, 1 << 2); // PAD_DRIVER open-drain
                mmio_write(GPIO_OUT_W1TS_REG, 1 << pin);
                mmio_write(GPIO_ENABLE_W1TS_REG, 1 << pin);
            }
        }
        // 9-clock bus recovery + STOP
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
        gpio_set(I2C_SDA, true); // release for ACK
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
    // SAFETY: Analog PLL / REGI2C clock gate registers are valid ESP32-C3 MMIO addresses.
    let saved_gate = unsafe {
        mmio_write(ANA_CONF0_REG, mmio_read(ANA_CONF0_REG) & !(1 << 18));
        mmio_write(ANA_CONF1_REG, mmio_read(ANA_CONF1_REG) | (1 << 16));
        let saved_gate = mmio_read(REGI2C_CLK_GATE_REG);
        mmio_write(REGI2C_CLK_GATE_REG, 4);
        saved_gate
    };
    let rom_fn: extern "C" fn(u8, u8, u8, u8, u8, u8) =
        unsafe { core::mem::transmute(ROM_I2C_WRITEREG_MASK) };
    rom_fn(0x69, 0, reg, msb, lsb, data);
    // SAFETY: Restores previous REGI2C clock gate register state.
    unsafe { mmio_write(REGI2C_CLK_GATE_REG, saved_gate) };
}

fn adc1_oneshot_raw_cfg(cfg: u32) -> u16 {
    // SAFETY: APB_SARADC_* registers are valid SAR ADC1 MMIO registers after clock enable.
    unsafe {
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
        mmio_write(APB_SARADC_ONETIME_SAMPLE, cfg);
        val
    }
}

fn init_adc1() -> u16 {
    // SAFETY: RTC_CNTL_SENSOR_CTRL_REG and APB_SARADC_* registers are valid ESP32-C3 MMIO addresses.
    unsafe {
        let sens = mmio_read(RTC_CNTL_SENSOR_CTRL_REG) & 0x07FF_FFFF;
        mmio_write(RTC_CNTL_SENSOR_CTRL_REG, sens | (4 << 27));
        mmio_write(
            APB_SARADC_CTRL_REG,
            mmio_read(APB_SARADC_CTRL_REG) | (1 << 6) | (3 << 27),
        );
        let clkm = mmio_read(APB_SARADC_CLKM_CONF_REG) & !((3 << 21) | 0xFF);
        mmio_write(APB_SARADC_CLKM_CONF_REG, clkm | (2 << 21) | (1 << 20) | 4);
    }
    regi2c_saradc_write_mask(6, 3, 0, 0x0F); // I2C_SARADC_DREF = 15
    EspSysTimer.delay_us(300);

    let gnd_cfg = (1 << 31) | (0x0F << 25) | (3 << 23);
    regi2c_saradc_write_mask(7, 5, 5, 1); // ENCAL_GND = 1
    let mut lo = 0u16;
    let mut hi = 4096u16;
    while hi - lo > 1 {
        let mid = (lo + hi) / 2;
        regi2c_saradc_write_mask(1, 3, 0, ((mid >> 8) & 0x0F) as u8);
        regi2c_saradc_write_mask(0, 7, 0, (mid & 0xFF) as u8);
        if adc1_oneshot_raw_cfg(gnd_cfg) == 0 {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    let init_code = (lo + hi) / 2;
    regi2c_saradc_write_mask(1, 3, 0, ((init_code >> 8) & 0x0F) as u8);
    regi2c_saradc_write_mask(0, 7, 0, (init_code & 0xFF) as u8);
    regi2c_saradc_write_mask(7, 5, 5, 0); // ENCAL_GND = 0
    EspSysTimer.delay_us(100);
    init_code
}

fn read_adc1_raw_mv(channel: u8) -> (u16, u16) {
    let cfg = (1 << 31) | (u32::from(channel & 0x0F) << 25) | (3 << 23);
    let raw = adc1_oneshot_raw_cfg(cfg);
    let mv = ((u32::from(raw) * 3100) / 4095) as u16;
    (raw, mv)
}

// ============================================================================
// 5. Compact 5x7 Bitmap Font & 528x792 Strip Renderer for Stage 2 Card
// ============================================================================

fn glyph_5x7(ch: char) -> [u8; 7] {
    match ch.to_ascii_uppercase() {
        'A' => [0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'B' => [0x1E, 0x11, 0x11, 0x1E, 0x11, 0x11, 0x1E],
        'C' => [0x0F, 0x10, 0x10, 0x10, 0x10, 0x10, 0x0F],
        'D' => [0x1E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x1E],
        'E' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F],
        'F' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x10],
        'G' => [0x0F, 0x10, 0x10, 0x17, 0x11, 0x11, 0x0F],
        'H' => [0x11, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'I' => [0x0E, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E],
        'J' => [0x07, 0x02, 0x02, 0x02, 0x02, 0x12, 0x0C],
        'K' => [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11],
        'L' => [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1F],
        'M' => [0x11, 0x1B, 0x15, 0x11, 0x11, 0x11, 0x11],
        'N' => [0x11, 0x19, 0x15, 0x13, 0x11, 0x11, 0x11],
        'O' => [0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'P' => [0x1E, 0x11, 0x11, 0x1E, 0x10, 0x10, 0x10],
        'Q' => [0x0E, 0x11, 0x11, 0x11, 0x15, 0x12, 0x0D],
        'R' => [0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x11],
        'S' => [0x0F, 0x10, 0x10, 0x0E, 0x01, 0x01, 0x1E],
        'T' => [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        'U' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'V' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x0A, 0x04],
        'W' => [0x11, 0x11, 0x11, 0x11, 0x15, 0x1B, 0x11],
        'X' => [0x11, 0x11, 0x0A, 0x04, 0x0A, 0x11, 0x11],
        'Y' => [0x11, 0x11, 0x0A, 0x04, 0x04, 0x04, 0x04],
        'Z' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x10, 0x1F],
        '0' => [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
        '1' => [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
        '2' => [0x0E, 0x11, 0x01, 0x06, 0x08, 0x10, 0x1F],
        '3' => [0x1E, 0x01, 0x01, 0x0E, 0x01, 0x01, 0x1E],
        '4' => [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
        '5' => [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E],
        '6' => [0x0E, 0x10, 0x10, 0x1E, 0x11, 0x11, 0x0E],
        '7' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        '8' => [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
        '9' => [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x01, 0x0E],
        ':' => [0x00, 0x04, 0x04, 0x00, 0x04, 0x04, 0x00],
        '.' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x04],
        ',' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x08],
        '-' => [0x00, 0x00, 0x00, 0x1F, 0x00, 0x00, 0x00],
        '+' => [0x00, 0x04, 0x04, 0x1F, 0x04, 0x04, 0x00],
        '/' => [0x01, 0x02, 0x02, 0x04, 0x08, 0x08, 0x10],
        '=' => [0x00, 0x00, 0x1F, 0x00, 0x1F, 0x00, 0x00],
        '%' => [0x19, 0x1A, 0x02, 0x04, 0x08, 0x0B, 0x13],
        '(' => [0x02, 0x04, 0x08, 0x08, 0x08, 0x04, 0x02],
        ')' => [0x08, 0x04, 0x02, 0x02, 0x02, 0x04, 0x08],
        '[' => [0x0E, 0x08, 0x08, 0x08, 0x08, 0x08, 0x0E],
        ']' => [0x0E, 0x02, 0x02, 0x02, 0x02, 0x02, 0x0E],
        '!' => [0x04, 0x04, 0x04, 0x04, 0x04, 0x00, 0x04],
        '|' => [0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        '_' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1F],
        _ => [0; 7],
    }
}

fn draw_text_strip(
    strip: &mut [u8],
    y0: usize,
    rows: usize,
    tx: usize,
    ty: usize,
    scale: usize,
    text: &str,
) {
    let char_h = 7 * scale;
    if ty + char_h <= y0 || ty >= y0 + rows {
        return;
    }
    for (ci, ch) in text.chars().enumerate() {
        let g = glyph_5x7(ch);
        let cx0 = tx + ci * 6 * scale;
        for (gr, row_bits) in g.iter().enumerate() {
            for gc in 0..5usize {
                if (row_bits >> (4 - gc)) & 1 != 0 {
                    for sy in 0..scale {
                        let py = ty + gr * scale + sy;
                        if py < y0 || py >= y0 + rows {
                            continue;
                        }
                        let row_off = (py - y0) * ROW_2BPP_BYTES;
                        for sx in 0..scale {
                            let px = cx0 + gc * scale + sx;
                            if px < PORTRAIT_W {
                                let idx = row_off + (px >> 2);
                                let shift = 6 - (px & 3) * 2;
                                strip[idx] &= !(0b11 << shift); // Tone::Ink (00)
                            }
                        }
                    }
                }
            }
        }
    }
}

fn set_pixel_ink(strip: &mut [u8], y0: usize, rows: usize, px: usize, py: usize) {
    if py < y0 || py >= y0 + rows || px >= PORTRAIT_W {
        return;
    }
    let idx = (py - y0) * ROW_2BPP_BYTES + (px >> 2);
    let shift = 6 - (px & 3) * 2;
    strip[idx] &= !(0b11 << shift);
}

fn render_self_test_screen(
    epd: &mut X3EpdDriver<SharedSpi2, EspEpdPins, EspSysTimer>,
    lines: &[String],
) -> Result<(), HalError> {
    const STRIP_ROWS: usize = 16;
    let mut strip = [0xFFu8; STRIP_ROWS * ROW_2BPP_BYTES];
    epd.begin_frame(RefreshMode::FullGc);

    for y0 in (0..PORTRAIT_H).step_by(STRIP_ROWS) {
        let rows = (PORTRAIT_H - y0).min(STRIP_ROWS);
        strip[..rows * ROW_2BPP_BYTES].fill(0xFF);

        // Bauhaus Minimalist orthogonal frame + corner orientation tags + geometric header block
        for dy in 0..rows {
            let py = y0 + dy;
            if (16..=18).contains(&py) || (PORTRAIT_H - 19..=PORTRAIT_H - 17).contains(&py) {
                for px in 16..PORTRAIT_W - 16 {
                    set_pixel_ink(&mut strip, y0, rows, px, py);
                }
            } else if py > 18 && py < PORTRAIT_H - 19 {
                for px in [16, 17, 18, PORTRAIT_W - 19, PORTRAIT_W - 18, PORTRAIT_W - 17] {
                    set_pixel_ink(&mut strip, y0, rows, px, py);
                }
            }
            // Bauhaus top-left black square monument
            if (38..=114).contains(&py) {
                for px in 28..42 {
                    set_pixel_ink(&mut strip, y0, rows, px, py);
                }
            }
            // Top-right geometric square index block
            if (50..=78).contains(&py) {
                for px in PORTRAIT_W - 72..PORTRAIT_W - 44 {
                    set_pixel_ink(&mut strip, y0, rows, px, py);
                }
            }
            // Strong horizontal structural rule below header
            if (130..=133).contains(&py) {
                for px in 28..PORTRAIT_W - 28 {
                    set_pixel_ink(&mut strip, y0, rows, px, py);
                }
            }
            // Footer structural rule + solid left anchor
            if (694..=697).contains(&py) {
                for px in 28..PORTRAIT_W - 28 {
                    set_pixel_ink(&mut strip, y0, rows, px, py);
                }
            }
            if (704..=764).contains(&py) {
                for px in 28..40 {
                    set_pixel_ink(&mut strip, y0, rows, px, py);
                }
            }
        }

        draw_text_strip(&mut strip, y0, rows, 24, 22, 2, "[TL]");
        draw_text_strip(&mut strip, y0, rows, PORTRAIT_W - 76, 22, 2, "[TR]");
        draw_text_strip(&mut strip, y0, rows, 24, PORTRAIT_H - 36, 2, "[BL]");
        draw_text_strip(&mut strip, y0, rows, PORTRAIT_W - 76, PORTRAIT_H - 36, 2, "[BR]");

        draw_text_strip(&mut strip, y0, rows, 54, 48, 3, "VVBOOK // FOLIO");
        draw_text_strip(&mut strip, y0, rows, 54, 92, 2, "STAGE 2 BAUHAUS DIAGNOSTIC");

        for (i, line) in lines.iter().enumerate() {
            let ty = 154 + i * 34;
            if ty + 14 < PORTRAIT_H - 104 {
                draw_text_strip(&mut strip, y0, rows, 36, ty, 2, line);
            }
        }

        draw_text_strip(&mut strip, y0, rows, 52, 712, 2, "STOCK FLASH: 100% UNTOUCHED");
        draw_text_strip(&mut strip, y0, rows, 52, 742, 2, "PURE SRAM EXECUTION ACTIVE");

        epd.write_strip_2bpp(y0, rows, &strip[..rows * ROW_2BPP_BYTES]);
    }

    epd.try_end_frame(RefreshMode::FullGc)
}

fn btn_name(b: Button) -> &'static str {
    match b {
        Button::Up => "UP",
        Button::Down => "DOWN",
        Button::Left => "LEFT",
        Button::Right => "RIGHT",
        Button::Confirm => "CONFIRM",
        Button::Back => "BACK",
        Button::Power => "POWER",
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_main() -> ! {
    disable_watchdogs();
    EspSysTimer::init();
    enable_periph_clocks();

    let clear_hold = (1 << EPD_RST) | (1 << SD_POWER_EN) | (1 << PWR_BTN);
    // SAFETY: Clears RTC digital pad hold bits for awake peripheral GPIOs.
    unsafe {
        mmio_write(
            RTC_CNTL_DIG_PAD_HOLD_REG,
            mmio_read(RTC_CNTL_DIG_PAD_HOLD_REG) & !clear_hold,
        );
    }

    gpio_config_output(SD_POWER_EN, false);
    gpio_config_output(SD_CS, true);
    gpio_config_output(EPD_RST, true);
    gpio_config_input(PWR_BTN, true);
    gpio_config_analog(1);
    gpio_config_analog(2);

    // Wait 500ms after load-ram so USB host can open /dev/ttyACM0 cleanly
    EspSysTimer.delay_us(500_000);
    ulog!("[VVBOOK-STAGE2] === VVBook Folio OS Stage 2 RAM Probe (0 flash bytes modified) ===");

    // 1. I2C scan & sensor telemetry
    let mut i2c = BitBangI2c::init();
    let mut found_i2c = Vec::<u8>::new();
    for addr in 0x08u8..=0x77 {
        if i2c.write(addr, &[]).is_ok() {
            found_i2c.push(addr);
        }
    }
    ulog!("[VVBOOK-STAGE2] I2C ACK addresses ({}): {:02X?}", found_i2c.len(), &found_i2c);

    let mut bq = Bq27220::new(EspSysTimer);
    let bat_line = match bq.read_battery(&mut i2c) {
        Ok(b) => {
            let temp = bq.read_temperature_c_x10(&mut i2c).unwrap_or(0);
            let s = format!(
                "BAT: {}%  {}MV  {}MA  {}.{}C",
                b.percent,
                b.millivolts,
                b.current_ma,
                temp / 10,
                (temp % 10).abs()
            );
            ulog!("[VVBOOK-STAGE2] {}", s);
            s
        }
        Err(e) => {
            let s = format!("BAT: ERR ({:?})", e);
            ulog!("[VVBOOK-STAGE2] {}", s);
            s
        }
    };

    let rtc_line = match Ds3231::read_datetime(&mut i2c) {
        Ok(dt) => {
            let s = format!(
                "RTC: {:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                dt.year, dt.month, dt.day, dt.hour, dt.minute, dt.second
            );
            ulog!("[VVBOOK-STAGE2] {}", s);
            s
        }
        Err(e) => {
            let s = format!("RTC: ERR ({:?})", e);
            ulog!("[VVBOOK-STAGE2] {}", s);
            s
        }
    };

    let mut who = [0u8; 1];
    let imu_line = match i2c.write_read(0x6B, &[0x00], &mut who) {
        Ok(()) => {
            let mut imu = Qmi8658::new();
            let _ = imu.set_enabled(&mut i2c, false);
            let s = format!("IMU: QMI8658 ID=0x{:02X} OK", who[0]);
            ulog!("[VVBOOK-STAGE2] {}", s);
            s
        }
        Err(e) => {
            let s = format!("IMU: ERR ({:?})", e);
            ulog!("[VVBOOK-STAGE2] {}", s);
            s
        }
    };

    // 2. Resting ADC1 / Button rails
    let adc_cali = init_adc1();
    ulog!("[VVBOOK-STAGE2] ADC1 calibrated init_code={}", adc_cali);
    let (raw1, mv1) = read_adc1_raw_mv(1);
    let (raw2, mv2) = read_adc1_raw_mv(2);
    let pwr = !gpio_get(PWR_BTN);
    let adc_line = format!("ADC: CH1={}MV CH2={}MV PWR={}", mv1, mv2, u8::from(pwr));
    ulog!(
        "[VVBOOK-STAGE2] ADC REST: ch1={} ({} mV)  ch2={} ({} mV)  pwr={}",
        raw1, mv1, raw2, mv2, pwr
    );

    // 3. Read-only 16 GB microSD check (mounted cleanly, zero writes, then powered down)
    ulog!("[VVBOOK-STAGE2] Mounting microSD (read-only)...");
    let spi = SharedSpi2(Rc::new(RefCell::new(Esp32Spi2::new())));
    let sd_line = {
        let card = SpiSdhcCard::new(spi.clone(), EspSdPins, EspSysTimer, 31_250_000);
        match Fat32Volume::mount(card) {
            Ok(mut vol) => {
                let root_n = vol.list_dir("/").map(|v| v.len()).unwrap_or(0);
                let books_n = vol.list_dir("/books").map(|v| v.len()).unwrap_or(0);
                let s = format!("SD: FAT32 OK ROOT={} BOOKS={}", root_n, books_n);
                ulog!("[VVBOOK-STAGE2] {} (read-only)", s);
                vol.set_power_save(true);
                s
            }
            Err(e) => {
                let s = format!("SD: ERR ({:?})", e);
                ulog!("[VVBOOK-STAGE2] {}", s);
                gpio_set(SD_CS, true);
                gpio_set(SD_POWER_EN, false);
                s
            }
        }
    };

    // 4. EPD 0x70 silicon probe + live self-test render + POF(0x02)
    ulog!("[VVBOOK-STAGE2] Probing EPD silicon (0x70) & rendering Bauhaus card...");
    let epd_pins = EspEpdPins::init();
    match X3EpdDriver::new(spi, epd_pins, EspSysTimer) {
        Ok(mut epd) => {
            let mut lines = Vec::new();
            lines.push(format!("PANEL: {}", epd.silicon().name()));
            lines.push(bat_line);
            lines.push(rtc_line);
            lines.push(imu_line);
            lines.push(sd_line);
            lines.push(adc_line);
            lines.push(String::from("CHIN: [BACK][OK][<][>]"));
            lines.push(String::from("EDGE: [PWR] [^][V]"));
            lines.push(String::from("RAM MODE: NO FLASH WRITTEN"));

            let ver_bytes = epd.pins_mut().last_0x70;
            let busy_now = epd.pins_mut().is_busy();
            ulog!(
                "[VVBOOK-STAGE2] EPD init OK: {} (0x70={:02X?}) busy_after_init={}",
                epd.silicon().name(),
                ver_bytes,
                busy_now
            );
            let t_drf = EspSysTimer::now_ms();
            let drf_res = render_self_test_screen(&mut epd, &lines);
            let drf_ms = EspSysTimer::now_ms().saturating_sub(t_drf);
            let busy_after_drf = epd.pins_mut().is_busy();
            let t_pof = EspSysTimer::now_ms();
            let pof_res = epd.set_hv_standby(true);
            let pof_ms = EspSysTimer::now_ms().saturating_sub(t_pof);
            let busy_after_pof = epd.pins_mut().is_busy();
            ulog!(
                "[VVBOOK-STAGE2] EPD render(PON+DRF)={:?} ({}ms) busy_after_drf={} POF(0x02)={:?} ({}ms) busy_after_pof={}",
                drf_res,
                drf_ms,
                busy_after_drf,
                pof_res,
                pof_ms,
                busy_after_pof
            );
        }
        Err(e) => {
            ulog!("[VVBOOK-STAGE2] EPD INIT ERROR: {:?}", e);
        }
    }

    // 5. Live Button Monitor Loop
    let mut dec = AdcButtonDecoder::new();
    let mut events = Vec::<InputEvent>::with_capacity(4);
    let mut last_hb = 0u64;

    loop {
        let now = EspSysTimer::now_ms();
        let (_, m1) = read_adc1_raw_mv(1);
        let (_, m2) = read_adc1_raw_mv(2);
        let pw = !gpio_get(PWR_BTN);
        events.clear();
        dec.poll_into(now, m1, m2, pw, &mut events);
        for ev in events.drain(..) {
            match ev {
                InputEvent::Press(b) => {
                    ulog!("[VVBOOK-STAGE2] BTN PRESS: {} (ch1={}mV ch2={}mV pwr={})", btn_name(b), m1, m2, pw);
                }
                InputEvent::LongPress(b) => {
                    ulog!("[VVBOOK-STAGE2] BTN LONG: {}", btn_name(b));
                }
                InputEvent::Release(b) => {
                    ulog!("[VVBOOK-STAGE2] BTN RELEASE: {}", btn_name(b));
                }
                _ => {}
            }
        }
        if now.saturating_sub(last_hb) >= 2_000 {
            last_hb = now;
            ulog!("[VVBOOK-STAGE2] HEARTBEAT t={}ms (ch1={}mV ch2={}mV pwr={})", now, m1, m2, pw);
        }
        EspSysTimer.delay_us(20_000);
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    gpio_set(EPD_RST, false);
    gpio_set(SD_POWER_EN, false);
    ulog!("[VVBOOK-STAGE2] PANIC at {:?}!", info.location());
    loop {
        unsafe { asm!("wfi") };
    }
}
