use std::time::Duration;

#[cfg(feature = "time")]
pub struct Stopwatch(std::time::Instant);

#[cfg(not(feature = "time"))]
pub struct Stopwatch();

#[cfg(feature = "time")]
impl Stopwatch {
    pub fn now() -> Self {
        Self(std::time::Instant::now())
    }

    pub fn elapsed(&self) -> Duration {
        self.0.elapsed()
    }
}

#[cfg(not(feature = "time"))]
impl Stopwatch {
    pub fn now() -> Self {
        Self()
    }

    pub fn elapsed(&self) -> Duration {
        Duration::ZERO
    }
}
