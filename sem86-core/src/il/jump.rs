use std::fmt::Display;

use arrayvec::ArrayVec;
use itertools::Itertools;
use liblisa::arch::Arch;
use liblisa::encoding::{ParLoc, UnsizedParLoc};
use serde::{Deserialize, Serialize};

use crate::il::Val;
use crate::il::part_values::{PackingStructure, PartValues};

/// Specifies what assumptions can be made about the next instruction that will be executed.
/// For common cases (relative and absolute near jumps), the value to which the IP will jump is specified.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum Jump<A: Arch> {
    /// IP should be incremented by the instruction length.
    /// That is, there is only one possible next instruction.
    /// That instruction is the instruction right after this one.
    Sequential,

    /// IP is only incremented by the instruction length if `condition` is false.
    ///
    /// The condition is evaluated after the semantics have been executed, and may reference values and temporaries computed in the semantics.
    ///
    /// There are two possible next instructions:
    /// - If condition is non-zero, IP is unchanged and the same instruction is executed again.
    /// - If condition is zero, IP is incremented by the instruction length and the next sequential instruction is executed.
    Repeat { condition: Val<A> },

    /// If `condition` is non-zero, the sum of all values in `offset` should be added to the IP register.
    ///
    /// Regardless of the value of `condition`, the IP register should also be incremented by the instruction length.
    /// The `offset` value should therefore be the post-IP-increment offset.
    ///
    /// Current code size should be taken into account.
    /// When running in 16-bit mode, the new value of IP should be cropped to 16 bits.
    /// When running in 32-bit mode, the new value of IP should be cropped to 32 bits.
    ///
    /// There are two possible next instructions:
    /// - If condition is non-zero, `offset` is added to IP, and the instruction at the new IP will be executed.
    /// - If condition is zero, the next sequential instruction is executed.
    NearRelativeOffset {
        /// Jump is only taken if this condition is non-zero.
        ///
        /// May be set to a non-zero constant if the jump is always taken.
        ///
        /// The condition is evaluated after the semantics have been executed, and may reference values and temporaries computed in the semantics.
        condition: Val<A>,

        /// The post-IP-increment offset of the jump.
        ///
        /// The sum of all these values is taken as the offset.
        ///
        /// All values in this offset should be constant given a concrete instruction.
        /// That is, they may consist of constants and immediate values only.
        ///
        /// For more complex jumps, use [`Self::NearAbsolute`] instead, and perform the computation in the semantics themselves.
        offset: Vec<Val<A>>,
    },

    /// Indicates that the IP should be updated to the value specified.
    /// The value is evaluated after the semantics have been executed, and may reference values and temporaries computed in the semantics.
    ///
    /// There are an unbounded number of possible next instructions.
    NearAbsolute(Val<A>),

    /// Indicates that one should expect this instruction to update both CS and IP in arbitrary fashion.
    /// No assumptions can be made about the resulting values in CS and IP.
    ///
    /// There are an unbounded number of possible next instructions.
    /// The CS may have changed, which means the processor may have switched modes and CPL may have changed.
    /// Before looking up the next instruction, current processor state should be inspected if necessary.
    Far,
}

impl<A: Arch> Display for Jump<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Jump::Sequential => write!(f, "next sequential"),
            Jump::Repeat {
                condition,
            } => write!(f, "repeat if {condition}"),
            Jump::NearRelativeOffset {
                condition,
                offset,
            } => write!(f, "if {condition} jump to (IP + {})", offset.iter().format(" + ")),
            Jump::NearAbsolute(val) => write!(f, "to {val}"),
            Jump::Far => write!(f, "far"),
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NextIp {
    Absolute(u128),
    Relative(i128),
}

impl<A: Arch> Jump<A> {
    pub fn map(&self, mut map_flows: impl FnMut(bool, &ParLoc<A>) -> Option<ParLoc<A>>) -> Jump<A> {
        match self {
            Jump::Sequential => Jump::Sequential,
            Jump::Repeat {
                condition,
            } => Jump::Repeat {
                condition: condition.map_locs(&mut map_flows),
            },
            Jump::NearRelativeOffset {
                condition,
                offset,
            } => Jump::NearRelativeOffset {
                condition: condition.map_locs(&mut map_flows),
                offset: offset.iter().map(|val| val.map_locs(&mut map_flows)).collect(),
            },
            Jump::NearAbsolute(val) => Jump::NearAbsolute(val.map_locs(&mut map_flows)),
            Jump::Far => Jump::Far,
        }
    }

