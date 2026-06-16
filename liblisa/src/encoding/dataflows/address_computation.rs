//! Address computations.

use std::cmp::Ordering;
use std::fmt::{Debug, Display};

use arrayvec::ArrayVec;
use bitcode::{Decode, Encode};
use itertools::Itertools;
use serde::{Deserialize, Serialize};

use crate::arch::{Arch, CpuState};
use crate::encoding::UnsizedParLoc;
use crate::encoding::dataflows::Inputs;
use crate::semantics::ARG_NAMES;
use crate::state::{Addr, SystemState};
use crate::value::Value;

/// An address computation of a memory access.
///
/// The address computation is a sum of shift-then-multiplied values.
/// For example: `A + (B >> 1) * 4 + offset`
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode, PartialOrd, Ord, Hash)]
pub struct AddressComputation {
    /// The offsets that is added to the computation.
    pub offset: i64,

    /// The terms in the computation.
    /// There are always five terms, but some terms may be effectively 0.
    pub terms: ArrayVec<AddrTerm, 8>,

    /// The size of the address (e.g., 32-bit or 64-bit)
    pub addr_size: AddrSize,
}

#[cfg(feature = "mem_dbg")]
impl mem_dbg::MemSize for AddressComputation {
    fn mem_size(&self, _flags: mem_dbg::SizeFlags) -> usize {
        size_of::<Self>()
    }
}

#[cfg(feature = "mem_dbg")]
impl mem_dbg::CopyType for AddressComputation {
    type Copy = mem_dbg::True;
}

impl Debug for AddressComputation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.display(ARG_NAMES))
    }
}

/// The address size of memory operations
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(
    Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Encode, Decode, PartialOrd, Ord, Hash, arbitrary::Arbitrary,
)]
pub enum AddrSize {
    Addr16,
    Addr20,
    Addr32,
    Addr64,
}

impl AddrSize {
    #[inline(always)]
    pub fn bitmask(&self) -> u64 {
        match self {
            AddrSize::Addr16 => 0xffff,
            AddrSize::Addr20 => 0xf_ffff,
            AddrSize::Addr32 => 0xffff_ffff,
            AddrSize::Addr64 => u64::MAX,
        }
    }

    #[inline(always)]
    pub fn apply(&self, addr: u64) -> u64 {
        addr & self.bitmask()
    }

    #[inline(always)]
    pub fn try_apply(&self, addr: u64) -> Option<u64> {
        if addr <= self.bitmask() { Some(addr) } else { None }
    }

    pub fn delta(&self, new_addr: u64, base_addr: u64) -> u64 {
        match self {
            AddrSize::Addr16 => (new_addr as u16).wrapping_sub(base_addr as u16) as i16 as i64 as u64,
            AddrSize::Addr20 => ((((new_addr as u32).wrapping_sub(base_addr as u32) as i32) << 12) >> 12) as i64 as u64,
            AddrSize::Addr32 => (new_addr as u32).wrapping_sub(base_addr as u32) as i32 as i64 as u64,
            AddrSize::Addr64 => new_addr.wrapping_sub(base_addr),
        }
    }

    pub fn num_bits(&self) -> usize {
        match self {
            AddrSize::Addr16 => 16,
            AddrSize::Addr20 => 20,
            AddrSize::Addr32 => 32,
            AddrSize::Addr64 => 64,
        }
    }

    pub fn fits(&self, addr: Addr) -> bool {
        let mask = match self {
            AddrSize::Addr16 => !0xffff,
            AddrSize::Addr20 => !0xf_ffff,
            AddrSize::Addr32 => !0xffff_ffff,
            AddrSize::Addr64 => 0,
        };

        addr.as_u64() & mask == 0
    }

    pub fn iter() -> impl DoubleEndedIterator<Item = AddrSize> {
        [AddrSize::Addr16, AddrSize::Addr20, AddrSize::Addr32, AddrSize::Addr64].into_iter()
    }
}

impl<'a> arbitrary::Arbitrary<'a> for AddressComputation {
    fn arbitrary(_u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        unimplemented!()
    }
}

/// The values for the shift-then-multiply operation.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "mem_dbg", derive(mem_dbg::MemSize))]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Encode, Decode, PartialOrd, Ord, Hash)]
pub struct AddrTermShift {
    right: u8,
    mult: u16,
}

