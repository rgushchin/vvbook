use alloc::vec::Vec;
use vvbook_pal::{Button, InputEvent};

const DEBOUNCE_MS: u64 = 14;
const LONG_PRESS_MS: u64 = 650;

/// Decodes the dual resistor-ladder ADC button buses (`GPIO 1` & `GPIO 2`) plus
/// the active-low `PWR_BTN` (`GPIO 3`) into debounced `InputEvent`s.
#[derive(Clone, Debug)]
pub struct AdcButtonDecoder {
    rotated_180: bool,
    stable: Option<Button>,
    stable_since_ms: u64,
    long_emitted: bool,
    pending: Option<Button>,
    pending_since_ms: u64,
    suppress_until_release: bool,
    quiet_until_ms: u64,
    release_bounce_ms: u64,
}

impl Default for AdcButtonDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl AdcButtonDecoder {
    pub const fn new() -> Self {
        Self {
            rotated_180: false,
            stable: None,
            stable_since_ms: 0,
            long_emitted: false,
            pending: None,
            pending_since_ms: 0,
            suppress_until_release: false,
            quiet_until_ms: 0,
            release_bounce_ms: 0,
        }
    }

    pub fn set_rotated_180(&mut self, rotated: bool) {
        self.rotated_180 = rotated;
    }

    /// Classifies raw millivolt readings into a raw logical button before debouncing.
    ///
    /// Xteink X3 resistor-ladder calibration (`mv = raw * 3100 / 4095`):
    /// - `GPIO 1` (`mv_a`, 4 front chin buttons):
    ///   - `2347..=2952` -> `Back`
    ///   - `1583..=2346` -> `Confirm`
    ///   - `568..=1582` -> `Left`
    ///   - `0..=567` -> `Right`
    /// - `GPIO 2` (`mv_b`, 2 right-side rocker buttons):
    ///   - `848..=2952` -> `Up`
    ///   - `0..=847` -> `Down`
    /// - `GPIO 3` (`pwr_pressed`, active-low): `Power`
    pub fn classify_raw(&self, mv_a: u16, mv_b: u16, pwr_pressed: bool) -> Option<Button> {
        let raw = if pwr_pressed {
            Some(Button::Power)
        } else if (2347..=2952).contains(&mv_a) {
            Some(Button::Back)
        } else if (1583..=2346).contains(&mv_a) {
            Some(Button::Confirm)
        } else if (568..=1582).contains(&mv_a) {
            Some(Button::Left)
        } else if mv_a <= 567 {
            Some(Button::Right)
        } else if (848..=2952).contains(&mv_b) {
            Some(Button::Up)
        } else if mv_b <= 847 {
            Some(Button::Down)
        } else {
            None
        };

        raw.map(|b| self.apply_rotation(b))
    }

    fn apply_rotation(&self, btn: Button) -> Button {
        if !self.rotated_180 {
            return btn;
        }
        match btn {
            Button::Back => Button::Right,
            Button::Right => Button::Back,
            Button::Confirm => Button::Left,
            Button::Left => Button::Confirm,
            Button::Up => Button::Down,
            Button::Down => Button::Up,
            Button::Power => Button::Power,
        }
    }

    /// Swallows any currently held button until it debounces cleanly back to idle (`None`)
    /// and suppresses new presses/bounces until at least `now_ms + min_settle_ms`.
    pub fn suppress_until_idle(&mut self, now_ms: u64, min_settle_ms: u64) {
        if self.stable.is_some() || self.pending.is_some() {
            self.suppress_until_release = true;
        }
        self.stable = None;
        self.pending = None;
        self.pending_since_ms = now_ms;
        self.long_emitted = true;
        self.release_bounce_ms = min_settle_ms.min(80);
        self.quiet_until_ms = self.quiet_until_ms.max(now_ms.saturating_add(min_settle_ms));
    }

    /// Samples current ADC voltages and appends debounced `Press`, `LongPress`, and `Release`
    /// transitions into `out`.
    pub fn poll_into(
        &mut self,
        now_ms: u64,
        mv_a: u16,
        mv_b: u16,
        pwr_pressed: bool,
        out: &mut Vec<InputEvent>,
    ) {
        let raw = self.classify_raw(mv_a, mv_b, pwr_pressed);

        if self.suppress_until_release {
            if raw.is_some() {
                self.pending = raw;
                self.pending_since_ms = now_ms;
                if self.release_bounce_ms > 0 {
                    self.quiet_until_ms = self
                        .quiet_until_ms
                        .max(now_ms.saturating_add(self.release_bounce_ms));
                }
                return;
            }
            if self.pending.is_some() {
                self.pending = None;
                self.pending_since_ms = now_ms;
                return;
            }
            if now_ms.saturating_sub(self.pending_since_ms) < DEBOUNCE_MS {
                return;
            }
            self.suppress_until_release = false;
            self.stable = None;
            self.long_emitted = false;
            return;
        }

        if now_ms < self.quiet_until_ms {
            if raw.is_some() {
                self.suppress_until_release = true;
                self.pending = raw;
                self.pending_since_ms = now_ms;
            } else {
                self.pending = None;
                self.pending_since_ms = now_ms;
            }
            return;
        }

        if raw != self.pending {
            self.pending = raw;
            self.pending_since_ms = now_ms;
            return;
        }

        if raw != self.stable && now_ms.saturating_sub(self.pending_since_ms) >= DEBOUNCE_MS {
            if let Some(prev) = self.stable {
                out.push(InputEvent::Release(prev));
                self.stable = None;
                self.long_emitted = false;
                if raw.is_some() {
                    // Direct button-to-button slew on shared ADC resistor ladder:
                    // require clean return to idle (`None`) before accepting a new press.
                    self.suppress_until_release = true;
                    self.release_bounce_ms = 0;
                }
            } else if let Some(next) = raw {
                self.stable = Some(next);
                self.stable_since_ms = now_ms;
                self.long_emitted = false;
                out.push(InputEvent::Press(next));
            }
        } else if let Some(held) = self.stable
            && !self.long_emitted
            && now_ms.saturating_sub(self.stable_since_ms) >= LONG_PRESS_MS
        {
            self.long_emitted = true;
            out.push(InputEvent::LongPress(held));
        }
    }

