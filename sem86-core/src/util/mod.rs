use std::fmt::{Debug, Display};

use itertools::Itertools;
use liblisa::Instruction;

pub mod lcvec;
pub mod miniprofiler;
pub mod packing;
pub mod ringbuf;
pub mod version;

pub fn delay(iterations: u32) {
    for _ in 0..iterations {
        std::hint::spin_loop();
    }
}

pub struct DebugAddr(u32);

impl Debug for DebugAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "0x{:X}", self.0)
    }
}

pub struct DebugInstr(Instruction);

impl Debug for DebugInstr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:X}", self.0)
    }
}

pub struct DebugInstrs<'a>(pub &'a [(u32, Instruction)]);

impl Debug for DebugInstrs<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map()
            .entries(
                self.0
                    .iter()
                    .sorted_by_key(|&(ip, _)| ip)
                    .map(|&(ip, instr)| (DebugAddr(ip), DebugInstr(instr))),
            )
            .finish()
    }
}

pub trait ByteSubstitutions {
    fn set_byte(&mut self, index: usize, val: u8);
    fn get_byte(&self, index: usize) -> u8;
}

impl ByteSubstitutions for u16 {
    fn set_byte(&mut self, index: usize, val: u8) {
        let offset = index * 8;
        let mask = !(0xff << offset);
        *self = (*self & mask) | ((val as u16) << offset);
    }

    fn get_byte(&self, index: usize) -> u8 {
        (*self >> (index * 8)) as u8
    }
}

impl ByteSubstitutions for u32 {
    fn set_byte(&mut self, index: usize, val: u8) {
        let offset = index * 8;
        let mask = !(0xff << offset);
        *self = (*self & mask) | ((val as u32) << offset);
    }

    fn get_byte(&self, index: usize) -> u8 {
        (*self >> (index * 8)) as u8
    }
}

impl ByteSubstitutions for usize {
    fn set_byte(&mut self, index: usize, val: u8) {
        let offset = index * 8;
        let mask = !(0xff << offset);
        *self = (*self & mask) | ((val as usize) << offset);
    }

    fn get_byte(&self, index: usize) -> u8 {
        (*self >> (index * 8)) as u8
    }
}

pub struct DisplayByteSize<N>(pub N);

impl Display for DisplayByteSize<u64> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        format_byte_size(f, self.0.try_into().unwrap())
    }
}

impl Display for DisplayByteSize<usize> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        format_byte_size(f, self.0.try_into().unwrap())
    }
}

impl<N: TryInto<u64>, F: Fn() -> N> Display for DisplayByteSize<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Ok(val) = (self.0)().try_into() else { panic!() };
        format_byte_size(f, val)
    }
}

fn format_byte_size(f: &mut std::fmt::Formatter<'_>, val: u64) -> Result<(), std::fmt::Error> {
    let units = [
        (0, 1, "B"),
        (2 << 10, 1 << 10, "KiB"),
        (300 << 10, 1 << 20, "MiB"),
        (300 << 20, 1 << 30, "GiB"),
    ];
    let (_, divisor, unit) = *units.iter().rev().find(|&&(threshold, ..)| val >= threshold).unwrap();
    write!(f, "{:.1}{unit}", val as f64 / (divisor as f64))
}
