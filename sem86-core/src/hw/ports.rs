use std::fmt::{Debug, Display, LowerHex, UpperHex};

use crate::hw::HwMmio;

pub trait PortIoData: Sized + Display + Debug + UpperHex + LowerHex + Copy {
    const SIZE: usize;
    const NO_DEVICE: Self;

    fn u8(&self) -> u8;
    fn u16(&self) -> u16;
    fn u32(&self) -> u32;

    fn require_u8(&self) -> Result<u8, PortError>;

    fn from_u8(f: impl FnOnce() -> u8) -> Result<Self, PortError>;
    fn from_u16(offset: impl Into<usize>, f: impl FnOnce() -> u16) -> Result<Self, PortError>;
    fn from_u32(offset: impl Into<usize>, f: impl FnOnce() -> u32) -> Result<Self, PortError>;

    fn blend_into_u32(&self, offset: impl Into<usize>, old_val: impl FnOnce() -> u32) -> u32;

    fn le_bytes(&self) -> impl AsRef<[u8]>;
    fn from_le_bytes<const N: usize>(f: impl FnOnce() -> [u8; N]) -> Result<Self, PortError>;
}

impl PortIoData for u8 {
    const SIZE: usize = 1;
    const NO_DEVICE: Self = Self::MAX;

    fn u8(&self) -> u8 {
        *self
    }
    fn u16(&self) -> u16 {
        *self as u16
    }
    fn u32(&self) -> u32 {
        *self as u32
    }

    fn require_u8(&self) -> Result<u8, PortError> {
        Ok(self.u8())
    }

    fn from_u8(f: impl FnOnce() -> u8) -> Result<Self, PortError> {
        Ok(f())
    }

    fn from_u16(offset: impl Into<usize>, f: impl FnOnce() -> u16) -> Result<Self, PortError> {
        Ok((f() >> (offset.into() * 8)) as u8)
    }

    fn from_u32(offset: impl Into<usize>, f: impl FnOnce() -> u32) -> Result<Self, PortError> {
        Ok((f() >> (offset.into() * 8)) as u8)
    }

    fn le_bytes(&self) -> impl AsRef<[u8]> {
        self.to_le_bytes()
    }

    fn from_le_bytes<const N: usize>(f: impl FnOnce() -> [u8; N]) -> Result<Self, PortError> {
        let bytes: &[u8] = &f();
        Ok(Self::from_le_bytes(bytes[..Self::SIZE].try_into().unwrap()))
    }

    fn blend_into_u32(&self, offset: impl Into<usize>, old_val: impl FnOnce() -> u32) -> u32 {
        let shift = offset.into() * 8;
        let old_val = old_val();
        let mask = 0xff << shift;

        (old_val & !mask) | ((*self as u32) << shift)
    }
}

impl PortIoData for u16 {
    const SIZE: usize = 2;
    const NO_DEVICE: Self = Self::MAX;

    fn u8(&self) -> u8 {
        *self as u8
    }
    fn u16(&self) -> u16 {
        *self
    }
    fn u32(&self) -> u32 {
        *self as u32
    }

    fn require_u8(&self) -> Result<u8, PortError> {
        Err(PortError::MustSplit)
    }

    fn from_u8(_: impl FnOnce() -> u8) -> Result<Self, PortError> {
        Err(PortError::MustSplit)
    }

    fn from_u16(_offset: impl Into<usize>, f: impl FnOnce() -> u16) -> Result<Self, PortError> {
        Ok(f())
    }

    fn from_u32(offset: impl Into<usize>, f: impl FnOnce() -> u32) -> Result<Self, PortError> {
        let offset = offset.into();
        if offset > 2 {
            Err(PortError::MustSplit)
        } else {
            Ok((f() >> (offset * 8)) as u16)
        }
    }

    fn le_bytes(&self) -> impl AsRef<[u8]> {
        self.to_le_bytes()
    }

