use std::ops::Sub;
use std::time::Duration;

use num_traits::AsPrimitive;

pub struct Stat<T> {
    last_val: T,
    delta: f64,
    name: String,
}

impl<T> Stat<T> {
    pub fn new(name: impl Into<String>) -> Self
    where
        T: Default,
    {
        Self {
            last_val: T::default(),
            delta: 0.0,
            name: name.into(),
        }
    }

    pub fn update(&mut self, period: Duration, new_val: T) -> &mut Self
    where
        T: Sub<T> + Copy,
        <T as Sub<T>>::Output: AsPrimitive<f64>,
    {
        self.delta = (new_val - self.last_val).as_() / period.as_secs_f64();
        self.last_val = new_val;
        self
    }

    pub fn delta(&self) -> f64 {
        self.delta
    }
}

impl<T> std::fmt::Display for Stat<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:.1} {}/s", self.delta, self.name)
    }
}