impl AddrTermShift {
    pub fn new(right: u8, mult: u16) -> Self {
        assert!(mult != 0, "mult cannot be 0");
        Self {
            right,
            mult,
        }
    }

    /// Applies the shift-then-multiply operation to `val`.
    #[inline]
    pub fn apply(self, val: u64) -> u64 {
        ((val as i64 >> self.right).wrapping_mul(self.mult as i64)) as u64
    }

    /// Returns the number of bits by which this operation shifts right.
    pub fn right(&self) -> u8 {
        self.right
    }

    /// Returns the value
    pub fn mult(&self) -> u16 {
        self.mult
    }
}

/// The size of a term in the address computation.
/// Also specifies whether the term should be interpreted as signed or unsigned after cropping it to the right size.
///
/// The variants are named as follows:
///
/// - The first `I`: signed, `U`: unsigned
/// - The number following the `I`/`U` determines the bit size of the term.
#[allow(missing_docs)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "mem_dbg", derive(mem_dbg::MemSize))]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Encode, Decode, PartialOrd, Ord, Hash)]
pub enum AddrTermSize {
    U8,
    I16,
    U16,
    I32,
    U32,
    U64,
}

impl From<AddrSize> for AddrTermSize {
    fn from(value: AddrSize) -> Self {
        match value {
            AddrSize::Addr16 => Self::U16,
            AddrSize::Addr20 => Self::U32,
            AddrSize::Addr32 => Self::U32,
            AddrSize::Addr64 => Self::U64,
        }
    }
}

impl Display for AddrTermSize {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            AddrTermSize::U8 => "u8",
            AddrTermSize::U16 => "u16",
            AddrTermSize::I16 => "i16",
            AddrTermSize::U32 => "u32",
            AddrTermSize::I32 => "i32",
            AddrTermSize::U64 => "u64",
        })
    }
}

impl AddrTermSize {
    /// Applies the sizing operation to `val`.
    pub fn apply(self, val: u64) -> u64 {
        match self {
            AddrTermSize::U64 => val,
            AddrTermSize::U8 => val as u8 as u64,
            AddrTermSize::U16 => val as u16 as u64,
            AddrTermSize::U32 => val as u32 as u64,
            AddrTermSize::I16 => val as i16 as u64,
            AddrTermSize::I32 => val as i32 as u64,
        }
    }

    /// Returns the number of possible values in a term with this size.
    /// Panicks for `AddrTermSize::U64`.
    pub fn num_values(self) -> u64 {
        match self {
            AddrTermSize::U8 => 0x100,
            AddrTermSize::U16 | AddrTermSize::I16 => 0x1_0000,
            AddrTermSize::U32 | AddrTermSize::I32 => 0x1_0000_0000,
            AddrTermSize::U64 => panic!("Cannot fit TermSize::U64::len() in a u64"),
        }
    }

    /// Returns true if the term is signed.
    pub fn is_signed(self) -> bool {
        match self {
            AddrTermSize::U8 | AddrTermSize::U16 | AddrTermSize::U32 | AddrTermSize::U64 => false,
            AddrTermSize::I16 | AddrTermSize::I32 => true,
        }
    }

    /// Returns the highest bit index that can be affected by this term.
    pub fn max_bit_influence(self) -> usize {
        match self {
            AddrTermSize::U8 => 8,
            AddrTermSize::U16 | AddrTermSize::I16 => 16,
            AddrTermSize::U32 | AddrTermSize::I32 => 32,
            AddrTermSize::U64 => 64,
        }
    }

    /// Returns the number of bits that must at least be used from this value in order for no smaller size to be applicable.
    /// For example, if you use 33 bits from an U64, there is no smaller size that could be used.
    /// However, if you use 27 bits from an U64, this operation could have also been done on an U32 instead.
    pub fn size_usefulness_threshold(self) -> usize {
        match self {
            AddrTermSize::U8 => 8,
            AddrTermSize::U16 | AddrTermSize::I16 => 9,
            AddrTermSize::U32 | AddrTermSize::I32 => 17,
            AddrTermSize::U64 => 33,
        }
    }

