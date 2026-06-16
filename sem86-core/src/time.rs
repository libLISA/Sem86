use std::fmt::Debug;
use std::ops::{Add, AddAssign, Sub};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bitcode::{Decode, Encode};
use log::info;
use serde::{Deserialize, Serialize};

use crate::hw::intr::{IntrHandle, PendingRequest};

pub trait Timer: Debug + Send + Sync {
    fn tick(&mut self, now: EmulatorTimestamp) -> bool;
    fn next_tick(&self) -> EmulatorTimestamp;
}

#[derive(Copy, Clone, Debug, PartialOrd, Ord, PartialEq, Eq)]
pub struct EmulatorTimestamp(u64);

impl EmulatorTimestamp {
    pub fn now(clock: &EmulatorClock) -> Self {
        if clock.is_synchronous {
            Self(clock.current.load(Ordering::Relaxed))
        } else {
            Self(clock.zero.elapsed().as_nanos() as u64)
        }
    }

    fn from_duration(elapsed: Duration) -> EmulatorTimestamp {
        Self(elapsed.as_nanos() as u64)
    }
}

impl Add<Duration> for EmulatorTimestamp {
    type Output = EmulatorTimestamp;

    fn add(self, rhs: Duration) -> Self::Output {
        Self(self.0 + rhs.as_nanos() as u64)
    }
}

impl AddAssign<Duration> for EmulatorTimestamp {
    fn add_assign(&mut self, rhs: Duration) {
        self.0 += rhs.as_nanos() as u64;
    }
}

impl Sub for EmulatorTimestamp {
    type Output = Duration;

    fn sub(self, rhs: Self) -> Self::Output {
        Duration::from_nanos(self.0 - rhs.0)
    }
}

#[derive(Debug)]
pub struct EmulatorClock {
    base: u128,
    start: Instant,
    timer_sender: Sender<Box<dyn Timer>>,
    is_running: Arc<AtomicBool>,
    has_new: Arc<AtomicBool>,
    is_synchronous: bool,
    zero: Instant,

    /// Only valid when the clock is synchronous.
    ///
    /// The current number of elapsed nanoseconds.
    current: Arc<AtomicU64>,
}

#[derive(Clone, Serialize, Deserialize, Encode, Decode)]
pub struct EmulatorClockSnapshot {
    base: u128,
    is_synchronous: bool,
    current: u64,
}

struct ClockTicker {
    next_deadline: EmulatorTimestamp,
    timers: Vec<Box<dyn Timer>>,
}

impl ClockTicker {
    fn new(now: EmulatorTimestamp) -> Self {
        Self {
            next_deadline: now,
            timers: Vec::new(),
        }
    }

    fn add(&mut self, timer: Box<dyn Timer>) {
        self.timers.push(timer);
    }

    fn tick(&mut self, now: EmulatorTimestamp) -> bool {
        if self.next_deadline < now {
            let mut next_deadline = now + Duration::from_secs(1);
            self.timers.retain_mut(|timer| {
                if timer.next_tick() <= now && !timer.tick(now) {
                    return false
                }

                assert!(
                    timer.next_tick() > now,
                    "timers must update next_tick() after tick() is called"
                );
                next_deadline = next_deadline.min(timer.next_tick());
                true
            });

            self.next_deadline = next_deadline;
            true
        } else {
            false
        }
    }

    pub fn next_deadline(&self) -> EmulatorTimestamp {
        self.next_deadline
    }
}

pub struct SynchronousClock {
    ticker: ClockTicker,
    receiver: Receiver<Box<dyn Timer + 'static>>,
    current: Arc<AtomicU64>,
    has_new: Arc<AtomicBool>,
}

impl SynchronousClock {
    pub fn tick_by(&mut self, duration: Duration) {
        self.current.fetch_add(duration.as_nanos() as u64, Ordering::Relaxed);
        let now = EmulatorTimestamp(self.current.load(Ordering::Relaxed));
        self.ticker.tick(now);

        if self.has_new.fetch_and(false, Ordering::Relaxed) {
            while let Ok(timer) = self.receiver.try_recv() {
                self.ticker.add(timer);
            }
        }
    }
}

impl Debug for SynchronousClock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SynchronousClock").finish()
    }
}

