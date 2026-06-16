use std::sync::Arc;

use cpal::{FromSample, SizedSample};
pub mod device;

pub trait Frontend: Send + Sync {
    fn fill_buffer<T: SizedSample + FromSample<f32>>(&self, buf: &mut [T], channels: usize);
}

impl<F: Frontend> Frontend for Arc<F> {
    fn fill_buffer<T: SizedSample + FromSample<f32>>(&self, buf: &mut [T], channels: usize) {
        F::fill_buffer(self, buf, channels)
    }
}
