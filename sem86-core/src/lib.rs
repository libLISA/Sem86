#![feature(try_blocks)]
#![feature(macro_metavar_expr)]
#![allow(incomplete_features, internal_features)]
#![feature(generic_const_exprs)]
#![feature(pattern_type_macro)]
#![feature(pattern_type_range_trait)]
#![feature(pattern_types)]
#![feature(const_trait_impl)]

use std::fmt::Display;
use std::str::FromStr;

use arbitrary_int::Number;
use arrayvec::ArrayVec;
use bilge::prelude::*;
use bitcode::{Decode, Encode};
use liblisa::Instruction;
use liblisa::arch::Arch;
use serde::{Deserialize, Serialize};

#[macro_export]
macro_rules! extend_path_with {
    ($val:literal) => {
        concat!(concat!(module_path!(), "::"), $val)
    };
}

pub mod arch;
pub mod codegen;
pub mod decoder;
pub mod emulator;
pub mod hw;
pub mod icache;
pub mod il;
pub mod jit;
pub mod system;
pub mod tests;
pub mod time;
pub mod tracefile;
pub mod util;

struct DisplayK(u64);

impl Display for DisplayK {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let digits = self.0.to_string();
        for (n, c) in [(9, "G"), (6, "M"), (3, "k")] {
            if digits.len() > n {
                write!(f, "{}{}{}", &digits[..digits.len() - n], c, &digits[digits.len() - n..])?;
                return Ok(())
            }
        }

        write!(f, "{}", digits)
    }
}

#[derive(Debug)]
pub enum IoEv {
    WriteChar(char),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Encode, Decode)]
pub enum SegmentSizes {
    Cs16Ss16,
    Cs16Ss32,
    Cs32Ss16,
    Cs32Ss32,
}

impl SegmentSizes {
    pub fn is_cs32(&self) -> bool {
        match self {
            SegmentSizes::Cs16Ss16 | SegmentSizes::Cs16Ss32 => false,
            SegmentSizes::Cs32Ss16 | SegmentSizes::Cs32Ss32 => true,
        }
    }
}

impl Bitsized for SegmentSizes {
    type ArbitraryInt = u2;
    const BITS: usize = 2;
    const MAX: Self::ArbitraryInt = u2::new(3);
}

impl From<SegmentSizes> for u2 {
    fn from(value: SegmentSizes) -> Self {
        u2::new(value as u8)
    }
}

impl From<u2> for SegmentSizes {
    fn from(value: u2) -> Self {
        match value.as_u8() {
            0 => Self::Cs16Ss16,
            1 => Self::Cs16Ss32,
            2 => Self::Cs32Ss16,
            3 => Self::Cs32Ss32,
            _ => unreachable!(),
        }
    }
}

impl FromStr for SegmentSizes {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "cs16ss16" => Self::Cs16Ss16,
            "cs16ss32" => Self::Cs16Ss32,
            "cs32ss16" => Self::Cs32Ss16,
            "cs32ss32" => Self::Cs32Ss32,
            _ => return Err(String::from("invalid segment sizes")),
        })
    }
}

pub struct Before<A: Arch> {
    pub instr: Instruction,
    pub cpu: A::CpuState,
    pub mem: ArrayVec<u64, 16>,
}
