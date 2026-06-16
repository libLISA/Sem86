use std::collections::HashMap;
use std::error::Error;

use arrayvec::ArrayVec;
use bilge::prelude::*;
use liblisa::utils::EitherIter;

use crate::codegen::graph_traits::{Graph, Node};
use crate::codegen::lir::Lir;
use crate::codegen::mm::Object;
use crate::emulator::Emulator;
use crate::il::part_values::PartValues;

pub mod cranelift;
pub mod interpreter;

// #[cfg(not(target_os = "android"))]
pub mod inkwell;

#[derive(Clone, Debug)]
pub struct TracedAccess {
    pub addr: u64,
    pub len: u8,
    pub is_write: bool,
    pub data: u128,
}

#[bitsize(8)]
#[derive(Copy, Clone, DebugBits, FromBits)]
pub struct JitExecutionResult {
    pub can_continue_execution: bool,
    pub jump_taken: bool,
    reserved: u6,
}

pub trait UninstantiatedBackendFn: Copy + Clone + Default {
    unsafe fn from_ptr(ptr: *mut u8) -> Self;

    fn execute_uninstantiated(
        &self, emulator: &mut Emulator<'_, '_>, instr_len: u8, part_values: PartValues, trace_memory: impl FnMut(TracedAccess),
    ) -> JitExecutionResult;

    /// Should return a pointer to a function that takes (emulator: &mut Emulator, instr_len: u8, part_values: u128) as arguments
    fn as_fptr(&self) -> fn(&mut Emulator, u8, PartValues) -> u64 {
        unimplemented!("as_fptr()")
    }
}

pub trait BackendFn: Copy + Clone + Default {
    unsafe fn from_ptr(ptr: *mut u8) -> Self;

    fn execute(&self, emulator: &mut Emulator<'_, '_>, trace_memory: impl FnMut(TracedAccess)) -> (JitExecutionResult, u64);

    /// Should return a pointer to a function that takes (emulator: &mut Emulator, instr_len: u8, part_values: u128) as arguments
    fn as_fptr(&self) -> fn(&mut Emulator) -> u64 {
        unimplemented!("as_fptr()")
    }
}

pub trait Backend {
    type UninstantiatedFn: UninstantiatedBackendFn;
    type Fn: BackendFn;
    type Error: Error;

    fn codegen_lir(&mut self, lir: &Lir) -> Result<Self::UninstantiatedFn, Self::Error>;

    fn codegen_lir_object(&mut self, lir: &Lir) -> Result<Object, Self::Error>;
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NextInstr {
    pub offset: u16,
    pub block_index: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NextOnPage {
    /// The next instruction is *likely* on this page, at one of these offsets.
    ///
    /// Before jumping, the current CS:IP must be checked to make sure we are still on the same page.
    Speculative(ArrayVec<NextInstr, 2>),

    /// The next instruction can be derived from the last jump condition.
    ///
    /// Neither CS:IP nor the frame offset needs to be checked for this
    FromCondition {
        condition_nonzero: Option<NextInstr>,
        condition_zero: Option<NextInstr>,
    },

    /// The offset in the page at which the next instruction is located.
    Certain(NextInstr),
}

impl NextOnPage {
    fn items(&self) -> impl Iterator<Item = NextInstr> {
        match self {
            NextOnPage::Speculative(array_vec) => array_vec.iter().cloned().collect::<Vec<_>>().into_iter(),
            NextOnPage::FromCondition {
                condition_nonzero,
                condition_zero,
            } => [condition_nonzero, condition_zero]
                .iter()
                .flat_map(|&&v| v)
                .collect::<Vec<_>>()
                .into_iter(),
            NextOnPage::Certain(next_instr) => vec![*next_instr].into_iter(),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            NextOnPage::Speculative(v) => v.is_empty(),
            NextOnPage::FromCondition {
                condition_nonzero,
                condition_zero,
            } => condition_nonzero.is_none() && condition_zero.is_none(),
            NextOnPage::Certain(_) => false,
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = NextInstr> {
        match self {
            NextOnPage::Speculative(v) => EitherIter::Left(v.iter().copied()),
            NextOnPage::FromCondition {
                condition_nonzero,
                condition_zero,
            } => EitherIter::Right((*condition_nonzero).into_iter().chain(*condition_zero)),
            NextOnPage::Certain(next_instr) => EitherIter::Right(Some(*next_instr).into_iter().chain(None)),
        }
    }
}

#[derive(Clone)]
pub struct LirBlock {
    pub lir: Lir,
    pub export: bool,
    pub offset: u16,
    pub id: u32,
    pub next: HashMap<u64, NextOnPage>,
    pub check_intr: bool,
}

impl Graph for &[LirBlock] {
    type Index = usize;
    type Node = LirBlock;
    const ROOT: Self::Index = 0;

    fn num_nodes(&self) -> usize {
        self.len()
    }

    fn node(&self, index: Self::Index) -> &Self::Node {
        &self[index]
    }
}

impl Node<usize> for LirBlock {
    fn transitions(&self) -> impl Iterator<Item = usize> {
        self.next.values().flat_map(|v| v.items()).map(|item| item.block_index)
    }
}
