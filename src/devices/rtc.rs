//! Goldfish real-time clock (the register interface QEMU's `virt` uses).
//!
//! `TIME_LOW` latches the upper half, so a driver reading low-then-high sees
//! a consistent 64-bit nanosecond timestamp since the Unix epoch, taken from
//! the host's wall clock.

use std::time::{SystemTime, UNIX_EPOCH};

const TIME_LOW: u64 = 0x00;
const TIME_HIGH: u64 = 0x04;
const ALARM_LOW: u64 = 0x08;
const ALARM_HIGH: u64 = 0x0c;
const IRQ_ENABLED: u64 = 0x10;
const CLEAR_ALARM: u64 = 0x14;
const ALARM_STATUS: u64 = 0x18;
const CLEAR_INTERRUPT: u64 = 0x1c;

#[derive(Default)]
pub struct GoldfishRtc {
    time_high: u32,
    alarm: u64,
    alarm_armed: bool,
    alarm_fired: bool,
    irq_enabled: bool,
}

/// Nanoseconds since the Unix epoch.
pub fn wall_ns() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64)
}

impl GoldfishRtc {
    pub fn read(&mut self, off: u64) -> u64 {
        match off {
            TIME_LOW => {
                let now = wall_ns();
                self.time_high = (now >> 32) as u32;
                now & 0xffff_ffff
            }
            TIME_HIGH => self.time_high as u64,
            ALARM_LOW => self.alarm & 0xffff_ffff,
            ALARM_HIGH => self.alarm >> 32,
            IRQ_ENABLED => self.irq_enabled as u64,
            ALARM_STATUS => self.alarm_fired as u64,
            _ => 0,
        }
    }

    pub fn write(&mut self, off: u64, val: u64) {
        match off {
            ALARM_LOW => self.alarm = (self.alarm & 0xffff_ffff_0000_0000) | (val & 0xffff_ffff),
            ALARM_HIGH => {
                self.alarm = (self.alarm & 0xffff_ffff) | ((val & 0xffff_ffff) << 32);
                self.alarm_armed = true;
            }
            IRQ_ENABLED => self.irq_enabled = val & 1 != 0,
            CLEAR_ALARM => {
                self.alarm_armed = false;
                self.alarm_fired = false;
            }
            CLEAR_INTERRUPT => self.alarm_fired = false,
            _ => {}
        }
    }

    /// Fire the alarm when its time has come.
    pub fn tick(&mut self, now_ns: u64) {
        if self.alarm_armed && now_ns >= self.alarm {
            self.alarm_armed = false;
            self.alarm_fired = true;
        }
    }

    pub fn irq_asserted(&self) -> bool {
        self.irq_enabled && self.alarm_fired
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alarm_fires_once_its_time_passes() {
        let mut rtc = GoldfishRtc::default();
        rtc.write(ALARM_LOW, 1_000);
        rtc.write(ALARM_HIGH, 0);
        rtc.write(IRQ_ENABLED, 1);
        rtc.tick(999);
        assert!(!rtc.irq_asserted());
        rtc.tick(1_000);
        assert!(rtc.irq_asserted());
        assert_eq!(rtc.read(ALARM_STATUS), 1);
        rtc.write(CLEAR_INTERRUPT, 1);
        assert!(!rtc.irq_asserted());
    }

    #[test]
    fn time_latches_high_word() {
        let mut rtc = GoldfishRtc::default();
        let low = rtc.read(TIME_LOW);
        let high = rtc.read(TIME_HIGH);
        let ns = (high << 32) | low;
        assert!(ns > 1_600_000_000_000_000_000, "wall clock is after 2020");
    }
}
