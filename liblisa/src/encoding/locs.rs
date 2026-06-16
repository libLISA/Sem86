use std::fmt::{Debug, Display};

use bitcode::{Decode, Encode};
use itertools::Itertools;
use serde::{Deserialize, Serialize};

use super::PART_NAMES;
use super::bitpattern::{Part, PartMapping};
use crate::arch::{Arch, Register};
use crate::state::{LocationKind, Size, UnsizedLoc};
use crate::value::ValueType;

/// A destination in a dataflow.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "mem_dbg", derive(mem_dbg::MemSize))]
#[cfg_attr(
    feature = "schemars",
    schemars(bound = "A: schemars::JsonSchema, A::Reg: schemars::JsonSchema")
)]
#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Encode, Decode)]
#[serde(bound(serialize = "", deserialize = ""))]
pub enum UnsizedParLoc<A: Arch> {
    /// A specific area of a register.
    Reg(A::Reg),

    /// A specific area of a memory access.
    Mem(usize),

    /// The value of a part in an encoding.
    Part(usize),

    /// The instruction length
    InstrLen,

    /// A constant value
    Const(u64),
}

/// A destination in a dataflow.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "mem_dbg", derive(mem_dbg::MemSize))]
#[cfg_attr(
    feature = "schemars",
    schemars(bound = "A: schemars::JsonSchema, A::Reg: schemars::JsonSchema")
)]
#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Encode, Decode)]
#[serde(bound(serialize = "", deserialize = ""))]
pub struct ParLoc<A: Arch> {
    pub loc: UnsizedParLoc<A>,
    pub size: Size,
}

impl<R: Register, A: Arch<Reg = R>> PartialEq<R> for UnsizedParLoc<A> {
    fn eq(&self, other: &R) -> bool {
        match self {
            UnsizedParLoc::Reg(reg) => reg == other,
            _ => false,
        }
    }
}

impl<A: Arch> From<UnsizedLoc<A>> for UnsizedParLoc<A> {
    fn from(value: UnsizedLoc<A>) -> Self {
        match value {
            UnsizedLoc::Reg(r) => UnsizedParLoc::Reg(r),
            UnsizedLoc::Memory(n) => UnsizedParLoc::Mem(n),
        }
    }
}

impl<A: Arch> PartialEq<UnsizedLoc<A>> for UnsizedParLoc<A> {
    fn eq(&self, other: &UnsizedLoc<A>) -> bool {
        *self == Self::from(*other)
    }
}

impl<A: Arch> ParLoc<A> {
    /// The size of the destination.
    #[inline]
    pub fn size(&self) -> Size {
        self.size
    }

    /// Replaces the size of the destination with the provided `size`.
    #[inline]
    pub fn with_size(&self, size: Size) -> Self {
        Self {
            loc: self.loc,
            size,
        }
    }

    /// Returns true if the destination fully contains `other`.
    #[inline]
    pub fn contains(&self, other: &ParLoc<A>) -> bool {
        self.loc == other.loc && self.size.contains(&other.size)
    }

    /// Returns true if the destination is a flags register.
    #[inline]
    pub fn is_flags(&self) -> bool {
        match self.loc {
            UnsizedParLoc::Reg(reg) => reg.is_flags(),
            _ => false,
        }
    }

    /// Returns the mask of the destination, if it has one.
    /// The value of the destination must always be masked with the mask before setting it.
    #[inline]
    pub fn mask(&self) -> Option<u64> {
        match self.loc {
            UnsizedParLoc::Reg(reg) => reg
                .mask()
                .map(|m| (m >> (self.size.start_byte() * 8)) & (u64::MAX >> (64 - self.size.num_bytes() * 8))),
            _ => None,
        }
    }

    /// Returns the [`LocationKind`] of the destination.
    #[inline]
    pub fn kind(&self) -> LocationKind {
        match self.loc {
            UnsizedParLoc::Reg(..) => LocationKind::Reg,
            UnsizedParLoc::Mem(..) => LocationKind::Memory,
            _ => todo!(),
        }
    }

    /// Returns the [`ValueType`] of the destination.
    pub fn value_type(&self) -> ValueType {
        match self.loc {
            UnsizedParLoc::Reg(reg) => match reg.reg_type() {
                ValueType::Num => ValueType::Num,
                ValueType::Bytes(_) => ValueType::Bytes(self.size.num_bytes()),
            },
            UnsizedParLoc::Mem(_) => ValueType::Bytes(self.size.num_bytes()),
            UnsizedParLoc::InstrLen => ValueType::Num,
            UnsizedParLoc::Part(part_index) => todo!("figure out value type of part {part_index}"),
            UnsizedParLoc::Const(_) => ValueType::Num,
        }
    }

    /// Returns the [`ValueType`] of the destination, like [`Self::value_type`].
    /// Resolves [`UnsizedParLoc::Part`] according to the provided `parts`.
    pub fn value_type_ex(&self, parts: &[Part<A>]) -> ValueType {
        if let UnsizedParLoc::Part(part_index) = self.loc {
            match &parts[part_index].mapping {
                PartMapping::Imm {
                    ..
                } => ValueType::Num,
                PartMapping::MemoryComputation {
                    ..
                } => unreachable!(),
                PartMapping::Register {
                    mapping,
                } => mapping.iter().flatten().next().unwrap().reg_type(),
            }
        } else {
            self.value_type()
        }
    }

    /// Returns true if the value of this source can be changed in the CPU state.
    /// Returns false if the value of this source is a constant or a part in the instruction bitstring.
    /// Note that even though a part could be modified if it is a register, this function will return false.
    #[inline]
    pub fn can_modify(&self) -> bool {
        matches!(self.loc, UnsizedParLoc::Mem(_) | UnsizedParLoc::Reg(_))
    }
}

impl<A: Arch> Debug for UnsizedParLoc<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Reg(r) => write!(f, "Reg({r})"),
            Self::Mem(n) => write!(f, "Mem{n}"),
            Self::Part(n) => write!(f, "<{}>", PART_NAMES[*n]),
            Self::InstrLen => write!(f, "InstrLen"),
            Self::Const(val) => write!(f, "0x{val:X}"),
        }
    }
}

impl<A: Arch> Debug for ParLoc<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.loc {
            UnsizedParLoc::Reg(reg) if reg.is_flags() && self.size.num_bytes() == 1 => {
                let flags = A::flagreg_to_flags(reg, self.size.start_byte(), self.size.end_byte());
                write!(f, "Flag({})", flags.iter().map(|f| f.to_string()).join(", "))
            },
            _ => write!(f, "{:?}[{:?}]", self.loc, self.size),
        }
    }
}

impl<A: Arch> Display for ParLoc<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Debug::fmt(self, f)
    }
}
