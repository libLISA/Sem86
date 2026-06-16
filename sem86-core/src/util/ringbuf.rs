use std::fmt::Debug;

pub struct FixedRingbuf<const N: usize, T> {
    buf: [T; N],
    pos: usize,
}

impl<const N: usize, T> Default for FixedRingbuf<N, T>
where
    T: Copy + Default,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize, T> FixedRingbuf<N, T> {
    pub fn new() -> Self
    where
        T: Copy + Default,
    {
        Self {
            buf: [T::default(); N],
            pos: 0,
        }
    }

    pub fn new_with(mut f: impl FnMut() -> T) -> Self {
        Self {
            buf: [(); N].map(|_| f()),
            pos: 0,
        }
    }

    pub fn push_with(&mut self, item: impl FnOnce() -> T) {
        self.pos = (self.pos + 1) % N;
        self.buf[self.pos] = item();
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> {
        let (lhs, rhs) = self.buf.split_at((self.pos + 1) % N);
        rhs.iter().chain(lhs)
    }

    pub fn last(&self) -> &T {
        &self.buf[self.pos]
    }
}

impl<const N: usize, T: Debug> Debug for FixedRingbuf<N, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}
