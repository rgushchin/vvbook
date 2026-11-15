/// Dynamic CPU operating point selected by [`ActivePowerGovernor`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CpuPowerMode {
    /// 160 MHz PLL CPU, 80 MHz APB — full speed for rendering, SPI2, I2C, and Wi-Fi.
    Turbo160MHz,
    /// 40 MHz XTAL awake poll + 20 MHz APB / 0 MHz clock-gated RISC-V `wfi` between 6 ms ticks.
    EcoXtal40MHz,
}

impl CpuPowerMode {
    /// CPU clock frequency in MHz while awake and servicing interrupts or ADC polls.
    pub const fn awake_mhz(self) -> u32 {
        match self {
            Self::Turbo160MHz => 160,
            Self::EcoXtal40MHz => 40,
        }
    }

    /// APB bus clock frequency in MHz while waiting inside idle (`wfi` or busy-wait).
    pub const fn idle_apb_mhz(self) -> u32 {
        match self {
            Self::Turbo160MHz => 80,
            Self::EcoXtal40MHz => 20,
        }
    }

    /// Returns `true` when this operating point gates the CPU core via RISC-V `wfi`.
    pub const fn uses_wfi(self) -> bool {
        matches!(self, Self::EcoXtal40MHz)
    }

    /// Short telemetry name for logs and diagnostics.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Turbo160MHz => "Turbo",
            Self::EcoXtal40MHz => "Eco",
        }
    }
}

/// Deterministic Active-Reading Power Governor coordinating `160 MHz` PLL burst
/// execution and `40 MHz` XTAL / `20 MHz` `wfi` clock-gated idle on ESP32-C3.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActivePowerGovernor {
    mode: CpuPowerMode,
    boost_until_ms: u64,
    eco_entries: u32,
    turbo_boosts: u32,
    wfi_sleeps: u64,
}

impl ActivePowerGovernor {
    /// Post-render holdoff window in clean Reading mode (`Screen::Reader`, no overlay)
    /// before scaling down to `40 MHz` XTAL + `20 MHz` APB RISC-V `wfi`.
    pub const READER_HOLDOFF_MS: u64 = 50;
    /// Post-activity holdoff window in interactive menus/overlays before dropping to Eco `wfi`.
    pub const UI_HOLDOFF_MS: u64 = 250;
    /// Default UI holdoff alias preserved for callers checking interactive holdoff.
    pub const HOLDOFF_MS: u64 = Self::UI_HOLDOFF_MS;
    /// Periodic `SYSTIMER_TARGET2` wakeup cadence aligned 1:1 with the 12 ms ADC button poll.
    pub const WFI_PERIOD_MS: u64 = 12;

    /// Creates a new governor starting in `Turbo160MHz` for the initial boot window.
    pub const fn new(boot_ms: u64) -> Self {
        Self {
            mode: CpuPowerMode::Turbo160MHz,
            boost_until_ms: boot_ms.saturating_add(Self::UI_HOLDOFF_MS),
            eco_entries: 0,
            turbo_boosts: 0,
            wfi_sleeps: 0,
        }
    }

    /// Extends or engages `Turbo160MHz` for at least `duration_ms` from `now_ms`.
    pub fn boost(&mut self, now_ms: u64, duration_ms: u64) {
        self.boost_until_ms = self.boost_until_ms.max(now_ms.saturating_add(duration_ms));
        if self.mode != CpuPowerMode::Turbo160MHz {
            self.mode = CpuPowerMode::Turbo160MHz;
            self.turbo_boosts = self.turbo_boosts.saturating_add(1);
        }
    }

    /// Boosts into `Turbo160MHz` for [`Self::READER_HOLDOFF_MS`] in clean Reading mode or
    /// [`Self::UI_HOLDOFF_MS`] otherwise. Clean Reading mode clamps any leftover UI holdoff.
    pub fn note_input(&mut self, now_ms: u64, reading_mode: bool) {
        if reading_mode {
            self.boost_until_ms = now_ms.saturating_add(Self::READER_HOLDOFF_MS);
            if self.mode != CpuPowerMode::Turbo160MHz {
                self.mode = CpuPowerMode::Turbo160MHz;
                self.turbo_boosts = self.turbo_boosts.saturating_add(1);
            }
        } else {
            self.boost(now_ms, Self::UI_HOLDOFF_MS);
        }
    }

    /// Boosts into `Turbo160MHz` for [`Self::UI_HOLDOFF_MS`] on any generic touch or wake event.
    pub fn note_user_input(&mut self, now_ms: u64) {
        self.note_input(now_ms, false);
    }

    /// Evaluates whether the SoC must run at `Turbo160MHz` or can drop into `EcoXtal40MHz`.
    pub fn select_mode(
        &mut self,
        now_ms: u64,
        wifi_active: bool,
        background_io_pending: bool,
    ) -> CpuPowerMode {
        if wifi_active || background_io_pending || now_ms < self.boost_until_ms {
            if self.mode != CpuPowerMode::Turbo160MHz {
                self.mode = CpuPowerMode::Turbo160MHz;
                self.turbo_boosts = self.turbo_boosts.saturating_add(1);
            }
        } else if self.mode != CpuPowerMode::EcoXtal40MHz {
            self.mode = CpuPowerMode::EcoXtal40MHz;
            self.eco_entries = self.eco_entries.saturating_add(1);
        }
        self.mode
    }

    /// Records completed `wfi` idle sleep intervals.
    pub fn record_wfi_sleeps(&mut self, count: u64) {
        self.wfi_sleeps = self.wfi_sleeps.saturating_add(count);
    }

    pub const fn current_mode(&self) -> CpuPowerMode {
        self.mode
    }

    pub const fn boost_until_ms(&self) -> u64 {
        self.boost_until_ms
    }

    pub const fn eco_entries(&self) -> u32 {
        self.eco_entries
    }

    pub const fn turbo_boosts(&self) -> u32 {
        self.turbo_boosts
    }

    pub const fn wfi_sleeps(&self) -> u64 {
        self.wfi_sleeps
    }
}