    pub fn is_relevant_for_addr_size(&self, addr_size: AddrSize) -> bool {
        use AddrTermSize::*;

        match (self, addr_size) {
            (U8, _) => true,
            (I16 | U16, AddrSize::Addr16) => true,
            (I16 | U16, AddrSize::Addr20) => true,
            (I16 | U16, AddrSize::Addr32) => true,
            (I16 | U16, AddrSize::Addr64) => true,
            (I32 | U32, AddrSize::Addr16) => false,
            // Needed because there are no I20/U20 variants of AddrTermSize.
            (I32 | U32, AddrSize::Addr20) => true,
            (I32 | U32, AddrSize::Addr32) => true,
            (I32 | U32, AddrSize::Addr64) => true,
            (U64, AddrSize::Addr16) => false,
            (U64, AddrSize::Addr20) => false,
            (U64, AddrSize::Addr32) => false,
            (U64, AddrSize::Addr64) => true,
        }
    }

    pub fn fits_in(&self, addr_size: AddrSize) -> bool {
        use AddrTermSize::*;

        match (self, addr_size) {
            (U8 | I16 | U16, AddrSize::Addr16 | AddrSize::Addr20 | AddrSize::Addr32 | AddrSize::Addr64) => true,
            (I32 | U32, AddrSize::Addr16 | AddrSize::Addr20) => false,
            (I32 | U32, AddrSize::Addr32 | AddrSize::Addr64) => true,
            (U64, AddrSize::Addr16 | AddrSize::Addr20 | AddrSize::Addr32) => false,
            (U64, AddrSize::Addr64) => true,
        }
    }
}

/// A shift-then-multiply operation.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "mem_dbg", derive(mem_dbg::MemSize))]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Encode, Decode, PartialOrd, Ord, Hash)]
pub struct AddrTermCalculation {
    /// The shift-then-multiply applied to the term.
    pub shift: AddrTermShift,

    /// The sizing operation applied to the term.
    pub size: AddrTermSize,
}

impl Display for AddrTermCalculation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "(X as {} >> {}) * {}", self.size, self.shift.right, self.shift.mult)
    }
}

impl AddrTermCalculation {
    /// Applies the shift-then-multiply operation to `val`.
    #[inline]
    pub fn apply(self, val: u64) -> u64 {
        self.shift.apply(self.size.apply(val))
    }

    /// Returns the highest bit index that this term can influence.
    #[inline]
    pub fn max_bit_influence(self) -> usize {
        let base = self.size.max_bit_influence();
        let mut mult_bits = 0;
        let mut k = 1;

        while k < self.shift.mult {
            k <<= 1;
            mult_bits += 1;
        }

        base + mult_bits - self.shift.right as usize
    }
}

/// An address term, consisting of a primary sized shift-then-multipy operation, and an optional second sized shift-then-multiply operation.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "mem_dbg", derive(mem_dbg::MemSize))]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Encode, Decode, PartialOrd, Ord, Hash)]
pub struct AddrTerm {
    /// The primary operation on the input.
    pub primary: AddrTermCalculation,

    /// The optional second use of the input.
    pub second_use: Option<AddrTermCalculation>,
}

impl Display for AddrTerm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.second_use {
            Some(second_use) => write!(f, "{} + {second_use}", self.primary),
            None => Display::fmt(&self.primary, f),
        }
    }
}

impl Default for AddrTerm {
    fn default() -> Self {
        AddrTerm {
            primary: AddrTermCalculation {
                shift: AddrTermShift {
                    mult: 0,
                    right: 0,
                },
                size: AddrTermSize::U64,
            },
            second_use: None,
        }
    }
}

impl AddrTerm {
    /// Creates a new address term that always returns 0.
    #[inline]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Creates an address term that always returns the value it receives.
    #[inline]
    pub fn identity(addr_size: impl Into<AddrTermSize>) -> Self {
        AddrTerm {
            primary: AddrTermCalculation {
                shift: AddrTermShift {
                    right: 0,
                    mult: 1,
                },
                size: addr_size.into(),
            },
            second_use: None,
        }
    }

    /// Creates an address term that sizes, shifts and multiplies once.
    pub fn single(size: AddrTermSize, right: u8, mult: u16) -> Self {
        AddrTerm {
            primary: AddrTermCalculation {
                shift: AddrTermShift {
                    right,
                    mult,
                },
                size,
            },
            second_use: None,
        }
    }

