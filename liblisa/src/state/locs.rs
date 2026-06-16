use std::fmt::{Debug, Display};

use bitcode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::arch::{Arch, Register};
use crate::encoding::UnsizedParLoc;
use crate::value::ValueType;

/// The kind of a storage location.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum LocationKind {
    /// A register.
    Reg,

    /// An accessed memory area.
    Memory,
}

/// A storage location in a CPU state.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum UnsizedLoc<A: Arch> {
    /// A register
    Reg(A::Reg),

    /// The nth memory access.
    Memory(usize),
}

impl<A: Arch> UnsizedLoc<A> {
    /// Returns the type of the location.
    pub fn kind(&self) -> LocationKind {
        match self {
            UnsizedLoc::Reg(_) => LocationKind::Reg,
            UnsizedLoc::Memory(_) => LocationKind::Memory,
        }
    }

    /// Returns true if this location has the same [`ValueType`] as `other`.
    pub fn matches_value_type_with(&self, other: &UnsizedLoc<A>) -> bool {
        match (self, other) {
            (UnsizedLoc::Reg(a), UnsizedLoc::Reg(b)) => a.reg_type() == b.reg_type(),
            (UnsizedLoc::Reg(r), UnsizedLoc::Memory(_)) => matches!(r.reg_type(), ValueType::Bytes(_)),
            (UnsizedLoc::Memory(_), UnsizedLoc::Reg(r)) => matches!(r.reg_type(), ValueType::Bytes(_)),
            (UnsizedLoc::Memory(_), UnsizedLoc::Memory(_)) => true,
        }
    }

    /// Returns true if the location is a flags register.1
    pub fn is_flags(&self) -> bool {
        if let UnsizedLoc::Reg(reg) = self {
            reg.is_flags()
        } else {
            false
        }
    }
}

impl<A: Arch> Debug for UnsizedLoc<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UnsizedLoc::Reg(reg) => write!(f, "Reg[{reg}]")?,
            UnsizedLoc::Memory(index) => write!(f, "Memory[#{index}]")?,
        }

        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct ConversionError<T> {
    source: T,
    target: &'static str,
}

impl<T: Debug> Display for ConversionError<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "could not covert {:?} into {}", self.source, self.target)
    }
}

impl<A: Arch> TryFrom<UnsizedParLoc<A>> for UnsizedLoc<A> {
    type Error = ConversionError<UnsizedParLoc<A>>;

    fn try_from(value: UnsizedParLoc<A>) -> Result<Self, Self::Error> {
        Ok(match value {
            UnsizedParLoc::Reg(reg) => UnsizedLoc::Reg(reg),
            UnsizedParLoc::Mem(mem) => UnsizedLoc::Memory(mem),
            _ => {
                return Err(ConversionError {
                    source: value,
                    target: "UnsizedLoc",
                })
            },
        })
    }
}

/// A range of bytes.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "mem_dbg", derive(mem_dbg::MemSize))]
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Encode, Decode)]
pub struct Size {
    /// The lowest index included in the range.
    start_byte: u16,

    /// The highest index included in the range.
    end_byte: u16,
}

impl Default for Size {
    fn default() -> Self {
        Size {
            start_byte: u16::MIN,
            end_byte: u16::MAX,
        }
    }
}

impl Size {
    /// Creates a new range from the provided values.
    #[inline]
    pub const fn new(start_byte: usize, end_byte: usize) -> Self {
        Size {
            start_byte: start_byte as u16,
            end_byte: end_byte as u16,
        }
    }

    /// Creates a new range containing (num_bits + 7) / 8 bytes, starting at index 0.
    #[inline]
    pub const fn from_bits(num_bits: usize) -> Self {
        Size {
            start_byte: 0,
            end_byte: ((num_bits - 1) / 8) as u16,
        }
    }

    /// Creates a new range containing `num_bytes` bytes, starting at index 0.
    #[inline]
    pub const fn from_bytes(num_bytes: usize) -> Self {
        Size {
            start_byte: 0,
            end_byte: (num_bytes - 1) as u16,
        }
    }

    /// Creates a range that only contains the byte at index `index`.
    #[inline]
    pub const fn one_byte(index: usize) -> Self {
        Self {
            start_byte: index as u16,
            end_byte: index as u16,
        }
    }

    /// Returns `Size::new(0, 7)`.
    #[inline]
    pub const fn qword() -> Self {
        Size {
            start_byte: 0,
            end_byte: 7,
        }
    }

    /// Returns a union of the two ranges.
    #[inline]
    pub fn union(&self, other: &Self) -> Self {
        Size {
            start_byte: self.start_byte.min(other.start_byte),
            end_byte: self.end_byte.max(other.end_byte),
        }
    }

    /// Returns true if the size contains `other`.
    #[inline]
    pub fn contains(&self, other: &Self) -> bool {
        self.start_byte <= other.start_byte && self.end_byte >= other.end_byte
    }

    /// Returns true if the size and `other` overlap.
    #[inline]
    pub fn overlaps(&self, other: &Self) -> bool {
        self.end_byte >= other.start_byte && self.start_byte <= other.end_byte
    }

    /// The number of bytes in the size.
    #[inline]
    pub fn num_bytes(&self) -> usize {
        (self.end_byte - self.start_byte) as usize + 1
    }

    pub fn start_byte(&self) -> usize {
        self.start_byte as usize
    }

    pub fn end_byte(&self) -> usize {
        self.end_byte as usize
    }
}

impl Debug for Size {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.end_byte < self.start_byte {
            write!(f, "-")
        } else {
            write!(f, "{}..{}", self.start_byte, self.end_byte)
        }
    }
}

#[cfg(test)]
mod test {
    use super::Size;

    #[test]
    pub fn size_contains() {
        assert!(Size::new(0, 7).contains(&Size::new(0, 5)));
        assert!(Size::new(0, 7).contains(&Size::new(0, 7)));
        assert!(Size::new(0, 7).contains(&Size::new(0, 6)));
        assert!(!Size::new(0, 7).contains(&Size::new(0, 8)));
        assert!(Size::new(0, 7).contains(&Size::new(1, 7)));
        assert!(!Size::new(1, 7).contains(&Size::new(0, 7)));
    }
}
