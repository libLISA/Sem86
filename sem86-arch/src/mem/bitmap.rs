use std::iter::repeat_with;
use std::sync::atomic::{AtomicU64, Ordering};

use liblisa::utils::bitmap::BitmapSlice;

pub struct AtomicBitmap {
    data: Box<[AtomicU64]>,
}

impl AtomicBitmap {
    pub fn new_all_ones(len: usize) -> Self {
        assert!(len.is_multiple_of(64));
        Self {
            data: repeat_with(|| AtomicU64::new(u64::MAX)).take(len / 64).collect(),
        }
    }

    #[inline]
    fn index(x: usize) -> (usize, usize) {
        (x / 64, x % 64)
    }

    /// Sets `self[n]` to true.
    /// Returns true if `self[n]` changed; False if `self[n]` was already true.
    #[inline]
    pub fn set(&self, n: usize) -> bool {
        let (index, offset) = Self::index(n);

        let mask = 1 << offset;
        let old_value = self.data[index].fetch_or(mask, Ordering::Relaxed);

        old_value & mask == 0
    }

    /// Sets `self[n]` to false.
    /// Returns true if `self[n]` changed; False if `self[n]` was already false.
    #[inline]
    pub fn reset(&self, n: usize) -> bool {
        let (index, offset) = Self::index(n);

        let mask = 1 << offset;
        let old_value = self.data[index].fetch_and(!mask, Ordering::Relaxed);

        old_value & mask != 0
    }
}

impl BitmapSlice for AtomicBitmap {
    fn get(&self, n: usize) -> bool {
        let (index, offset) = Self::index(n);
        (self.data[index].load(Ordering::Relaxed) >> offset) & 1 != 0
    }

    fn len(&self) -> usize {
        self.data.len() * 64
    }

    fn iter_data(&self) -> impl Iterator<Item = u64> + '_ {
        self.data.iter().map(|val| val.load(Ordering::Relaxed))
    }
}