impl EmulatorClock {
    pub fn new_asynchronous() -> Self {
        let is_running = Arc::new(AtomicBool::new(false));
        let is_running_copy = is_running.clone();
        let (sender, receiver) = channel();
        let zero = Instant::now();
        std::thread::Builder::new()
            .name(String::from("clock"))
            .spawn(move || {
                let mut ticker = ClockTicker::new(EmulatorTimestamp::from_duration(zero.elapsed()));
                loop {
                    if is_running_copy.load(Ordering::SeqCst) {
                        let now = EmulatorTimestamp::from_duration(zero.elapsed());
                        ticker.tick(now);

                        let next_deadline = ticker.next_deadline();
                        match receiver.recv_timeout(next_deadline - now) {
                            Ok(timer) => ticker.add(timer),
                            Err(RecvTimeoutError::Disconnected) => std::thread::sleep(next_deadline - now),
                            Err(RecvTimeoutError::Timeout) => (),
                        }
                    } else {
                        std::thread::sleep(Duration::from_millis(30));
                    }
                }
            })
            .unwrap();

        Self {
            base: 0,
            start: Instant::now(),
            timer_sender: sender,
            is_running,
            is_synchronous: false,
            has_new: Arc::new(AtomicBool::new(false)),
            zero,
            current: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn new_synchronous() -> (Self, SynchronousClock) {
        let is_running = Arc::new(AtomicBool::new(false));
        let has_new = Arc::new(AtomicBool::new(false));
        let current = Arc::new(AtomicU64::new(0));
        let (sender, receiver) = channel();

        (
            Self {
                base: 0,
                start: Instant::now(),
                timer_sender: sender,
                is_running,
                has_new: has_new.clone(),
                is_synchronous: true,
                zero: Instant::now(),
                current: current.clone(),
            },
            SynchronousClock {
                ticker: ClockTicker::new(EmulatorTimestamp(0)),
                receiver,
                has_new,
                current,
            },
        )
    }

    pub fn start(&mut self) {
        self.start = Instant::now();
        self.is_running.store(true, Ordering::SeqCst);
    }

    pub fn pause(&mut self) {
        info!("Stopping clock at {}", self.get());
        self.base = self.get();
        self.start = Instant::now();
        self.is_running.store(false, Ordering::SeqCst);

        info!("Clock is now frozen at {}", self.get());
    }

    pub fn get(&self) -> u128 {
        self.base
            + if self.is_synchronous {
                self.current.load(Ordering::Relaxed) as u128
            } else if self.is_running.load(Ordering::SeqCst) {
                self.start.elapsed().as_nanos()
            } else {
                0
            }
    }

    pub fn get_ticks_in_hz(&self, hz: u64) -> u64 {
        let ns = self.get();
        (ns * hz as u128 / 1_000_000_000) as u64
    }

    pub fn register_timer(&self, timer: Box<dyn Timer>) {
        self.timer_sender.send(timer).unwrap();
        self.has_new.store(true, Ordering::SeqCst);
    }

    pub fn snapshot(&self) -> EmulatorClockSnapshot {
        assert!(
            !self.is_running.load(Ordering::SeqCst),
            "cannot make snapshot while clock is running"
        );
        EmulatorClockSnapshot {
            base: self.base,
            is_synchronous: self.is_synchronous,
            current: self.current.load(Ordering::Relaxed),
        }
    }

    pub fn restore(&mut self, snapshot: EmulatorClockSnapshot) {
        self.base = snapshot.base;
        assert_eq!(self.is_synchronous, snapshot.is_synchronous);
        self.current.store(snapshot.current, Ordering::Relaxed);
        self.start();

        info!("Resumed clock at {}, now at {}", self.base, self.get());
    }
}

#[derive(Clone, Debug)]
pub struct PeriodicIntrTimer {
    intr: IntrHandle,
    next_tick: EmulatorTimestamp,
    info: PeriodicIntr,
}

#[derive(Clone, Debug)]
pub struct PeriodicIntr {
    pending_interrupt_request: Arc<Mutex<Option<PendingRequest>>>,
}

impl PeriodicIntr {
    pub fn clear(&self) {
        *self.pending_interrupt_request.lock().unwrap() = None;
    }

    pub fn new(intr: IntrHandle, clock: &EmulatorClock) -> Self {
        let info = Self {
            pending_interrupt_request: Arc::new(Mutex::new(None)),
        };

        clock.register_timer(Box::new(PeriodicIntrTimer {
            intr,
            next_tick: EmulatorTimestamp::now(clock),
            info: info.clone(),
        }));

        info
    }
}

impl Timer for PeriodicIntrTimer {
    fn tick(&mut self, now: EmulatorTimestamp) -> bool {
        let mut p = self.info.pending_interrupt_request.lock().unwrap();
        if p.is_none() {
            *p = Some(self.intr.request())
        }

        let period = Duration::from_secs(2);
        let next = self.next_tick + period;
        if next < now {
            self.next_tick = now + period;
        } else {
            self.next_tick = next;
        }

        true
    }

    fn next_tick(&self) -> EmulatorTimestamp {
        self.next_tick
    }
}