    /// Returns `true` as soon as a raw button voltage dip is detected (even before
    /// the 14 ms debounce completes) and for as long as a button remains held.
    pub fn has_active_or_pending(&self) -> bool {
        self.pending.is_some() || self.stable.is_some()
    }

    /// Convenience single-event poller when only one transition per poll matters.
    pub fn sample(
        &mut self,
        now_ms: u64,
        mv_a: u16,
        mv_b: u16,
        pwr_pressed: bool,
    ) -> Option<InputEvent> {
        let mut tmp = Vec::new();
        self.poll_into(now_ms, mv_a, mv_b, pwr_pressed, &mut tmp);
        tmp.into_iter().next()
    }
}

/// Swallows a held Power button across cold boot / deep-sleep wake so the
/// button press that woke the SoC cannot immediately re-trigger deep sleep.
#[derive(Debug, Clone)]
pub struct WakePowerGuard {
    released_since_boot: bool,
    high_since_ms: Option<u64>,
    allow_power_at_ms: u64,
}

impl WakePowerGuard {
    pub const RELEASE_STABLE_MS: u64 = 40;
    pub const POST_BOOT_GUARD_MS: u64 = 350;

    pub fn new(boot_ready_ms: u64) -> Self {
        Self {
            released_since_boot: false,
            high_since_ms: None,
            allow_power_at_ms: boot_ready_ms.saturating_add(Self::POST_BOOT_GUARD_MS),
        }
    }

    /// Returns the filtered `pwr_pressed` boolean to pass into `AdcButtonDecoder::poll_into`.
    pub fn filter_pwr_pressed(&mut self, now_ms: u64, raw_pwr_pressed: bool) -> bool {
        if !self.released_since_boot {
            if !raw_pwr_pressed {
                let t0 = *self.high_since_ms.get_or_insert(now_ms);
                if now_ms.saturating_sub(t0) >= Self::RELEASE_STABLE_MS {
                    self.released_since_boot = true;
                    // Guarantee at least 120ms bounce guard after a late release.
                    let min_ready = now_ms.saturating_add(120);
                    if self.allow_power_at_ms < min_ready {
                        self.allow_power_at_ms = min_ready;
                    }
                }
            } else {
                self.high_since_ms = None;
            }
            return false;
        }
        if now_ms < self.allow_power_at_ms {
            return false;
        }
        raw_pwr_pressed
    }

    pub fn is_armed(&self) -> bool {
        self.released_since_boot
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wake_power_guard_swallows_held_power_until_clean_release() {
        let mut guard = WakePowerGuard::new(100);
        let mut decoder = AdcButtonDecoder::new();
        let mut events = Vec::new();

        // User holds Power continuously across wake from t=100..2600ms.
        for ms in (100..=2600).step_by(20) {
            let pwr = guard.filter_pwr_pressed(ms, true);
            decoder.poll_into(ms, 3100, 3100, pwr, &mut events);
            assert!(
                events.is_empty(),
                "held wake power emitted event at {ms}ms: {events:?}"
            );
            assert!(!guard.is_armed());
        }

        // User releases Power at t=2620ms and leaves it released for 50ms -> arms at >=2660ms.
        for ms in (2620..=2670).step_by(10) {
            let pwr = guard.filter_pwr_pressed(ms, false);
            decoder.poll_into(ms, 3100, 3100, pwr, &mut events);
            assert!(events.is_empty());
        }
        assert!(guard.is_armed());

        // Inside the 120ms post-release bounce guard (< 2780ms), momentary glitch is ignored.
        let pwr_bounce = guard.filter_pwr_pressed(2700, true);
        assert!(!pwr_bounce);
        let _ = guard.filter_pwr_pressed(2720, false);

        // Fresh deliberate Power press at t=2820..2860ms cleanly emits Press(Button::Power).
        let p0 = guard.filter_pwr_pressed(2820, true);
        decoder.poll_into(2820, 3100, 3100, p0, &mut events);
        let p1 = guard.filter_pwr_pressed(2840, true);
        decoder.poll_into(2840, 3100, 3100, p1, &mut events);
        assert_eq!(events, alloc::vec![InputEvent::Press(Button::Power)]);
    }

    #[test]
    fn wake_power_guard_rejects_noisy_contact_chatter() {
        let mut guard = WakePowerGuard::new(0);

        // Toggle every 10ms during release chatter -> timer resets on each low glitch.
        for step in 0..10u64 {
            let ms = step * 10;
            let raw_pressed = step % 2 == 1;
            assert!(!guard.filter_pwr_pressed(ms, raw_pressed));
            assert!(!guard.is_armed());
        }

        // Sustained release for >= 40ms arms at ms=140, but remains within POST_BOOT_GUARD_MS (350ms).
        for ms in (100..=340).step_by(10) {
            assert!(!guard.filter_pwr_pressed(ms, false));
        }
        assert!(guard.is_armed());
        // Pressing before POST_BOOT_GUARD_MS (350ms) is still suppressed.
        assert!(!guard.filter_pwr_pressed(345, true));
        // At 360ms (>= POST_BOOT_GUARD_MS and >= 140+120), deliberate press passes through.
        assert!(guard.filter_pwr_pressed(360, true));
    }
}
