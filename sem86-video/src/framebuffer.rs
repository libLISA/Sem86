use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

#[derive(Clone)]
pub struct Framebuffer {
    width: u32,
    data: Arc<Vec<AtomicU32>>,
}

impl Framebuffer {
    pub fn new(width: u32, height: u32) -> Self {
        let mut v = Vec::new();
        for _ in 0..width * height {
            v.push(AtomicU32::new(0));
        }

        Self {
            data: Arc::new(v),
            width,
        }
    }

    pub fn write_pixel(&self, x: u32, y: u32, data: u32) {
        self.data[(x + y * self.width) as usize].store(data, Ordering::Relaxed);
    }

    pub fn read_pixel(&self, x: u32, y: u32) -> u32 {
        self.data[(x + y * self.width) as usize].load(Ordering::Relaxed)
    }

    pub fn copy_to_slice(&self, slice: &mut [u8]) {
        assert_eq!(self.data.len() * 4, slice.len());
        for (src, dst) in self.data.iter().zip(slice.chunks_mut(4)) {
            dst.copy_from_slice(&src.load(Ordering::Relaxed).to_le_bytes());
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn fill(&self, col: u32) {
        for p in self.data.iter() {
            p.store(col, Ordering::Relaxed);
        }
    }
}
