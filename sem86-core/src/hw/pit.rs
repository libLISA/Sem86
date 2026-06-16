//! Implementation of an Intel 8252 programmable interval timer (PIT).
//!
//! Implemented according to the datasheet from Intel from September 1993.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bitcode::{Decode, Encode};
use log::{debug, info, warn};
use serde::{Deserialize, Serialize};

use crate::hw::pic::DualIrqLine;
use crate::time::{EmulatorClock, EmulatorTimestamp};

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
enum Mode {
    Mode0,
    Mode1,
    Mode2,
    Mode3,
    Mode4,
    Mode5,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
enum Encoding {
    Binary,
    Bcd,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
enum Byte {
    Lsb,
    Msb,
}

impl Byte {
    pub fn extract(&self, val: u16) -> u8 {
        match self {
            Byte::Lsb => val as u8,
            Byte::Msb => (val >> 8) as u8,
        }
    }

    pub fn set(&self, cur: u16, new: u8) -> u16 {
        match self {
            Byte::Lsb => (cur & !0x00ff) | new as u16,
            Byte::Msb => (cur & !0xff00) | ((new as u16) << 8),
        }
    }

    fn flip(&self) -> Byte {
        match self {
            Byte::Lsb => Byte::Msb,
            Byte::Msb => Byte::Lsb,
        }
    }