    pub fn double(
        primary_size: AddrTermSize, primary_right: u8, primary_mult: u16, secondary_size: AddrTermSize, secondary_right: u8,
        secondary_mult: u16,
    ) -> Self {
        AddrTerm {
            primary: AddrTermCalculation {
                shift: AddrTermShift {
                    right: primary_right,
                    mult: primary_mult,
                },
                size: primary_size,
            },
            second_use: Some(AddrTermCalculation {
                shift: AddrTermShift {
                    right: secondary_right,
                    mult: secondary_mult,
                },
                size: secondary_size,
            }),
        }
    }

    /// Applies the term to `val`.
    #[inline]
    pub fn apply(self, val: u64) -> u64 {
        self.primary
            .apply(val)
            .wrapping_add(self.second_use.map(|t| t.apply(val)).unwrap_or(0))
    }

    #[inline]
    pub fn is_empty(self) -> bool {
        self.primary.shift.mult == 0 && self.second_use.is_none()
    }
}

#[derive(Deserialize, Serialize)]
struct DisplayAddressComputation {
    calculation: AddressComputation,
    inputs: Vec<String>,
}

impl Debug for DisplayAddressComputation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Display::fmt(self, f)
    }
}

impl Display for DisplayAddressComputation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}(",
            match self.calculation.addr_size {
                AddrSize::Addr16 => "crop16",
                AddrSize::Addr20 => "crop20",
                AddrSize::Addr32 => "crop32",
                AddrSize::Addr64 => "crop64",
            }
        )?;

        let mut first = true;

        for (input, term) in self.inputs.iter().zip(self.calculation.terms.iter()) {
            if term.is_empty() || term.primary.size > AddrTermSize::from(self.calculation.addr_size) {
                continue
            }

            if first {
                first = false;
            } else {
                write!(f, " + ")?;
            }

            write_term(f, input, term)?;
        }

        match self.calculation.offset.cmp(&0) {
            Ordering::Less => {
                if !first {
                    write!(f, " - ")?;
                }

                write!(f, "0x{:X}", -self.calculation.offset)?
            },
            Ordering::Greater => {
                if !first {
                    write!(f, " + ")?;
                }

                write!(f, "0x{:X}", self.calculation.offset)?
            },
            Ordering::Equal => {
                if first {
                    write!(f, "0x0")?
                }
            },
        }

        write!(f, ")")?;

        for (input, term) in self.inputs.iter().zip(self.calculation.terms.iter()) {
            if term.is_empty() || term.primary.size <= AddrTermSize::from(self.calculation.addr_size) {
                continue
            }

            write!(f, " + ")?;
            write_term(f, input, term)?;
        }

        Ok(())
    }
}

fn write_term(f: &mut std::fmt::Formatter<'_>, input: &String, term: &AddrTerm) -> Result<(), std::fmt::Error> {
    write!(f, "{}{}", input, term.primary.size)?;
    if term.primary.shift.right > 0 {
        write!(f, " >> {}", term.primary.shift.right)?;
    }

    if term.primary.shift.mult != 1 {
        write!(f, " * {}", term.primary.shift.mult)?;
    }

    if let Some(second_use) = term.second_use {
        write!(f, " + {}{}", input, second_use.size)?;
        if second_use.shift.right > 0 {
            write!(f, " >> {}", second_use.shift.right)?;
        }

        if second_use.shift.mult != 1 {
            write!(f, " * {}", second_use.shift.mult)?;
        }
    }

    Ok(())
}

impl AddressComputation {
    pub fn display<'a, S: AsRef<str>>(&'a self, input_names: &'a [S]) -> impl Display + Debug + 'a {
        DisplayAddressComputation {
            calculation: self.clone(),
            inputs: input_names.iter().map(|n| n.as_ref().to_string()).collect::<Vec<_>>(),
        }
    }

    pub fn num_terms(&self) -> usize {
        self.terms.len()
    }

    /// Computes the address accessed by the instruction when executed on `state`, given that the computation uses `inputs`.
    #[inline]
    pub fn compute<A: Arch>(&self, inputs: &Inputs<A>, state: &SystemState<A>) -> u64 {
        self.evaluate_from_iter::<A>(inputs.iter().map(|input| match input.loc {
            UnsizedParLoc::Const(value) => value,
            UnsizedParLoc::Part(_) => panic!("Cannot evaluate an expression that contains immediate value references"),
            UnsizedParLoc::InstrLen => state.memory().get(0).2.len() as u64,
            _ => match state.get_dest(input) {
                Value::Num(n) => n,
                other => panic!("Cannot handle: {other:?}"),
            },
        }))
    }

