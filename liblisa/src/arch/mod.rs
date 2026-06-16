//! All traits needed to define an architecture.
//!
//! In order to implement an architecture, you should define a struct that implements the [`Arch`] trait.
//! Additionally, you will need to define types that implement [`Register`] and [`Flag`], and reference these from the [`Arch`] trait.
//!
//! An example of a minimal implementation can be found in the [`fake`] module.
//! It implements a fake architecture that is used in some tests.
//!
//! In addition, you can inspect the source code of the various existing architecture implementation crates, such as `liblisa-x64`.

use std::fmt::{Debug, Display};
use std::hash::Hash;

use bitcode::{DecodeOwned, Encode};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::encoding::dataflows::AddrSize;
use crate::value::{MutValue, Value, ValueType};

/// Represents a CPU architecture.
pub trait Arch: Copy + Clone + Debug + PartialEq + Eq + Hash + Default + PartialOrd + Ord + Send + Sync + 'static
where
    Self: Sized,
{
    /// The CPU state representation.
    type CpuState: CpuState<Self> + Clone + PartialEq + Eq + Send + Sync + Debug + Display;

    /// The register representation.
    type Reg: Register
        + Copy
        + Clone
        + Debug
        + Display
        + Eq
        + Hash
        + PartialOrd
        + Ord
        + Serialize
        + DeserializeOwned
        + Encode
        + DecodeOwned
        + Send
        + Sync;

    /// The general-purpose register representation.
    /// This should be equal to [`Self::Reg`], or be a subset.
    /// General-purpose registers must be integers (see [`crate::value::ValueType`]).
    ///
    /// These are the only registers that are used in address computations.
    type GpReg: Register
        + NumberedRegister
        + Clone
        + Debug
        + Display
        + Eq
        + Hash
        + PartialOrd
        + Ord
        + Serialize
        + DeserializeOwned
        + Encode
        + DecodeOwned
        + Send
        + Sync;

    /// The flag representation.
    ///
    /// A flag should always be part of a flag register.
    ///
    /// See also [`Arch::flagreg_to_flags`].
    type Flag: Flag
        + Clone
        + Debug
        + Display
        + PartialEq
        + Eq
        + Hash
        + PartialOrd
        + Ord
        + Serialize
        + DeserializeOwned
        + Encode
        + DecodeOwned
        + Send
        + Sync;

    /// The number of bits that are used in a page.
    /// The page size is `2**PAGE_BITS`.
    /// For example, for a page size of 4096 bytes `PAGE_BITS` would be `12`.
    const PAGE_BITS: usize;

    /// The program counter register.
    const PC: Self::GpReg;

    /// The zero register.
    /// If the architecture does not explicitly list a zero register, you can invent one.
    const ZERO: Self::GpReg;

    /// The alignment of the instructions. Must be a multiple of 2.
    const INSTRUCTION_ALIGNMENT: usize = 1;

    const ADDR_SIZE: AddrSize = AddrSize::Addr64;

    /// Converts a general-purpose register into a generic register.
    /// This must always succeed.
    fn reg(reg: Self::GpReg) -> Self::Reg;

    /// Converts a generic register into a general-purpose register.
    /// If the generic register is not a general-purpose register, `None` is returned.
    fn try_reg_to_gpreg(reg: Self::Reg) -> Option<Self::GpReg>;

    /// Returns the flags associated with the byte range in the flags register.
    /// By convention, a flag register should contain one flag per byte.
    fn flagreg_to_flags(reg: Self::Reg, start_byte: usize, end_byte: usize) -> &'static [Self::Flag];

    /// Returns an iterator that iterates over all general-purpose registers.
    /// The zero register must not be included, as it is not a real register.
    fn iter_gpregs() -> impl Iterator<Item = Self::GpReg>;

    /// Returns an iterator that iterates over all registers.
    /// The zero register must not be included, as it is not a real register.
    fn iter_regs() -> impl Iterator<Item = Self::Reg>;
}

/// Represents a CPU state.
pub trait CpuState<A: Arch>: Default {
    /// The type of the difference mask used in [`CpuState::find_dataflows_masked`].
    ///
    /// An optional optimization. Set to `()` if not used.
    type DiffMask: Clone + Default + Debug;

    /// Returns the value of `reg`.
    fn gpreg(&self, reg: A::GpReg) -> u64;

    /// Sets the value of `reg`.
    fn set_gpreg(&mut self, reg: A::GpReg, value: u64);

    /// Returns the value of `reg`.
    fn reg(&self, reg: A::Reg) -> Value<'_>;

    /// Modifies the value of `reg` using update function `update`.
    ///
    /// The update function receives a [`crate::value::MutValue`], which can be used to update the value of the register.
    fn modify_reg<F: FnOnce(MutValue)>(&mut self, reg: A::Reg, update: F);

    /// Returns the value of `flag`.
    fn flag(&self, flag: A::Flag) -> bool;

    /// Sets the value of `flag`.
    fn set_flag(&mut self, flag: A::Flag, value: bool);

    /// Creates a new CPU state using `regval` to determine the values of the registers.
    ///
    /// Implementation is optional.
    /// The default implementation calls [`CpuState::modify_reg`] on each register to initialize them.
    #[inline(always)]
    fn create<R: FnMut(A::Reg, MutValue)>(mut regval: R) -> Self {
        let mut state = Self::default();
        for reg in A::iter_regs() {
            CpuState::modify_reg(&mut state, reg, |val| regval(reg, val));
        }

        state
    }
}

/// Represents a register.
pub trait Register: Copy + Sized + PartialOrd + Ord + PartialEq {
    /// Returns whether this register is the program counter.
    fn is_pc(&self) -> bool;

    /// Returns whether this register is the zero register.
    fn is_zero(&self) -> bool;

    /// Returns whether this register is a flags register.
    fn is_flags(&self) -> bool;

    /// Indicates which bits may be set. Any bit '1' in the mask may be set, any bit '0' MUST always be set to '0'.
    /// Returns `None` when the register is a [`crate::value::ValueType::Bytes`].
    /// Returns `None` when all bits may be set (this is equivalent to returning `Some(0xffffffff_ffffffff)`)
    fn mask(&self) -> Option<u64>;

    /// Returns true if the register should always contain a valid memory address.
    fn is_addr_reg(&self) -> bool;

    /// Returns the number of bytes the register uses.
    /// It must be possible to modify at least one bit in each byte.
    fn byte_size(&self) -> usize;

    /// Returns the value type of the register.
    fn reg_type(self) -> ValueType;
}

/// Implements conversion to and from `usize`.
/// This is required for general purpose registers, and is used as an optimization in some code.
pub trait NumberedRegister {
    /// Converts the register to a `usize`.
    /// Inverse of [`NumberedRegister::from_num`].
    fn as_num(&self) -> usize;

    /// Converts the `usize` to a register.
    /// Inverse of [`NumberedRegister::as_num`].
    ///
    /// # Panics
    /// This function will panic if the `usize` does not refer to a valid register.
    fn from_num(num: usize) -> Self;
}

/// Represents a flag.
pub trait Flag: Copy + Sized + PartialOrd + Ord {
    /// Returns an iterator that iterates over all flags.
    fn iter() -> impl Iterator<Item = Self>;
}
