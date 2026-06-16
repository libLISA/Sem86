use std::hint::assert_unchecked;
use std::ops::{Add, Sub};

use serde::{Deserialize, Serialize};

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PhysAddr(u32);

impl PhysAddr {
    #[inline(always)]
    pub fn new(addr: u32) -> Self {
        Self(addr)
    }

    #[inline(always)]
    pub fn frame_offset(&self) -> u16 {
        self.0 as u16 & 0xfff
    }

    #[inline(always)]
    pub fn as_u32(&self) -> u32 {
        self.0
    }
}

impl std::fmt::Display for PhysAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "0x{:X}", self.0)
    }
}

const PHYS_FRAME_MAX: u32 = 0x000f_ffff;
type PhysFrameIndexValue = std::pat::pattern_type!(u32 is 0..PHYS_FRAME_MAX);

#[derive(Copy, Clone)]
pub struct PhysFrameIndex(PhysFrameIndexValue);

impl Default for PhysFrameIndex {
    fn default() -> Self {
        Self::new(0)
    }
}

impl std::fmt::Debug for PhysFrameIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.value(), f)
    }
}

impl PartialEq for PhysFrameIndex {
    fn eq(&self, other: &Self) -> bool {
        self.value() == other.value()
    }
}

impl Eq for PhysFrameIndex {}

impl std::hash::Hash for PhysFrameIndex {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.value().hash(state)
    }
}

impl std::fmt::Display for PhysFrameIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "0x{:08X}___", self.value())
    }
}

impl Add<u32> for PhysAddr {
    type Output = PhysAddr;

    fn add(self, rhs: u32) -> Self::Output {
        Self(self.0.wrapping_add(rhs))
    }
}

impl From<PhysAddr> for PhysFrameIndex {
    #[inline(always)]
    fn from(value: PhysAddr) -> Self {
        unsafe {
            // SAFETY: Shift by 12 leaves upper 12 bits zero
            Self::new_unchecked(value.0 >> 12)
        }
    }
}

impl PhysFrameIndex {
    #[inline(always)]
    pub fn new(index: u32) -> Self {
        assert!(index <= PHYS_FRAME_MAX);
        // SAFETY: Assert ensures index is within valid range.
        unsafe { Self::new_unchecked(index) }
    }

    unsafe fn new_unchecked(index: u32) -> PhysFrameIndex {
        Self(unsafe { std::mem::transmute(index) })
    }

    #[inline(always)]
    pub fn start_address(&self) -> PhysAddr {
        PhysAddr(self.value() << 12)
    }

    #[inline(always)]
    pub fn index(&self) -> usize {
        self.value() as usize
    }

    #[inline(always)]
    fn value(&self) -> u32 {
        let val: u32 = unsafe { std::mem::transmute(self.0) };
        unsafe {
            assert_unchecked(val as u32 <= PHYS_FRAME_MAX);
        }
        val
    }

    #[inline(always)]
    pub fn with_offset(&self, frame_offset: u16) -> PhysAddr {
        PhysAddr::new(self.start_address().as_u32() + (frame_offset as u32 & 0xfff))
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct LinAddr(u32);

impl LinAddr {
    pub const fn new(addr: u32) -> Self {
        Self(addr)
    }

    pub fn page_offset(&self) -> u16 {
        self.0 as u16 & 0xfff
    }

    #[inline(always)]
    pub fn as_u32(&self) -> u32 {
        self.0
    }
}

impl Add<u32> for LinAddr {
    type Output = LinAddr;

    fn add(self, rhs: u32) -> Self::Output {
        Self(self.0.wrapping_add(rhs))
    }
}

impl std::fmt::Display for LinAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "0x{:X}", self.0)
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LinPageIndex(u32);

impl std::fmt::Display for LinPageIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "0x{:X}___", self.0)
    }
}

impl From<LinAddr> for LinPageIndex {
    fn from(value: LinAddr) -> Self {
        Self(value.0 >> 12)
    }
}

impl Sub<u32> for LinPageIndex {
    type Output = LinPageIndex;

    fn sub(self, rhs: u32) -> Self::Output {
        Self(self.0.wrapping_sub(rhs) & (u32::MAX >> 12))
    }
}

impl Add<u32> for LinPageIndex {
    type Output = LinPageIndex;

    fn add(self, rhs: u32) -> Self::Output {
        Self(self.0.wrapping_add(rhs) & (u32::MAX >> 12))
    }
}

impl LinPageIndex {
    #[inline(always)]
    pub fn index(&self) -> usize {
        self.0 as usize
    }

    #[inline(always)]
    pub fn start_addr(&self) -> LinAddr {
        LinAddr(self.0 << 12)
    }
}