    /// Computes the address accessed by the instruction when executed on `state`, given that the computation uses `inputs`.
    #[inline]
    pub fn compute_from_cpustate<A: Arch>(&self, inputs: &Inputs<A>, instr_len: u64, state: &A::CpuState) -> u64 {
        self.evaluate_from_iter::<A>(inputs.iter().map(|input| match input.loc {
            UnsizedParLoc::Const(value) => value,
            UnsizedParLoc::Part(_) => panic!("Cannot evaluate an expression that contains immediate value references"),
            UnsizedParLoc::InstrLen => instr_len,
            UnsizedParLoc::Reg(r) => match state.reg(r) {
                Value::Num(n) => n,
                other => panic!("Cannot handle: {other:?}"),
            },
            UnsizedParLoc::Mem(_) => unreachable!(),
        }))
    }

    /// Computes the address, using `inputs`.
    #[inline]
    pub fn evaluate<A: Arch>(&self, inputs: &[u64]) -> u64 {
        self.evaluate_from_iter::<A>(inputs.iter().copied())
    }

    /// Computes the address, using `inputs`.
    #[inline]
    pub fn evaluate_from_iter<A: Arch>(&self, inputs: impl Iterator<Item = u64>) -> u64 {
        let mut sum = 0u64;
        let mut base = 0u64;
        for (v, term) in inputs.zip(self.terms.iter()) {
            if term.primary.size > AddrTermSize::from(self.addr_size) {
                base = base.wrapping_add(term.apply(v));
            } else {
                sum = sum.wrapping_add(term.apply(v));
            }
        }

        let offset = self.addr_size.apply(sum.wrapping_add(self.offset as u64));
        A::ADDR_SIZE.apply(base.wrapping_add(offset))
    }

    pub fn add_term(&mut self, term: AddrTerm) {
        self.terms
            .try_push(term)
            .expect("add_term should have space to add another term");
    }

    /// An address computation of the form `A + B + C`.
    /// `num` determines the number of inputs.
    /// `num` cannot be more than 4.
    #[inline]
    pub fn unscaled_sum(num: usize, addr_size: AddrSize) -> AddressComputation {
        AddressComputation {
            offset: 0,
            terms: std::iter::repeat_n(AddrTerm::identity(addr_size), num).collect(),
            addr_size,
        }
    }

    /// Returns a new computation with the offset replaced with the specified `offset`.
    #[inline]
    pub fn with_addr_size(self, addr_size: AddrSize) -> AddressComputation {
        AddressComputation {
            addr_size,
            ..self
        }
    }

    /// Returns a new computation with the offset replaced with the specified `offset`.
    #[inline]
    pub fn with_offset(self, offset: i64) -> AddressComputation {
        AddressComputation {
            offset,
            ..self
        }
    }

    /// Returns a new computation with the offset incremented with the specified `offset`.
    #[inline]
    pub fn with_added_offset(self, offset: i64) -> AddressComputation {
        AddressComputation {
            offset: self.offset.wrapping_add(offset),
            ..self
        }
    }

    /// Returns a new computation with the terms (max 4) from `terms` and the `offset` specified.
    #[inline]
    pub fn from_iter(terms: impl Iterator<Item = AddrTerm>, offset: i64) -> AddressComputation {
        AddressComputation {
            terms: terms.collect(),
            offset,
            addr_size: AddrSize::Addr64,
        }
    }

    /// Adds a new [`AddrTerm::identity`] term if there are less than 4 terms.
    /// Otherwise, does nothing.
    #[inline]
    pub fn add_constant_term(&mut self) {
        self.terms.push(AddrTerm::identity(self.addr_size));
    }

    /// Removes the term at position `index` from the computation.
    /// If there are less than `index + 1` terms, the same computation is returned.
    #[inline]
    pub fn remove_term(self, index: usize) -> AddressComputation {
        AddressComputation {
            offset: self.offset,
            terms: self
                .terms
                .iter()
                .enumerate()
                .flat_map(|(n, term)| if n != index { Some(*term) } else { None })
                .collect(),
            addr_size: self.addr_size,
        }
    }
}