    fn wraps_when_flipped(&self) -> bool {
        *self == Byte::Msb
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
enum State {
    /// Write only the specified byte.
    Single(Byte),

    /// Write LSB, then MSB. Repeat.
    Both { next_read: Byte, next_write: Byte },
}
impl State {
    fn waiting_on_second_byte_write(&self) -> bool {
        matches!(
            self,
            State::Both {
                next_write: Byte::Msb,
                ..
            }
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
struct Timer {
    init: u16,
    val: u16,
    mode: Mode,
    encoding: Encoding,
    state: State,
    output: bool,
    null_count: bool,
    latched_value: Option<u16>,
    latched_status: Option<u8>,
    gate: bool,
    last_tick: u64,
}

impl Timer {
    pub fn new() -> Self {
        Self {
            init: 0,
            val: 0,
            mode: Mode::Mode0,
            encoding: Encoding::Binary,
            state: State::Single(Byte::Lsb),
            output: false,
            latched_value: None,
            latched_status: None,
            null_count: false,
            gate: true,
            last_tick: 0,
        }
    }

    pub fn read(&mut self) -> u8 {
        if let Some(status) = self.latched_status.take() {
            status
        } else {
            use State::*;
            let val = self.latched_value.unwrap_or(self.val);
            let (new_state, release_latch, result) = match self.state {
                Single(byte) => (Single(byte), true, byte.extract(val)),
                Both {
                    next_read,
                    next_write,
                } => (
                    Both {
                        next_read: next_read.flip(),
                        next_write,
                    },
                    next_read.wraps_when_flipped(),
                    next_read.extract(val),
                ),
            };

            if release_latch {
                self.latched_value = None
            }

            self.state = new_state;

            result
        }
    }

    pub fn write(&mut self, val: u8) {
        use State::*;
        let (new_state, done) = match self.state {
            Single(byte) => {
                self.init = byte.set(self.init, val);
                (Single(byte), true)
            },
            Both {
                next_read,
                next_write,
            } => {
                self.init = next_write.set(self.init, val);
                (
                    Both {
                        next_write: next_write.flip(),
                        next_read,
                    },
                    next_write.wraps_when_flipped(),
                )
            },
        };

        self.state = new_state;
        self.null_count |= done;
    }

    pub fn write_control(&mut self, val: u8) {
        *self = Self {
            init: 0,
            state: match (val >> 4) & 0b11 {
                0 => return self.latch_count(),
                1 => State::Single(Byte::Lsb),
                2 => State::Single(Byte::Msb),
                3 => State::Both {
                    next_read: Byte::Lsb,
                    next_write: Byte::Lsb,
                },
                _ => unreachable!(),
            },
            encoding: match val & 1 {
                0 => Encoding::Binary,
                1 => Encoding::Bcd,
                _ => unreachable!(),
            },
            mode: match (val >> 1) & 0b111 {
                0 => Mode::Mode0,
                1 => Mode::Mode1,
                2 | 6 => Mode::Mode2,
                3 | 7 => Mode::Mode3,
                4 => Mode::Mode4,
                5 => Mode::Mode5,
                _ => unreachable!(),
            },
            latched_status: None,
            latched_value: None,
            output: !matches!(self.mode, Mode::Mode0),
            val: self.val,
            null_count: true,
            gate: self.gate,
            last_tick: self.last_tick,
        }
    }

    fn latch_count(&mut self) {
        self.latched_value = self.latched_value.or(Some(self.val))
    }

    fn latch_status(&mut self) {
        let encoding = match self.encoding {
            Encoding::Binary => 0,
            Encoding::Bcd => 1,
        };

        let mode = match self.mode {
            Mode::Mode0 => 0,
            Mode::Mode1 => 1,
            Mode::Mode2 => 2,
            Mode::Mode3 => 3,
            Mode::Mode4 => 4,
            Mode::Mode5 => 5,
        };

        let rw = match self.state {
            State::Single(Byte::Lsb) => 0b01,
            State::Single(Byte::Msb) => 0b10,
            State::Both {
                ..
            } => 0b11,
        };

        let null_count = self.null_count as u8; // TODO
        let output = self.output as u8;

        self.latched_status = Some((output << 7) | (null_count << 6) | (rw << 4) | (mode << 1) | encoding);
    }

    fn tick_to(&mut self, tick: u64) {
        let Some(needed_ticks) = tick.checked_sub(self.last_tick) else {
            panic!(
                "ticks should montonically increase: last tick was {}, current tick is {tick}",
                self.last_tick
            )
        };

        if needed_ticks > 0 {
            self.last_tick = tick;
            self.tick(needed_ticks, None);
        }
    }

    fn tick(&mut self, num: u64, irq: Option<&DualIrqLine>) {
        assert!(num != 0);
        if self.null_count {
            self.val = self.init;
            self.null_count = false;
        }

        let init = if self.init == 0 { 0x1_0000 } else { self.init as u64 };

        // TODO: BCD
        // TODO: The counter should not stop at 0 but instead wrap around and continue counting (or reload for modes 2 and 3)

        let last_output = self.output;
        let mut saw_rising_edge = false;
        match self.mode {
            Mode::Mode0 => {
                if self.gate && !self.state.waiting_on_second_byte_write() {
                    saw_rising_edge = (self.val as u64) < num;
                    self.val = (self.val as u64).wrapping_sub(num) as u16;
                    self.output = self.val == 0;
                }
            },
            Mode::Mode1 => {
                // TODO: Use trigger (rising edge of gate)
                todo!()
            },
            Mode::Mode2 => {
                if self.gate {
                    self.val = (self.val as u64).checked_sub(num).unwrap_or_else(|| {
                        saw_rising_edge = self.val >= 1;
                        let remaining = num - self.val as u64;
                        init * ((remaining - 1) / init + 1) - remaining
                    }) as u16;

                    // TODO: self.val might not be 0 anymore...
                    saw_rising_edge |= self.val == 0 && num >= 1;

                    // TODO: If output has gone low but already went high again we might still need to trigger an interrupt.
                    // TODO: What does trigger do?
                    self.output = self.val != 1;
                }
            },
            Mode::Mode3 => {
                if self.gate {
                    // TODO: Trigger reloads val
                    // TODO: Double-decrement if init was an odd value
                    self.val = (self.val as u64).checked_sub(num).unwrap_or_else(|| {
                        let remaining = num - self.val as u64;
                        init * ((remaining - 1) / init + 1) - remaining
                    }) as u16;
                    self.output = self.val >= (init >> 1) as u16;
                }
            },
            Mode::Mode4 => {
                if self.gate {
                    let old_val = self.val;
                    saw_rising_edge = (self.val as u64) <= num && num >= 1;
                    self.val = (self.val as u64).wrapping_sub(num) as u16;
                    self.output = self.val == 0 && old_val != self.val;
                }
            },
            Mode::Mode5 => {
                if self.gate {
                    // TODO: Trigger should reload counter value
                    let old_val = self.val;
                    saw_rising_edge = (self.val as u64) <= num && num >= 1;
                    self.val = (self.val as u64).wrapping_sub(num) as u16;
                    self.output = self.val == 0 && old_val != self.val;
                }
            },
        }

        // TODO: Is rising edge to trigger PIC correct?
        if num == 1 {
            if self.output
                && last_output != self.output
                && let Some(irq) = &irq
            {
                irq.pulse();
            }
        } else if saw_rising_edge && let Some(irq) = &irq {
            irq.pulse();
        }
    }

    fn compute_effective_irq_period(&self) -> Duration {
        assert!(matches!(self.mode, Mode::Mode2 | Mode::Mode3), "{self:?}");
        let init = if self.init == 0 { 0x1_0000 } else { self.init as u32 };

        (Duration::from_secs(1) * init) / TIMER_CLOCK_RATE as u32
    }
}

#[derive(Debug)]
pub struct Pit {
    _status_val: u8,
    timers: [Timer; 3],
    irq: DualIrqLine,
    timer_info: TimerInfo,
}

#[derive(Clone, Debug, Serialize, Deserialize, Encode, Decode)]
pub struct PitSnapshot {
    _status_val: u8,
    timers: [Timer; 3],
}

const TIMER_CLOCK_RATE: u64 = 1_193_182;

impl Pit {
    pub fn new(irq: DualIrqLine, time: &EmulatorClock) -> Self {
        // - Timer 0 is connected to PIC, output cannot be read (directly), gate cannot be controlled.
        // - Timer 1 used to be for DRAM refresh but is unused (or unimplemented) in modern PCs, output cannot be read, gate cannot be controlled.
        // - Timer 2 is connected to the PC speaker, gate can be controlled via bit 0 of port 0x61, output can be read via bit 5 of port 0x61.
        let timer_info = TimerInfo {
            enabled: Arc::new(AtomicBool::new(true)),
            period: Duration::from_millis(100),
        };

        time.register_timer(Box::new(PitTimer {
            info: timer_info.clone(),
            next_interrupt: EmulatorTimestamp::now(time),
            irq: irq.clone(),
        }));

        Self {
            _status_val: 0,
            timers: [Timer::new(), Timer::new(), Timer::new()],
            irq,
            timer_info,
        }
    }

    pub fn read_timer(&mut self, reg: u8, time: &EmulatorClock) -> u8 {
        let timer = &mut self.timers[reg as usize];
        let num = time.get_ticks_in_hz(TIMER_CLOCK_RATE);
        timer.tick_to(num);
        timer.read()
    }

    pub fn write_timer(&mut self, reg: u8, val: u8, time: &EmulatorClock) {
        let timer = &mut self.timers[reg as usize];
        let num = time.get_ticks_in_hz(TIMER_CLOCK_RATE);
        timer.tick_to(num);
        timer.write(val);

        if reg == 0 {
            info!("Wrote timer0: {timer:?}");
            self.update_timer_registration(time);
        }
    }

    fn update_timer_registration(&mut self, time: &EmulatorClock) {
        let timer = &mut self.timers[0];
        let irq_period = timer.compute_effective_irq_period();
        if self.timer_info.period != irq_period {
            info!("Restarting timer 0 IRQ timer with period of {irq_period:?}");
            // TODO: Synchronize IRQ with timer.count reaching 1.
            self.timer_info = TimerInfo {
                enabled: Arc::new(AtomicBool::new(true)),
                period: irq_period,
            };

            time.register_timer(Box::new(PitTimer {
                info: self.timer_info.clone(),
                next_interrupt: EmulatorTimestamp::now(time) + irq_period,
                irq: self.irq.clone(),
            }));
        }
    }

    pub fn read_control(&mut self) -> u8 {
        // should be a "No-Operation (3-State)" according to the datasheet
        0
    }

    pub fn write_control(&mut self, val: u8, time: &EmulatorClock) {
        let target = val >> 6;
        if target == 0b11 {
            let latch_count = val & 0b10_0000 != 0;
            let latch_status = val & 0b01_0000 != 0;
            for (n, timer) in self.timers.iter_mut().enumerate() {
                let num = time.get_ticks_in_hz(TIMER_CLOCK_RATE);
                timer.tick_to(num);

                if (val >> (1 + n)) & 1 != 0 {
                    if latch_count {
                        timer.latch_count()
                    }

                    if latch_status {
                        timer.latch_status()
                    }
                }
            }
        } else {
            let timer = &mut self.timers[target as usize];
            let num = time.get_ticks_in_hz(TIMER_CLOCK_RATE);
            timer.tick_to(num);
            timer.write_control(val & 0x3f);
            debug!("Configured timer #{target}: {timer:X?}");
        }
    }

    pub fn snapshot(&self) -> PitSnapshot {
        PitSnapshot {
            timers: self.timers.clone(),
            _status_val: self._status_val,
        }
    }

    pub fn restore(&mut self, pit: PitSnapshot, clock: &EmulatorClock) {
        self.timers = pit.timers;
        self._status_val = pit._status_val;
        self.update_timer_registration(clock);
    }
}

#[derive(Clone, Debug)]
struct TimerInfo {
    period: Duration,
    enabled: Arc<AtomicBool>,
}

impl Drop for TimerInfo {
    fn drop(&mut self) {
        self.enabled.store(false, Ordering::Relaxed)
    }
}

#[derive(Debug)]
struct PitTimer {
    info: TimerInfo,
    next_interrupt: EmulatorTimestamp,
    irq: DualIrqLine,
}

impl crate::time::Timer for PitTimer {
    fn tick(&mut self, now: EmulatorTimestamp) -> bool {
        let enabled = self.info.enabled.load(Ordering::Relaxed);
        if enabled {
            self.irq.pulse();

            let next = self.next_interrupt + self.info.period;
            if next < now {
                warn!("PIT timer skipped a tick");
                self.next_interrupt = now + self.info.period;
            } else {
                self.next_interrupt = next;
            }
        }

        enabled
    }

    fn next_tick(&self) -> EmulatorTimestamp {
        self.next_interrupt
    }
}

#[cfg(test)]
mod test {
    use crate::hw::pit::{Byte, Encoding, Mode, State, Timer};

    #[test]
    fn timer_multiple_ticks_is_equal_to_single_tick() {
        for init in [0, 100, 20_000, 20_001, 20_002, 20_003, 20_004, u16::MAX] {
            // TODO: Mode::Mode1
            for mode in [Mode::Mode0, Mode::Mode2, Mode::Mode3, Mode::Mode4, Mode::Mode5] {
                for encoding in [Encoding::Binary, Encoding::Bcd] {
                    for gate in [true, false] {
                        let mut timer = Timer {
                            init,
                            val: 0,
                            mode,
                            encoding,
                            state: State::Single(Byte::Lsb),
                            output: false,
                            null_count: true,
                            latched_value: None,
                            latched_status: None,
                            gate,
                            last_tick: 0,
                        };

                        println!("Testing {timer:X?}");

                        for _ in 0..1000 {
                            let step_size = rand::random_range(2..10_000);
                            let mut t1 = timer.clone();
                            let mut t2 = timer.clone();

                            for _ in 0..step_size {
                                t1.tick(1, None);
                            }

                            t2.tick(step_size, None);

                            assert_eq!(t1, t2, "Timers should be equal after {step_size} steps from {timer:#X?}");

                            timer = t2;
                        }
                    }
                }
            }
        }
    }
}