    fn from_le_bytes<const N: usize>(f: impl FnOnce() -> [u8; N]) -> Result<Self, PortError> {
        let bytes: &[u8] = &f();
        Ok(Self::from_le_bytes(bytes[..Self::SIZE].try_into().unwrap()))
    }

    fn blend_into_u32(&self, offset: impl Into<usize>, old_val: impl FnOnce() -> u32) -> u32 {
        let shift = offset.into() * 8;
        let old_val = old_val();
        let mask = 0xffff << shift;

        (old_val & !mask) | ((*self as u32) << shift)
    }
}

impl PortIoData for u32 {
    const SIZE: usize = 4;
    const NO_DEVICE: Self = Self::MAX;

    fn u8(&self) -> u8 {
        *self as u8
    }
    fn u16(&self) -> u16 {
        *self as u16
    }
    fn u32(&self) -> u32 {
        *self
    }

    fn require_u8(&self) -> Result<u8, PortError> {
        Err(PortError::MustSplit)
    }

    fn from_u8(_: impl FnOnce() -> u8) -> Result<Self, PortError> {
        Err(PortError::MustSplit)
    }

    fn from_u16(_offset: impl Into<usize>, _: impl FnOnce() -> u16) -> Result<Self, PortError> {
        Err(PortError::MustSplit)
    }

    fn from_u32(_offset: impl Into<usize>, f: impl FnOnce() -> u32) -> Result<Self, PortError> {
        Ok(f())
    }

    fn le_bytes(&self) -> impl AsRef<[u8]> {
        self.to_le_bytes()
    }

    fn from_le_bytes<const N: usize>(f: impl FnOnce() -> [u8; N]) -> Result<Self, PortError> {
        let bytes: &[u8] = &f();
        Ok(Self::from_le_bytes(bytes[..Self::SIZE].try_into().unwrap()))
    }

    fn blend_into_u32(&self, _offset: impl Into<usize>, _old_val: impl FnOnce() -> u32) -> u32 {
        *self
    }
}

#[derive(Debug)]
pub enum PortError {
    MustSplit,
}

pub trait WithIoSpace {
    fn try_read<S: PortIoData>(&mut self, addr: u16, mmio: &mut HwMmio) -> Option<Result<S, PortError>>;
    fn try_write<S: PortIoData>(&mut self, addr: u16, val: S, mmio: &mut HwMmio) -> Option<Result<(), PortError>>;
}

#[macro_export]
macro_rules! port_write_chain {
    ($addr:expr, $val:expr, $mmio:expr => { $device:expr $(, $($rest:expr),* $(,)*)? }) => {{
        $crate::hw::ports::WithIoSpace::try_write($device, $addr, $val, $mmio)
            .or_else(|| {
                $crate::port_write_chain! {
                    $addr, $val, $mmio => {
                        $(
                            $($rest),*
                        )?
                    }
                }
            })
    }};
    ($addr:expr, $val:expr, $mmio:expr => {}) => {{
        None
    }};
}

#[macro_export]
macro_rules! port_read_chain {
    ($addr:expr, $size:ty, $mmio:expr => { $device:expr $(, $($rest:expr),* $(,)*)? }) => {{
        $crate::hw::ports::WithIoSpace::try_read::<$size>($device, $addr, $mmio)
            .or_else(|| {
                $crate::port_read_chain! {
                    $addr, $size, $mmio => {
                        $(
                            $($rest),*
                        )?
                    }
                }
            })
    }};
    ($addr:expr, $size:ty, $mmio:expr => {}) => {{
        None
    }};
}

impl<T: WithIoSpace> WithIoSpace for Option<&mut T> {
    fn try_read<S: PortIoData>(&mut self, addr: u16, mmio: &mut HwMmio) -> Option<Result<S, PortError>> {
        self.as_mut().and_then(|t| t.try_read(addr, mmio))
    }

    fn try_write<S: PortIoData>(&mut self, addr: u16, val: S, mmio: &mut HwMmio) -> Option<Result<(), PortError>> {
        self.as_mut().and_then(|t| t.try_write(addr, val, mmio))
    }
}