    /// Returns a superset of all possible IPs after successful execution of the instruction.
    pub fn try_determine_next_ips(
        &self, part_values: PartValues, ps: &PackingStructure, instr_len: usize, is_cs32: bool,
    ) -> Option<ArrayVec<NextIp, 2>> {
        match self {
            Jump::Sequential => Some([NextIp::Relative(instr_len as i128)].into_iter().collect()),
            Jump::Repeat {
                ..
            } => Some(
                [NextIp::Relative(0), NextIp::Relative(instr_len as i128)]
                    .into_iter()
                    .collect(),
            ),
            Jump::NearRelativeOffset {
                offset,
                condition,
            } => {
                let not_taken = NextIp::Relative(instr_len as i128);
                let offset = instr_len as i128
                    + offset
                        .iter()
                        .map(|&val| {
                            let par_loc = val.loc().expect("temporary variables are not supported as offsets");
                            let result = match par_loc.loc {
                                UnsizedParLoc::Reg(_) => unimplemented!(),
                                UnsizedParLoc::Mem(_) => unimplemented!(),
                                UnsizedParLoc::Part(n) => part_values.get(ps, n) as i128,
                                UnsizedParLoc::InstrLen => instr_len as i128,
                                UnsizedParLoc::Const(val) => val as i64 as i128,
                            };

                            match val {
                                Val::Temp(_) => unreachable!(),
                                Val::Loc(_) => result,
                                Val::Conv {
                                    source_bits,
                                    target_bits,
                                    sign_extend,
                                    swap_endianness,
                                    ..
                                } => Val::<A>::apply_conversion(
                                    result as u128,
                                    source_bits,
                                    sign_extend,
                                    swap_endianness,
                                    target_bits,
                                ) as i128,
                            }
                        })
                        .sum::<i128>();
                let shift = if is_cs32 { 128 - 32 } else { 128 - 16 };
                // Sign extend from 16 or 32 bits.
                let offset = (offset << shift) >> shift;
                let taken = NextIp::Relative(offset);

                match condition.as_const() {
                    Some(0) => Some([not_taken].into_iter().collect()),
                    Some(_) => Some([taken].into_iter().collect()),
                    None => Some([not_taken, taken].into_iter().collect()),
                }
            },
            Jump::NearAbsolute(_) => None,
            Jump::Far => None,
        }
    }

    /// Returns `(next_ip_if_condition_zero, next_ip_if_condition_nonzero)`
    pub fn try_derive_ips_from_condition(
        &self, part_values: PartValues, ps: &PackingStructure, instr_len: usize, is_cs32: bool,
    ) -> Option<(NextIp, NextIp)> {
        match self {
            Jump::Sequential => Some((NextIp::Relative(instr_len as i128), NextIp::Relative(instr_len as i128))),
            Jump::Repeat {
                ..
            } => Some((NextIp::Relative(instr_len as i128), NextIp::Relative(0))),
            Jump::NearRelativeOffset {
                offset,
                condition,
            } => {
                let not_taken = NextIp::Relative(instr_len as i128);
                let offset = instr_len as i128
                    + offset
                        .iter()
                        .map(|&val| {
                            let par_loc = val.loc().expect("temporary variables are not supported as offsets");
                            let result = match par_loc.loc {
                                UnsizedParLoc::Reg(_) => unimplemented!(),
                                UnsizedParLoc::Mem(_) => unimplemented!(),
                                UnsizedParLoc::Part(n) => part_values.get(ps, n) as i128,
                                UnsizedParLoc::InstrLen => instr_len as i128,
                                UnsizedParLoc::Const(val) => val as i64 as i128,
                            };

                            match val {
                                Val::Temp(_) => unreachable!(),
                                Val::Loc(_) => result,
                                Val::Conv {
                                    source_bits,
                                    target_bits,
                                    sign_extend,
                                    swap_endianness,
                                    ..
                                } => Val::<A>::apply_conversion(
                                    result as u128,
                                    source_bits,
                                    sign_extend,
                                    swap_endianness,
                                    target_bits,
                                ) as i128,
                            }
                        })
                        .sum::<i128>();
                let shift = if is_cs32 { 128 - 32 } else { 128 - 16 };
                // Sign extend from 16 or 32 bits.
                let offset = (offset << shift) >> shift;
                let taken = NextIp::Relative(offset);

                match condition.as_const() {
                    Some(_) => None,
                    None => Some((not_taken, taken)),
                }
            },
            Jump::NearAbsolute(_) => None,
            Jump::Far => None,
        }
    }

    pub fn is_sequential(&self) -> bool {
        matches!(self, Jump::Sequential)
    }

    pub fn is_fixed_relative(&self) -> bool {
        matches!(self, Jump::Sequential | Jump::Repeat { .. } | Jump::NearRelativeOffset { .. })
    }
}
