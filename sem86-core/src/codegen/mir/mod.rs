use std::collections::HashMap;
use std::fmt::{Debug, Display};
use std::io::Write;
use std::iter::once;
use std::time::Instant;
use std::u64;

use arrayvec::ArrayVec;
use bb::{Bb, BbBuilder};
use itertools::Itertools;
use liblisa::Instruction;
use liblisa::arch::{Arch, Register};
use liblisa::encoding::bitpattern::{ImmBitOrder, MappingOrBitOrder, PartMapping};
use liblisa::encoding::dataflows::{AddrTermCalculation, AddressComputation, MemoryAccess, ParameterizedComputation};
use liblisa::encoding::{EncodingRef, IgnoredMetadata, ParLoc, UnsizedParLoc};
use liblisa::state::{Size, UnsizedLoc};
use liblisa::utils::{EitherIter, bitmask_u128};
use log::{debug, info, trace};
use sem86_arch::exceptions::Exception;
use serde::{Deserialize, Serialize};
use val::{ValBuilder, ValId};

use crate::arch::intel386::{GpReg, Intel386, Reg, State};
use crate::codegen::components::StronglyConnectedComponents;
use crate::codegen::graph_traits::{Graph, Node};
use crate::codegen::mir::bb::{BbFallible, BbGraph, BbId, BbSeq, CommitDest};
use crate::codegen::mir::state::{UncommittedLoc, UncommittedState};
use crate::codegen::mir::val::{ValTree, VarId};
use crate::codegen::{DataSize, Ptr};
use crate::il::part_values::PartValues;
use crate::il::{BinOp, Cmd, Commands, Jump, MAX_TEMP_VARS, MiniSemRef, Op, UnOp, Val};

pub mod bb;
pub mod egraph;
pub mod state;
pub mod union_find;
pub mod val;

/// A middle intermediate representation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Mir {
    pub value_tree: ValTree,
    pub control_flow: BbGraph,
}

impl Display for Mir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (id, block) in self.control_flow.iter() {
            write!(f, "B{:04}: ", id.index())?;
            match block {
                Bb::Jump(id) => write!(f, "JUMP !{:04}", id.index())?,
                Bb::Br {
                    cond,
                    if_zero,
                    if_nonzero,
                } => write!(
                    f,
                    "IF_ZERO ({}) THEN !{:04} ELSE !{:04}",
                    self.value_tree.display(*cond),
                    if_zero.index(),
                    if_nonzero.index()
                )?,
                Bb::Seq {
                    entry,
                    next,
                } => {
                    match entry {
                        BbSeq::Store {
                            values,
                        } => {
                            writeln!(f)?;
                            for (dest, val) in values.iter() {
                                writeln!(f, "  {dest:?} <- {}", self.value_tree.display(*val))?;
                            }

                            write!(f, "   ")?;
                        },
                        BbSeq::ReadMemoryUnchecked {
                            value,
                            addr,
                            num_bytes,
                        } => write!(
                            f,
                            "{value:?} <- read_mem.{num_bytes}.unchecked {}",
                            self.value_tree.display(*addr)
                        )?,
                        BbSeq::WriteMemoryUnchecked {
                            value,
                            addr,
                            num_bytes,
                        } => write!(
                            f,
                            "write_mem.{num_bytes}.unchecked {} {}",
                            self.value_tree.display(*addr),
                            self.value_tree.display(*value)
                        )?,
                        BbSeq::Log {
                            message,
                        } => write!(f, "Log {message:?}")?,
                        BbSeq::SetException {
                            exception,
                            code,
                        } => write!(f, "SetException {exception:?} {}", self.value_tree.display(*code))?,
                        BbSeq::SetHandler {
                            id,
                            args,
                        } => write!(
                            f,
                            "SetHandler {id:?}({}, {})",
                            self.value_tree.display(args[0]),
                            self.value_tree.display(args[1])
                        )?,
                        BbSeq::Commit {
                            values,
                        } => {
                            writeln!(f)?;
                            for (dest, val) in values.iter() {
                                writeln!(f, "  {dest:?} <- {}", self.value_tree.display(*val))?;
                            }
                        },
                    }

                    write!(f, " THEN JUMP !{:04}", next.index())?;
                },
                Bb::Fallible {
                    op,
                    if_ok,
                    if_exception,
                } => {
                    match op {
                        BbFallible::PortOut {
                            port,
                            data,
                            len,
                        } => write!(
                            f,
                            "PORT_OUT.{len} {}, {}",
                            self.value_tree.display(*port),
                            self.value_tree.display(*data)
                        )?,
                        BbFallible::PortIn {
                            port,
                            data,
                            len,
                        } => write!(f, "{data:?} <- PORT_IN.{len} {}", self.value_tree.display(*port))?,
                        BbFallible::ReadMemory {
                            value,
                            addr,
                            num_bytes,
                        } => write!(f, "{value:?} <- read_mem.{num_bytes} {}", self.value_tree.display(*addr))?,
                        BbFallible::WriteMemory {
                            value,
                            addr,
                            num_bytes,
                        } => write!(
                            f,
                            "write_mem.{num_bytes} {} {}",
                            self.value_tree.display(*addr),
                            self.value_tree.display(*value)
                        )?,
                        BbFallible::ReadDescriptor {
                            selector,
                            force,
                            mark_accessed,
                            ok,
                            base,
                            limit,
                            ar,
                        } => write!(
                            f,
                            "({ok:?}, {base:?}, {limit:?}, {ar:?}) <- read_descriptor {} force={force}, mark_accessed={mark_accessed}",
                            self.value_tree.display(*selector)
                        )?,
                    }

                    write!(f, " THEN IF_SUCCESS !{:04} ELSE !{:04}", if_ok.index(), if_exception.index())?;
                },
                Bb::CommitAndExit {
                    values,
                    k,
                    success,
                    metadata,
                    last_jump_condition,
                } => {
                    writeln!(f)?;
                    for (dest, val) in values.iter() {
                        write!(f, "  ")?;
                        match dest {
                            CommitDest::Reg(reg) => write!(f, "{reg}")?,
                            CommitDest::Fixed {
                                offset,
                                size,
                                ..
                            } => write!(f, "[Cpu + 0x{offset:X}]:{size:?}")?,
                            CommitDest::Dynamic {
                                offset,
                                size,
                                ..
                            } => write!(f, "[Cpu + {}]:{size:?}", self.value_tree.display(*offset))?,
                        }

                        writeln!(f, " <- {}", self.value_tree.display(*val))?;
                    }

                    if let Some(k) = k {
                        writeln!(f, "  k <- {}", self.value_tree.display(*k))?;
                    }

                    write!(f, "  RETURN {success} with {metadata:?}")?;

                    if let Some(last_jump_condition) = last_jump_condition {
                        write!(f, " (last jump condition: {})", self.value_tree.display(*last_jump_condition))?;
                    }
                },
            }

            writeln!(f)?;
        }

        Ok(())
    }
}

impl Mir {
    pub fn export_dot(&self, mut output: impl Write) -> std::io::Result<()> {
        writeln!(output, "strict digraph {{")?;
        writeln!(output, "rankdir=\"LR\"")?;
        writeln!(output, "{{")?;
        writeln!(output, "rank=\"min\";")?;
        writeln!(output, "start [shape=point];")?;
        writeln!(output, "}}")?;
        writeln!(output, "start -> bb0")?;

        for (id, node) in self.control_flow.iter() {
            writeln!(output, "bb{} [label=\"{}\"]", id.index(), node)?;
            for next in node.next_blocks() {
                writeln!(output, "bb{} -> bb{} [label=\"{:?}\"]", id.index(), next.index(), next)?;
            }

            for val in node.referenced_values() {
                writeln!(output, "v{} -> bb{}", val.index(), id.index())?;
            }
        }

        for (id, val) in self.value_tree.iter() {
            writeln!(output, "v{} [label=\"{}\"]", id.index(), val)?;
            for referenced in val.referenced_nodes() {
                writeln!(output, "v{} -> v{}", referenced.index(), id.index())?;
            }
        }

        writeln!(output, "}}")
    }
}

pub enum InstantiatedDest {
    Loc(UnsizedLoc<Intel386>),
    Const(u128),
    Part(usize),
    InstrLen,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ValTarget {
    Loc(ParLoc<Intel386>),
    Temp(usize),
}

impl TryFrom<Val<Intel386>> for ValTarget {
    type Error = ();

    fn try_from(value: Val<Intel386>) -> Result<Self, Self::Error> {
        Ok(match value {
            Val::Temp(n) => ValTarget::Temp(n),
            Val::Loc(par_loc) => ValTarget::Loc(par_loc),
            Val::Conv {
                ..
            } => return Err(()),
        })
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum UnsizedValTarget {
    Loc(UnsizedLoc<Intel386>),
    Temp(usize),
    Part(usize),
}

use InstantiatedDest::*;

pub struct MirBuilder {
    value_tree_builder: ValBuilder,
    control_flow: BbBuilder,
    uncommitted_cpu_state_at: HashMap<BbId, UncommittedState>,
    current_block: BbId,
    vars: VarAlloc,
}

#[derive(Copy, Clone)]
pub struct EncodingEntry<'a> {
    pub instr: Option<Instruction>,
    pub instr_len: usize,
    pub encoding: EncodingRef<'a, Intel386, MiniSemRef<'a, Intel386>, IgnoredMetadata>,
    pub part_values: PartValues,
    pub metadata: Option<u64>,
    pub is_cs32: bool,
}

#[derive(Copy, Clone)]
pub struct EncodingEntryWithNext<'a> {
    pub entry: EncodingEntry<'a>,

    /// If the next instruction is within the entries provided, jumps[jump_taken] contains the index of this entry.
    /// Otherwise, it is None.
    pub jumps: [Option<usize>; 2],
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InstructionEntry {
    pub instr: Instruction,
    pub next_ip: ArrayVec<u32, 2>,
    pub next_ips_is_exhaustive: bool,
}

struct VarAlloc {
    next_var_id: usize,
}

impl VarAlloc {
    pub fn alloc_var(&mut self) -> VarId {
        let var = VarId::from_usize(self.next_var_id);
        self.next_var_id += 1;
        var
    }
}

#[macro_export]
macro_rules! instr_map {
    ($($addr:literal: $instr:expr),* $(,)*) => {
        [
            $(($addr, <liblisa::instr::Instruction as std::str::FromStr>::from_str($instr).unwrap())),*
        ]
    };
}

impl<'r> Graph for &[EncodingEntryWithNext<'r>] {
    type Index = usize;
    type Node = EncodingEntryWithNext<'r>;

    const ROOT: Self::Index = 0;

    fn num_nodes(&self) -> usize {
        self.len()
    }

    fn node(&self, index: Self::Index) -> &Self::Node {
        &self[index]
    }
}

impl Node<usize> for EncodingEntryWithNext<'_> {
    fn transitions(&self) -> impl Iterator<Item = usize> {
        self.jumps.iter().flatten().copied()
    }
}

impl Default for MirBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl MirBuilder {
    pub fn new() -> Self {
        Self {
            value_tree_builder: ValBuilder::new(),
            control_flow: BbBuilder::new(),
            uncommitted_cpu_state_at: HashMap::new(),
            current_block: BbId::ROOT,
            vars: VarAlloc {
                next_var_id: 0,
            },
        }
    }

    pub fn build_from_branching(protected_mode_memory_accesses: bool, items: &[EncodingEntryWithNext<'_>]) -> Mir {
        let start = Instant::now();
        let mut builder = MirBuilder::new();

        let mut entry_blocks = vec![Vec::<(BbId, UncommittedState)>::new(); items.len()];

        let reverse_topological_order = {
            let mut reverse_topological_order = Vec::<usize>::new();
            StronglyConnectedComponents::iterate(&items, |nodes| {
                assert_eq!(nodes.len(), 1, "loops not supported");
                reverse_topological_order.extend(nodes.iter().copied());
            });

            reverse_topological_order
        };

        assert_eq!(*reverse_topological_order.last().unwrap(), 0);

        for &index in reverse_topological_order.iter().rev() {
            let entry = items[index];
            let (entry, jumps) = (&entry.entry, &entry.jumps);
            debug!("Emitting {:X?}", entry.instr);

            // We must unify all previous blocks.
            // We leave a "fixup" block after each jump to a different instruction.
            // These fixup blocks can be used for any stores needed to unify the state.
            // We do that here.
            let unified_block = builder.create_block();
            let blocks_to_unify = &entry_blocks[index];
            let states = blocks_to_unify.iter().map(|(_, state)| state).collect::<Vec<_>>();

            let (state, stores) =
                UncommittedState::merge_n(&states, &mut builder.value_tree_builder, || builder.vars.alloc_var());
            let prev = builder.uncommitted_cpu_state_at.insert(unified_block, state);
            assert!(prev.is_none());
            for ((fixup_block, _), stores) in blocks_to_unify.iter().zip(stores) {
                builder.control_flow.switch_to(*fixup_block);
                builder.control_flow.write_seq(BbSeq::Store {
                    values: stores,
                });
                builder.control_flow.write_terminal(Bb::Jump(unified_block));
            }

            // Emit the encoding
            let has_next = jumps.iter().flatten().next().is_some();
            let (_, last_jump_condition) = builder.emit_encoding_without_jump(
                entry.instr_len,
                entry.encoding,
                entry.part_values,
                protected_mode_memory_accesses,
                unified_block,
                entry.metadata,
                entry.is_cs32,
                has_next,
            );

            // If there are any jumps, we emit them.
            if has_next {
                let condition_was_false = builder.create_block();
                let condition_was_true = builder.create_block();
                builder.control_flow.write_terminal(Bb::Br {
                    cond: last_jump_condition.unwrap(),
                    if_zero: condition_was_false,
                    if_nonzero: condition_was_true,
                });

                match jumps[0] {
                    Some(next) => entry_blocks[next].push((condition_was_false, builder.uncommitted_cpu_state().clone())),
                    None => {
                        builder.control_flow.switch_to(condition_was_false);
                        builder.commit_and_exit(true, entry.metadata, last_jump_condition);
                    },
                }

                match jumps[1] {
                    Some(next) => entry_blocks[next].push((condition_was_true, builder.uncommitted_cpu_state().clone())),
                    None => {
                        builder.control_flow.switch_to(condition_was_true);
                        builder.commit_and_exit(true, entry.metadata, last_jump_condition);
                    },
                }
            } else {
                // No jumps does not mean that the instruction will always terminate.
                // Sometimes the jumps may just be to a different page, which we also cannot handle here.
                builder.terminate_block_if_needed(entry.metadata, last_jump_condition);
            }
        }

        info!("Emitting encodings took {}ms", start.elapsed().as_millis());

        let start = Instant::now();
        info!("Finishing...");
        builder.terminate_block_if_needed(None, None);
        let result = builder.finish();

        info!("Finishing took {}ms", start.elapsed().as_millis());

        result
    }

    /// Builds a function that executes the provided chain of instructions sequentially.
    /// Metadata contains the index of the last executed instruction.
    pub fn build_from_sequence(protected_mode_memory_accesses: bool, items: &[EncodingEntry<'_>]) -> Mir {
        let start = Instant::now();
        let mut builder = MirBuilder::new();

        let blocks = items.iter().map(|_| builder.create_block()).collect::<Vec<_>>();
        let mut last_jump_condition = None;
        for (index, (entry, &block)) in items.iter().zip(blocks.iter()).enumerate() {
            // TODO: Check if an interrupt occurred
            builder.emit_jump(block);

            debug!("Emitting: {}", entry.encoding.to_owned());
            let is_last = index == items.len() - 1;
            (_, last_jump_condition) = builder.emit_encoding_without_jump(
                entry.instr_len,
                entry.encoding,
                entry.part_values,
                protected_mode_memory_accesses,
                block,
                entry.metadata,
                entry.is_cs32,
                is_last,
            );
        }

        info!("Emitting encodings took {}ms", start.elapsed().as_millis());

        let start = Instant::now();
        info!("Finishing...");
        builder.terminate_block_if_needed(items.last().unwrap().metadata, last_jump_condition);
        let result = builder.finish();

        info!("Finishing took {}ms", start.elapsed().as_millis());

        result
    }

    pub fn build_from_uninstantiated_encoding(
        encoding: EncodingRef<'_, Intel386, MiniSemRef<'_, Intel386>, IgnoredMetadata>, protected_mode_memory_accesses: bool,
        is_cs32: bool,
    ) -> Mir {
        // TODO: Improve performance of register parts. Right now we load all registers, modify one of them, then save them all. If possible, we should only load the register we're actually modifying.
        let mut builder = MirBuilder::new();

        let main_block = builder.create_block();
        builder.emit_jump(main_block);

        builder.control_flow.switch_to(main_block);
        builder.current_block = main_block;

        // Compute whether we should do protected mode memory accesses.
        // This is the case when protected mode is enabled in CR0 and we are not running in virtual 8086 mode.
        let zero = builder.value_tree_builder.imm(0);
        // let UnsizedParLoc::Reg(vm_flag_reg) = FLAG_VM.loc else { unreachable!() };
        // let vm = builder.value_tree_builder.load_ptr_imm(Ptr::CpuState, DataSize::Byte, (State::byte_offset_of(vm_flag_reg) + FLAG_VM.size.start_byte) as u16);
        // let cr0 = builder.value_tree_builder.load_ptr_imm(Ptr::CpuState, DataSize::Dword, State::byte_offset_of(Reg::Gp(GpReg::Cr0)) as u16);
        // let one = builder.value_tree_builder.imm(1);
        // let protected_mode = builder.value_tree_builder.binop(BinOp::And, [ cr0, one ]);
        // let not_vm = builder.value_tree_builder.unop(UnOp::IsZero, vm);
        // let protected_mode_memory_accesses = builder.value_tree_builder.binop(BinOp::And, [ protected_mode, not_vm ]);

        let mut emitter = EncodingEmitter {
            instr_len: None,
            encoding,
            part_values: None,
            tmp: [zero; _],
            memory: ArrayVec::new(),
            core: CoreEncodingEmitter {
                protected_mode_memory_accesses: builder.value_tree_builder.imm(protected_mode_memory_accesses as u128),
                initial_cpu_state: builder.uncommitted_cpu_state().clone(),
                builder: &mut builder,
                metadata: None,
            },
            is_cs32,
        };

        // TODO: Increment K regardless of whether the encoding fails or not by modifying initial_cpu_state instead of uncommitted_cpu_state.
        emitter.increment_k();
        emitter.emit_memory_accesses();
        emitter.emit_commands(encoding.semantics.commands, true);
        let last_jump_condition = emitter.emit_jump();
        builder.terminate_block_if_needed(None, last_jump_condition);

        builder.finish()
    }

    fn emit_encoding_without_jump(
        &mut self, instr_len: usize, encoding: EncodingRef<'_, Intel386, MiniSemRef<'_, Intel386>, IgnoredMetadata>,
        part_values: PartValues, protected_mode_memory_accesses: bool, bb: BbId, metadata: Option<u64>, is_cs32: bool,
        is_last: bool,
    ) -> (ValId, Option<ValId>) {
        // TODO: Remove is_last and replace it with a closure that can be called multiple times to emit multiple jumps to the next instruction.
        trace!("Emitting: {}", encoding.to_owned());

        self.control_flow.switch_to(bb);
        self.current_block = bb;

        let zero = self.value_tree_builder.imm(0);
        let protected_mode_memory_accesses = self.value_tree_builder.imm(protected_mode_memory_accesses as u128);
        let mut emitter = EncodingEmitter {
            instr_len: Some(instr_len),
            encoding,
            part_values: Some(part_values),
            tmp: [zero; _],
            memory: ArrayVec::new(),
            core: CoreEncodingEmitter {
                protected_mode_memory_accesses,
                initial_cpu_state: self.uncommitted_cpu_state().clone(),
                builder: self,
                metadata,
            },
            is_cs32,
        };
        let ip_before_encoding = emitter.core.emit_reg(Reg::Gp(GpReg::Ip));

        // TODO: Increment K regardless of whether the encoding fails or not by modifying initial_cpu_state instead of uncommitted_cpu_state.
        emitter.increment_k();
        emitter.emit_memory_accesses();
        emitter.emit_commands(encoding.semantics.commands, is_last);
        let last_jump_condition = emitter.emit_jump();
        (ip_before_encoding, last_jump_condition)
    }

    pub fn finish(mut self) -> Mir {
        if !self.control_flow.block_is_terminated() {
            panic!("Block is not terminated")
        }

        info!("Building control flow...");
        Mir {
            control_flow: self.control_flow.build(&mut self.value_tree_builder),
            value_tree: self.value_tree_builder.build(),
        }
    }

    fn uncommitted_cpu_state(&mut self) -> &mut UncommittedState {
        self.uncommitted_cpu_state_at.get_mut(&self.current_block).unwrap()
    }

    fn with_uncommitted_cpu_state<T>(&mut self, f: impl FnOnce(&mut ValBuilder, &mut UncommittedState) -> T) -> T {
        let state = self.uncommitted_cpu_state_at.get_mut(&self.current_block).unwrap();
        f(&mut self.value_tree_builder, state)
    }

    fn terminate_block_if_needed(&mut self, metadata: Option<u64>, last_jump_condition: Option<ValId>) {
        if !self.control_flow.block_is_terminated() {
            self.commit_and_exit(true, metadata, last_jump_condition);
        }
    }

    fn commit_and_exit<'r>(&mut self, success: bool, metadata: Option<u64>, last_jump_condition: Option<ValId>) {
        let state = self.uncommitted_cpu_state().clone();
        let values = state
            .iter(&mut self.value_tree_builder)
            .sorted()
            .flat_map(|(loc, y)| {
                let dest = match loc {
                    UncommittedLoc::Reg(reg) => {
                        if reg.is_flags() {
                            let flag_mask = reg.mask().unwrap_or(u64::MAX);
                            return EitherIter::Right(
                                std::array::from_fn::<_, 8, _>(|n| {
                                    let byte_mask = 0xff << (n * 8);
                                    if flag_mask & byte_mask != 0 {
                                        let offset = (State::byte_offset_of(reg) + n) as u16;
                                        trace!("Committing {reg}[{n}] (offset 0x{offset:X}) = {y:?}");
                                        let val = self.value_tree_builder.extract(y, n as u8 * 8, 8);

                                        if val != self.value_tree_builder.load_ptr_imm(Ptr::CpuState, DataSize::Byte, offset) {
                                            Some((
                                                CommitDest::Fixed {
                                                    size: DataSize::Byte,
                                                    offset,
                                                },
                                                val,
                                            ))
                                        } else {
                                            None
                                        }
                                    } else {
                                        None
                                    }
                                })
                                .into_iter()
                                .flatten(),
                            );
                        }

                        let original_value = self.value_tree_builder.load_ptr_imm(
                            Ptr::CpuState,
                            DataSize::try_from_bytes(reg.byte_size()).unwrap(),
                            State::byte_offset_of(reg) as u16,
                        );
                        let updated_bytes = self.value_tree_builder.determine_updated_bytes_in(y, original_value);
                        if let Some(updated_bytes) = updated_bytes
                            && updated_bytes <= reg.byte_size() / 2
                        {
                            let updated_bytes = updated_bytes.next_power_of_two();
                            let sliced = self.value_tree_builder.extract(y, 0, updated_bytes as u8 * 8);
                            return EitherIter::Left(once((
                                CommitDest::Fixed {
                                    size: DataSize::try_from_bytes(updated_bytes).unwrap(),
                                    offset: State::byte_offset_of(reg) as u16,
                                },
                                sliced,
                            )))
                        }

                        CommitDest::Reg(reg)
                    },
                    UncommittedLoc::Dynamic {
                        offset,
                        size,
                        ..
                    } => CommitDest::Dynamic {
                        offset,
                        size: DataSize::try_from_bytes(size).expect("DataSize should exist for all register sizes"),
                    },
                };

                EitherIter::Left(once((dest, y)))
            })
            .collect();
        self.control_flow.write_terminal(Bb::CommitAndExit {
            metadata,
            values,
            k: state.k(),
            success,
            last_jump_condition,
        });
    }

    pub fn control_flow(&mut self) -> &mut BbBuilder {
        &mut self.control_flow
    }

    pub fn create_block(&mut self) -> BbId {
        self.control_flow.create_block()
    }

    pub fn emit_jump(&mut self, next: BbId) {
        let state = self.uncommitted_cpu_state_at.entry(self.current_block).or_default().clone();
        let prev = self.uncommitted_cpu_state_at.insert(next, state.clone());
        assert!(prev.is_none(), "encodings must be emitted in topological order");
        self.control_flow.write_terminal(Bb::Jump(next));
    }
}

struct MemEntry {
    var: Option<VarId>,
    value: ValId,
    seg_info: Option<SegmentInfo>,
    addr: ValId,
    _offset_mask: ValId,
    segchecked: bool,
    needs_write: bool,
    num_bytes: usize,
}

impl MemEntry {
    pub fn read(&mut self, e: &mut CoreEncodingEmitter<'_>) {
        if let Some(seg_info) = self.seg_info {
            self.emit_memory_segcheck(e, seg_info);
        }

        let value = e.builder.vars.alloc_var();

        // let real_mode_block = builder.control_flow.create_block();
        // let protected_mode_block = builder.control_flow.create_block();
        // let done_block = builder.control_flow.create_block();

        // TODO: Fix real mode reads
        // builder.control_flow.write_and_switch_to(Bb::Br {
        //     cond: self.protected_mode_memory_accesses,
        //     if_zero: real_mode_block,
        //     if_nonzero: protected_mode_block,
        // }, protected_mode_block);
        // builder.control_flow.write_and_switch_to(Bb::Jump(protected_mode_block), protected_mode_block);

        e.emit_fallible(BbFallible::ReadMemory {
            value,
            addr: self.addr,
            num_bytes: self.num_bytes.try_into().unwrap(),
        });
        // builder.control_flow.write_and_switch_to(Bb::Jump(done_block), real_mode_block);

        // let mut result = builder.value_tree_builder.imm(0);
        // for n in 0..self.num_bytes {
        //     let tmp = builder.alloc_var();
        //     let n_imm = builder.value_tree_builder.imm(n as u128);
        //     let offset = builder.value_tree_builder.binop(BinOp::Add, [ offset, n_imm ]);
        //     let offset = builder.value_tree_builder.binop(BinOp::And, [ offset, offset_mask ]);
        //     let addr = builder.value_tree_builder.binop(BinOp::Add, [ base, offset ]);
        //     self.emit_fallible(BbFallible::ReadMemory {
        //         value: tmp,
        //         addr,
        //         num_bytes: 1,
        //     });

        //     let tmp = builder.value_tree_builder.use_var(tmp);
        //     let shift = builder.value_tree_builder.imm(n as u128 * 8);
        //     let val = builder.value_tree_builder.binop(BinOp::Shl, [ tmp, shift ]);
        //     result = builder.value_tree_builder.binop(BinOp::Or, [ result, val ]);
        // }

        // builder.control_flow.write_seq(BbSeq::Store {
        //     values: vec![
        //         (value, result),
        //     ]
        // });
        // builder.control_flow.write_and_switch_to(Bb::Jump(done_block), done_block);

        self.var = Some(value);
        self.value = e.builder.value_tree_builder.use_var(value);
    }

    fn emit_memory_segcheck(&mut self, e: &mut CoreEncodingEmitter<'_>, base: SegmentInfo) {
        // TODO: Track this per branch
        if self.segchecked {
            return;
        } else {
            self.segchecked = true;
        }

        let everything_ok = e.builder.control_flow.create_block();

        // Make sure the selector is not NULL
        let check_block = e.builder.control_flow.create_block();
        let selector_ok_block = e.builder.control_flow.create_block();
        e.builder.control_flow.write_and_switch_to(
            Bb::Br {
                cond: e.protected_mode_memory_accesses,
                if_zero: selector_ok_block,
                if_nonzero: check_block,
            },
            check_block,
        );

        let selector = base.selector;

        let selector_null_block = e.builder.control_flow.create_block();
        let mask = e.builder.value_tree_builder.imm(0xfff8);

        let selector_index = e.builder.value_tree_builder.binop(BinOp::And, [selector, mask]);
        e.builder.control_flow.write_and_switch_to(
            Bb::Br {
                cond: selector_index,
                if_zero: selector_null_block,
                if_nonzero: selector_ok_block,
            },
            selector_null_block,
        );

        let code = e.builder.value_tree_builder.imm(0);
        e.emit_exception(Exception::GeneralProtectionFault(0), code);

        e.builder.control_flow.switch_to(selector_ok_block);

        // Check segment limits
        // TODO: For multi-byte accesses, the entire access must be within the limits
        let ar = base.access_rights;
        let limit = base.limit;

        let effective_start = e.builder.value_tree_builder.extract(ar, 32, 32);
        let effective_offset = e.builder.value_tree_builder.binop(BinOp::Sub, [self.addr, effective_start]);
        let effective_offset = e.builder.value_tree_builder.extract(effective_offset, 0, 32);
        let is_out_of_range = e.builder.value_tree_builder.binop(BinOp::CmpGt, [effective_offset, limit]);

        let throw_exception = e.builder.control_flow.create_block();

        e.builder.control_flow.write_and_switch_to(
            Bb::Br {
                cond: is_out_of_range,
                if_zero: everything_ok,
                if_nonzero: throw_exception,
            },
            throw_exception,
        );
        let code = e.builder.value_tree_builder.imm(0);
        e.emit_exception(Exception::GeneralProtectionFault(0), code);

        e.builder.control_flow.switch_to(everything_ok);
    }

    fn write(&self, e: &mut CoreEncodingEmitter<'_>) {
        if self.needs_write {
            // let real_mode_block = e.builder.control_flow.create_block();
            // let protected_mode_block = e.builder.control_flow.create_block();
            // let done_block = e.builder.control_flow.create_block();

            // TODO: Fix real mode writes
            // e.builder.control_flow.write_and_switch_to(Bb::Br {
            //     cond: self.protected_mode_memory_accesses,
            //     if_zero: real_mode_block,
            //     if_nonzero: protected_mode_block,
            // }, protected_mode_block);
            // e.builder.control_flow.write_and_switch_to(Bb::Jump(protected_mode_block), protected_mode_block);

            e.emit_fallible(BbFallible::WriteMemory {
                value: self.value,
                addr: self.addr,
                num_bytes: self.num_bytes.try_into().unwrap(),
            });
            // e.builder.control_flow.write_and_switch_to(Bb::Jump(done_block), real_mode_block);

            // let byte_mask = e.builder.value_tree_builder.imm(0xff);
            // for n in 0..self.num_bytes {
            //     let n_imm = e.builder.value_tree_builder.imm(n as u128);
            //     let offset = e.builder.value_tree_builder.binop(BinOp::Add, [ self.offset, n_imm ]);
            //     let offset = e.builder.value_tree_builder.binop(BinOp::And, [ offset, self.offset_mask ]);
            //     let addr = e.builder.value_tree_builder.binop(BinOp::Add, [ self.base, offset ]);

            //     let shift = e.builder.value_tree_builder.imm(n as u128 * 8);
            //     let value = e.builder.value_tree_builder.binop(BinOp::Shr, [ self.value, shift ]);
            //     let value = e.builder.value_tree_builder.binop(BinOp::And, [ value, byte_mask ]);

            //     e.emit_fallible(BbFallible::WriteMemory {
            //         value,
            //         addr,
            //         num_bytes: 1,
            //     });
            // }

            // e.builder.control_flow.write_and_switch_to(Bb::Jump(done_block), done_block);
        }
    }
}

pub struct EncodingEmitter<'a> {
    instr_len: Option<usize>,
    encoding: EncodingRef<'a, Intel386, MiniSemRef<'a, Intel386>, IgnoredMetadata>,
    part_values: Option<PartValues>,
    tmp: [ValId; MAX_TEMP_VARS],
    memory: ArrayVec<MemEntry, 16>,
    core: CoreEncodingEmitter<'a>,
    is_cs32: bool,
}

struct CoreEncodingEmitter<'a> {
    builder: &'a mut MirBuilder,
    initial_cpu_state: UncommittedState,
    protected_mode_memory_accesses: ValId,
    metadata: Option<u64>,
}

impl CoreEncodingEmitter<'_> {
    fn emit_fallible(&mut self, op: BbFallible) {
        let fail_block = self.builder.control_flow.create_block();
        let ok_block = self.builder.control_flow.create_block();
        self.builder.control_flow.write_and_switch_to(
            Bb::Fallible {
                op,
                if_ok: ok_block,
                if_exception: fail_block,
            },
            fail_block,
        );

        let current_state = self.builder.uncommitted_cpu_state().clone();
        *self.builder.uncommitted_cpu_state() = self.initial_cpu_state.clone();
        self.builder.commit_and_exit(false, self.metadata, None);
        self.builder.control_flow.switch_to(ok_block);
        *self.builder.uncommitted_cpu_state() = current_state.clone();
    }

    fn emit_exception(&mut self, exception: Exception, code: ValId) {
        self.builder.control_flow.write_seq(BbSeq::SetException {
            exception,
            code,
        });

        let current_state = self.builder.uncommitted_cpu_state().clone();
        *self.builder.uncommitted_cpu_state() = self.initial_cpu_state.clone();
        self.builder.commit_and_exit(false, self.metadata, None);
        *self.builder.uncommitted_cpu_state() = current_state.clone();
    }

    fn emit_reg(&mut self, reg: Reg) -> ValId {
        self.builder
            .with_uncommitted_cpu_state(|value_tree_builder, state| state.get_or_compute(reg, value_tree_builder))
    }
}

#[derive(Copy, Clone)]
struct SegmentInfo {
    selector: ValId,
    access_rights: ValId,
    limit: ValId,
}

impl SegmentInfo {
    pub fn from_base(seg_base: GpReg, emitter: &mut EncodingEmitter<'_>) -> Self {
        let (seg_selector, seg_ar, seg_limit) = match seg_base {
            GpReg::CsBase => (GpReg::Cs, GpReg::CsAr, GpReg::CsLimit),
            GpReg::DsBase => (GpReg::Ds, GpReg::DsAr, GpReg::DsLimit),
            GpReg::SsBase => (GpReg::Ss, GpReg::SsAr, GpReg::SsLimit),
            GpReg::EsBase => (GpReg::Es, GpReg::EsAr, GpReg::EsLimit),
            GpReg::FsBase => (GpReg::Fs, GpReg::FsAr, GpReg::FsLimit),
            GpReg::GsBase => (GpReg::Gs, GpReg::GsAr, GpReg::GsLimit),
            _ => unreachable!(),
        };

        Self {
            selector: emitter.core.emit_reg(Reg::Gp(seg_selector)),
            access_rights: emitter.core.emit_reg(Reg::Gp(seg_ar)),
            limit: emitter.core.emit_reg(Reg::Gp(seg_limit)),
        }
    }
}

impl EncodingEmitter<'_> {
    fn emit_memory_accesses(&mut self) {
        for index in 0..self.encoding.semantics.addresses.len() {
            let access = &self.encoding.semantics.addresses[index];
            let segment = access.inputs.iter().enumerate()
                .find_map(|(index, input)| match input.loc {
                    UnsizedParLoc::Reg(seg_base) => {
                        match seg_base {
                            Reg::Gp(seg_base) if seg_base.is_segment_base() => Some((index, SegmentInfo::from_base(seg_base, self))),
                            _ => None,
                        }
                    },
                    UnsizedParLoc::Part(part_index) => if let Some(part_values) = self.part_values {
                        match &self.encoding.parts[part_index].mapping {
                            PartMapping::Register { mapping } => if matches!(mapping[part_values.get(self.encoding.semantics.part_packing, part_index) as usize].unwrap(), Reg::Gp(r) if r.is_segment_base()) {
                                let seg_base = mapping[part_values.get(self.encoding.semantics.part_packing, part_index) as usize].unwrap();
                                Some((index, SegmentInfo::from_base(Intel386::try_reg_to_gpreg(seg_base).unwrap(), self)))
                            } else {
                                None
                            },
                            _ => None,
                        }
                    } else {
                        match &self.encoding.parts[part_index].mapping {
                            PartMapping::Register { mapping } => {
                                let all_segments = mapping.iter().flatten().all(|reg| reg.is_segment_base());
                                let no_segments = mapping.iter().flatten().all(|reg| !reg.is_segment_base());

                                if all_segments {
                                    if let Some((const_scale, const_offset)) = compute_const_scale_offset(mapping) {
                                        assert!(self.part_values.is_none(), "TODO: need to account for segments potentially having been updated");

                                        let part_values = self.core.builder.value_tree_builder.part_values();
                                        let packing = &self.encoding.semantics.part_packing[part_index];
                                        let part_bits_value = self.core.builder.value_tree_builder.extract(part_values, packing.offset(), packing.len());
                                        let const_offset = self.core.builder.value_tree_builder.imm(const_offset as u64 as u128);
                                        let const_scale = self.core.builder.value_tree_builder.imm(const_scale as u128);
                                        let offset = self.core.builder.value_tree_builder.binop(BinOp::Mul, [ const_scale, part_bits_value ]);
                                        let eight = self.core.builder.value_tree_builder.imm(8);
                                        let base_offset = self.core.builder.value_tree_builder.binop(BinOp::Add, [ const_offset, offset ]);
                                        let selector_offset = self.core.builder.value_tree_builder.binop(BinOp::Sub, [ base_offset, eight ]);
                                        let ar_offset = self.core.builder.value_tree_builder.binop(BinOp::Add, [ base_offset, eight ]);
                                        let limit_offset = self.core.builder.value_tree_builder.binop(BinOp::Add, [ ar_offset, eight ]);

                                        Some((index, SegmentInfo {
                                            selector: self.core.builder.value_tree_builder.load_ptr_offset(Ptr::CpuState, DataSize::Dword, selector_offset),
                                            access_rights: self.core.builder.value_tree_builder.load_ptr_offset(Ptr::CpuState, DataSize::Qword, ar_offset),
                                            limit: self.core.builder.value_tree_builder.load_ptr_offset(Ptr::CpuState, DataSize::Dword, limit_offset),
                                        }))
                                    } else {
                                        panic!("Segments must be in order that allows for const offset-scale optimization")
                                    }
                                } else if no_segments {
                                    None
                                } else {
                                    panic!("parts must be all-segment or no segments, encountered a mix of both: {mapping:?}")
                                }
                            },
                            _ => None,
                        }
                    },
                    _ => None,
                });

            let computation = match &access.calculation {
                ParameterizedComputation::FromPart(part_index) => {
                    if let Some(part_values) = self.part_values {
                        if let PartMapping::MemoryComputation {
                            mapping,
                        } = &self.encoding.parts[*part_index].mapping
                        {
                            mapping[part_values.get(self.encoding.semantics.part_packing, *part_index) as usize]
                                .as_ref()
                                .unwrap()
                        } else {
                            unreachable!()
                        }
                    } else {
                        unreachable!("memory computation parts not supported yet")
                    }
                },
                ParameterizedComputation::Calculation(computation) => computation,
            };

            // TODO: Turn base_index_and_reg into base_index and (selector, ar, limit) segment values.
            let entry = self.prepare_memory_entry(computation, access, segment);
            self.memory.push(entry);
        }
    }

    fn emit_term(&mut self, term: AddrTermCalculation, val: ValId) -> ValId {
        // Apply size
        let val = if term.size.is_signed() {
            self.core
                .builder
                .value_tree_builder
                .unop(UnOp::SignExtend(term.size.max_bit_influence().try_into().unwrap()), val)
        } else {
            let mask = self
                .core
                .builder
                .value_tree_builder
                .imm(bitmask_u128(term.size.max_bit_influence() as u32));
            self.core.builder.value_tree_builder.binop(BinOp::And, [val, mask])
        };

        // Apply shift
        let val = if term.shift.right() != 0 {
            let shift = self.core.builder.value_tree_builder.imm(term.shift.right() as u128);
            self.core.builder.value_tree_builder.binop(BinOp::Shr, [val, shift])
        } else {
            val
        };

        if term.shift.mult() != 1 {
            if term.shift.mult().is_power_of_two() {
                let shift = self
                    .core
                    .builder
                    .value_tree_builder
                    .imm(term.shift.mult().trailing_zeros() as u128);
                self.core.builder.value_tree_builder.binop(BinOp::Shl, [val, shift])
            } else {
                let mult = self.core.builder.value_tree_builder.imm(term.shift.mult() as u128);
                self.core.builder.value_tree_builder.binop(BinOp::Mul, [val, mult])
            }
        } else {
            val
        }
    }

    fn prepare_memory_entry(
        &mut self, computation: &AddressComputation, access: &MemoryAccess<Intel386>,
        base_index_and_seg_info: Option<(usize, SegmentInfo)>,
    ) -> MemEntry {
        let mut base = None;
        let mut offset = None;
        let base_index = base_index_and_seg_info.map(|(n, _)| n);
        for (index, (&input, term)) in access.inputs.iter().zip(computation.terms.iter()).enumerate() {
            let val = self.emit_val(Val::Loc(input), true);
            let primary = self.emit_term(term.primary, val);
            let second_use = term.second_use.map(|term| self.emit_term(term, val));

            let sum = if let Some(second_use) = second_use {
                self.core.builder.value_tree_builder.binop(BinOp::Add, [primary, second_use])
            } else {
                primary
            };
            let sum = if Some(index) != base_index
                && let Some(offset) = offset.take()
            {
                self.core.builder.value_tree_builder.binop(BinOp::Add, [sum, offset])
            } else {
                sum
            };

            if Some(index) == base_index {
                base = Some(sum);
            } else {
                offset = Some(sum);
            }
        }

        let base = base.unwrap();
        let offset = offset.unwrap();
        let const_offset = self.core.builder.value_tree_builder.imm(computation.offset as u128);
        let offset = self.core.builder.value_tree_builder.binop(BinOp::Add, [offset, const_offset]);

        let offset_mask = self
            .core
            .builder
            .value_tree_builder
            .imm(bitmask_u128(computation.addr_size.num_bits() as u32));
        let offset = self.core.builder.value_tree_builder.binop(BinOp::And, [offset, offset_mask]);

        let addr = self.core.builder.value_tree_builder.binop(BinOp::Add, [base, offset]);
        let addr = self.core.builder.value_tree_builder.extract(addr, 0, 32);

        MemEntry {
            addr,
            _offset_mask: offset_mask,
            segchecked: false,
            needs_write: false,
            num_bytes: access.size.end as usize,
            value: self.core.builder.value_tree_builder.imm(0),
            var: None,
            seg_info: base_index_and_seg_info.map(|(_, seg)| seg),
        }
    }

    fn emit_commands(&mut self, ops: &Commands<Intel386>, last: bool) {
        let Commands::Ops(ops) = ops;
        for (index, cmd) in ops.iter().enumerate() {
            self.emit_cmd(cmd, last && index == ops.len() - 1);
        }
    }

    fn instantiate_dest(&self, loc: UnsizedParLoc<Intel386>, apply_memory_imm_transformation: bool) -> InstantiatedDest {
        match loc {
            UnsizedParLoc::Reg(reg) => Loc(UnsizedLoc::Reg(reg)),
            UnsizedParLoc::Mem(index) => Loc(UnsizedLoc::Memory(index)),
            UnsizedParLoc::Part(index) => {
                if let Some(part_values) = self.part_values {
                    match &self.encoding.parts[index].mapping {
                        PartMapping::Register {
                            mapping,
                        } => Loc(UnsizedLoc::Reg(
                            mapping[part_values.get(self.encoding.semantics.part_packing, index) as usize].unwrap(),
                        )),
                        PartMapping::Imm {
                            mapping,
                            bits,
                        } => {
                            assert!(bits.is_none());

                            let part_val = part_values.get(self.encoding.semantics.part_packing, index) as u128;
                            let val = if apply_memory_imm_transformation {
                                mapping
                                    .as_ref()
                                    .map(|mapping| mapping.compute(part_val as u64).unwrap())
                                    .unwrap_or(part_val as u64) as u128
                            } else {
                                part_val
                            };

                            Const(val)
                        },
                        _ => unreachable!(),
                    }
                } else {
                    Part(index)
                }
            },
            UnsizedParLoc::InstrLen => {
                if let Some(instr_len) = self.instr_len {
                    Const(instr_len as u128)
                } else {
                    InstrLen
                }
            },
            UnsizedParLoc::Const(n) => Const(n as u128),
        }
    }

    fn emit_cmd(&mut self, cmd: &Cmd<Intel386>, last: bool) {
        match cmd {
            Cmd::Store {
                to,
                op,
            } => {
                let val = self.emit_op(op);
                self.emit_store(to, val);
            },
            Cmd::StoreDynamicReg {
                regs,
                index,
                value,
                size,
            } => {
                let index = self.emit_val(*index, false);
                let new_val = self.emit_val(*value, false);
                let old_val = self.core.builder.with_uncommitted_cpu_state(|value_tree_builder, state| {
                    state.get_or_compute_dynamic(regs, index, value_tree_builder)
                });
                let combined = self
                    .core
                    .builder
                    .value_tree_builder
                    .combine_old_and_new(old_val, new_val, *size);
                self.core.builder.with_uncommitted_cpu_state(|value_tree_builder, state| {
                    state.store_dynamic(regs, index, value_tree_builder, combined)
                });
            },
            Cmd::LoadDynamicReg {
                regs,
                index,
                into,
                size,
            } => {
                let index = self.emit_val(*index, false);
                let val = self.core.builder.with_uncommitted_cpu_state(|value_tree_builder, state| {
                    state.get_or_compute_dynamic(regs, index, value_tree_builder)
                });
                let val =
                    self.core
                        .builder
                        .value_tree_builder
                        .extract(val, size.start_byte() as u8 * 8, size.num_bytes() as u8 * 8);
                self.emit_store(into, val);
            },
            Cmd::Log {
                message,
            } => self.core.builder.control_flow.write_seq(BbSeq::Log {
                message: message.clone(),
            }),
            Cmd::If {
                val,
                if_zero,
                if_nonzero,
            } => {
                let cond = self.emit_val(*val, false);

                let if_zero_block = self.core.builder.control_flow.create_block();
                let if_nonzero_block = self.core.builder.control_flow.create_block();

                let if_zero_terminates = if_zero.all_paths_terminate();
                let if_nonzero_terminates = if_nonzero.all_paths_terminate();

                if last || (if_zero_terminates && if_nonzero_terminates) {
                    self.core.builder.control_flow.write_and_switch_to(
                        Bb::Br {
                            cond,
                            if_zero: if_zero_block,
                            if_nonzero: if_nonzero_block,
                        },
                        if_zero_block,
                    );

                    let before = Backup::make(self);
                    self.emit_commands(if_zero, last);
                    let last_jump_condition = if !self.core.builder.control_flow.block_is_terminated() {
                        self.emit_jump()
                    } else {
                        None
                    };
                    self.core
                        .builder
                        .terminate_block_if_needed(self.core.metadata, last_jump_condition);

                    before.restore(self);
                    self.core.builder.control_flow.switch_to(if_nonzero_block);
                    self.emit_commands(if_nonzero, last);
                    let last_jump_condition = if !self.core.builder.control_flow.block_is_terminated() {
                        self.emit_jump()
                    } else {
                        None
                    };
                    self.core
                        .builder
                        .terminate_block_if_needed(self.core.metadata, last_jump_condition);

                    // Switch to new block for any additional (ignored) cmds
                    let after_if_block = self.core.builder.control_flow.create_block();
                    self.core.builder.control_flow.switch_to(after_if_block);
                } else if if_zero_terminates {
                    self.core.builder.control_flow.write_and_switch_to(
                        Bb::Br {
                            cond,
                            if_zero: if_zero_block,
                            if_nonzero: if_nonzero_block,
                        },
                        if_zero_block,
                    );

                    let before = Backup::make(self);
                    self.emit_commands(if_zero, last);

                    before.restore(self);
                    self.core.builder.control_flow.switch_to(if_nonzero_block);
                    self.emit_commands(if_nonzero, last);

                    assert!(!self.core.builder.control_flow.block_is_terminated());
                } else if if_nonzero_terminates {
                    self.core.builder.control_flow.write_and_switch_to(
                        Bb::Br {
                            cond,
                            if_zero: if_zero_block,
                            if_nonzero: if_nonzero_block,
                        },
                        if_nonzero_block,
                    );

                    let before = Backup::make(self);
                    self.emit_commands(if_nonzero, last);

                    before.restore(self);
                    self.core.builder.control_flow.switch_to(if_zero_block);
                    self.emit_commands(if_zero, last);

                    assert!(!self.core.builder.control_flow.block_is_terminated());
                } else {
                    let after_if_zero_block = self.core.builder.control_flow.create_block();
                    let after_if_nonzero_block = self.core.builder.control_flow.create_block();
                    let after_if_block = self.core.builder.control_flow.create_block();
                    let before = Backup::make(self);

                    self.core.builder.control_flow.write_and_switch_to(
                        Bb::Br {
                            cond,
                            if_zero: if_zero_block,
                            if_nonzero: if_nonzero_block,
                        },
                        if_zero_block,
                    );

                    // If zero
                    self.emit_commands(if_zero, last);
                    if self.core.builder.control_flow.block_is_terminated() {
                        self.core.builder.control_flow.switch_to(if_nonzero_block);
                    } else {
                        self.core
                            .builder
                            .control_flow
                            .write_and_switch_to(Bb::Jump(after_if_zero_block), if_nonzero_block);
                    }

                    let after_if_zero = Backup::make(self);

                    // If nonzero
                    before.restore(self);
                    self.emit_commands(if_nonzero, last);
                    if self.core.builder.control_flow.block_is_terminated() {
                        self.core.builder.control_flow.switch_to(after_if_zero_block);
                    } else {
                        self.core
                            .builder
                            .control_flow
                            .write_and_switch_to(Bb::Jump(after_if_nonzero_block), after_if_zero_block);
                    }

                    let after_if_nonzero = Backup::make(self);
                    let (if_zero_updates, if_nonzero_updates) = Backup::merge(after_if_zero, after_if_nonzero, self);

                    self.core.builder.control_flow.write_and_switch_to(
                        Bb::Seq {
                            entry: BbSeq::Store {
                                values: if_zero_updates,
                            },
                            next: after_if_block,
                        },
                        after_if_nonzero_block,
                    );
                    self.core.builder.control_flow.write_and_switch_to(
                        Bb::Seq {
                            entry: BbSeq::Store {
                                values: if_nonzero_updates,
                            },
                            next: after_if_block,
                        },
                        after_if_block,
                    );
                }
            },
            Cmd::Exception {
                exception,
                code,
            } => {
                let code = self.emit_val(*code, false);
                self.core.emit_exception(*exception, code);
            },
            Cmd::Handler {
                id,
                args,
            } => {
                assert!(args.len() <= 2);
                let args = args
                    .iter()
                    .copied()
                    .chain(std::iter::repeat(Val::const_val(0)))
                    .take(2)
                    .map(|val| self.emit_val(val, false))
                    .collect::<Vec<_>>();
                self.core.builder.control_flow.write_seq(BbSeq::SetHandler {
                    id: *id,
                    args: args.try_into().unwrap(),
                });

                let last_jump_condition = self.emit_jump();
                self.core
                    .builder
                    .commit_and_exit(false, self.core.metadata, last_jump_condition);
            },
            Cmd::ReadDescriptor {
                force,
                selector,
                ok,
                base,
                limit,
                access_rights,
                mark_accessed,
            } => {
                let ok_var = self.core.builder.vars.alloc_var();
                let base_var = self.core.builder.vars.alloc_var();
                let limit_var = self.core.builder.vars.alloc_var();
                let ar_var = self.core.builder.vars.alloc_var();

                let selector = self.emit_val(*selector, false);
                self.core.emit_fallible(BbFallible::ReadDescriptor {
                    selector,
                    force: *force,
                    mark_accessed: *mark_accessed,
                    ok: ok_var,
                    base: base_var,
                    limit: limit_var,
                    ar: ar_var,
                });

                let ok_var = self.core.builder.value_tree_builder.use_var(ok_var);
                let base_var = self.core.builder.value_tree_builder.use_var(base_var);
                let limit_var = self.core.builder.value_tree_builder.use_var(limit_var);
                let ar_var = self.core.builder.value_tree_builder.use_var(ar_var);
                self.emit_store(ok, ok_var);
                self.emit_store(base, base_var);
                self.emit_store(limit, limit_var);
                self.emit_store(access_rights, ar_var);
            },
            Cmd::Out {
                len,
                port,
                data,
            } => {
                let port = self.emit_val(*port, false);
                let data = self.emit_val(*data, false);

                self.core.emit_fallible(BbFallible::PortOut {
                    port,
                    data,
                    len: (*len).try_into().unwrap(),
                });
            },
            Cmd::In {
                len,
                port,
                data,
            } => {
                let port = self.emit_val(*port, false);
                let var = self.core.builder.vars.alloc_var();
                self.core.emit_fallible(BbFallible::PortIn {
                    port,
                    data: var,
                    len: (*len).try_into().unwrap(),
                });

                let val = self.core.builder.value_tree_builder.use_var(var);
                self.emit_store(data, val);
            },
            Cmd::ReadMemory {
                index,
            } => {
                self.memory[*index].read(&mut self.core);
            },
            Cmd::WriteMemory {
                index,
            } => {
                self.memory[*index].write(&mut self.core);
            },
        }
    }

    fn emit_op(&mut self, op: &Op<Intel386>) -> ValId {
        trace!("Emitting: {op:?}");
        let val = match *op {
            Op::BinOp {
                args,
                op,
            } => {
                let values = args.map(|val| self.emit_val(val, false));
                self.core.builder.value_tree_builder.binop(op, values)
            },
            Op::FpBinOp {
                args,
                rc,
                op,
            } => {
                let values = args.map(|val| self.emit_val(val, false));
                let rc = self.emit_val(rc, false);
                self.core.builder.value_tree_builder.fp_binop(op, values, rc)
            },
            Op::FpUnOp {
                arg,
                rc,
                op,
            } => {
                let value = self.emit_val(arg, false);
                let rc = self.emit_val(rc, false);
                self.core.builder.value_tree_builder.fp_unop(op, value, rc)
            },
            Op::UnOp {
                arg,
                op,
            } => {
                let value = self.emit_val(arg, false);
                if op == UnOp::Id {
                    value
                } else {
                    self.core.builder.value_tree_builder.unop(op, value)
                }
            },
            Op::Ite {
                cond,
                if_nonzero,
                if_zero,
            } => {
                let cond = self.emit_val(cond, false);
                let if_nonzero = self.emit_val(if_nonzero, false);
                let if_zero = self.emit_val(if_zero, false);
                self.core.builder.value_tree_builder.ite(cond, if_zero, if_nonzero)
            },
        };

        trace!("Emitted {val:?} = {op:?}");
        val
    }

    fn emit_val(&mut self, val: Val<Intel386>, apply_memory_imm_transformation: bool) -> ValId {
        match val {
            Val::Temp(n) => self.tmp[n],
            Val::Loc(par_loc) => {
                let val = match self.instantiate_dest(par_loc.loc, apply_memory_imm_transformation) {
                    Loc(UnsizedLoc::Reg(reg)) => {
                        if reg.is_zero() {
                            self.core.builder.value_tree_builder.imm(0)
                        } else {
                            self.core.emit_reg(reg)
                        }
                    },
                    Loc(UnsizedLoc::Memory(index)) => self.memory[index].value,
                    Part(index) => self.emit_part(index, apply_memory_imm_transformation),
                    InstrLen => self.emit_instr_len(),
                    Const(val) => self.core.builder.value_tree_builder.imm(val),
                };

                self.core.builder.value_tree_builder.extract(
                    val,
                    par_loc.size.start_byte() as u8 * 8,
                    par_loc.size.num_bytes() as u8 * 8,
                )
            },
            Val::Conv {
                loc,
                source_bits,
                target_bits,
                sign_extend,
                swap_endianness,
            } => {
                let mut val = self.emit_val(Val::Loc(loc), false);
                if swap_endianness {
                    match source_bits {
                        8 => (),
                        16 => val = self.core.builder.value_tree_builder.unop(UnOp::ByteSwap16, val),
                        32 => val = self.core.builder.value_tree_builder.unop(UnOp::ByteSwap32, val),
                        64 => val = self.core.builder.value_tree_builder.unop(UnOp::ByteSwap64, val),
                        n if n < 64 => {
                            val = self.core.builder.value_tree_builder.unop(UnOp::ByteSwap64, val);
                            let shift = self.core.builder.value_tree_builder.imm(((64 - n as u128) / 8) * 8);
                            val = self.core.builder.value_tree_builder.binop(BinOp::Shr, [val, shift]);
                        },
                        _ => panic!("todo: swap bytes for {source_bits} bits"),
                    }
                }

                if sign_extend {
                    val = self
                        .core
                        .builder
                        .value_tree_builder
                        .unop(UnOp::SignExtend(source_bits.try_into().unwrap()), val);
                }

                let mask = self.core.builder.value_tree_builder.imm(bitmask_u128(target_bits as u32));
                self.core.builder.value_tree_builder.binop(BinOp::And, [val, mask])
            },
        }
    }

    fn emit_instr_len(&mut self) -> ValId {
        match self.instr_len {
            Some(n) => self.core.builder.value_tree_builder.imm(n as u128),
            None => self.core.builder.value_tree_builder.instr_len(),
        }
    }

    fn emit_part(&mut self, index: usize, apply_memory_imm_transformation: bool) -> ValId {
        match &self.encoding.parts[index].mapping {
            PartMapping::Imm {
                mapping,
                bits,
            } => {
                assert!(bits.is_none());

                let part_values = self.core.builder.value_tree_builder.part_values();
                let packing = &self.encoding.semantics.part_packing[index];
                let part_val = self
                    .core
                    .builder
                    .value_tree_builder
                    .extract(part_values, packing.offset(), packing.len());
                if apply_memory_imm_transformation && let Some(MappingOrBitOrder::BitOrder(order)) = mapping {
                    let Some((signed, negative, little_endian)) = [
                        (false, false, false),
                        (false, false, true),
                        (true, false, false),
                        (true, false, true),
                        (false, true, false),
                        (false, true, true),
                        (true, true, false),
                        (true, true, true),
                    ]
                    .into_iter()
                    .find(|&(signed, negative, little_endian)| {
                        order
                            .iter()
                            .enumerate()
                            .map(|(index, bit)| {
                                if little_endian {
                                    let byte_index = (order.len() - 1 - index) / 8;
                                    (byte_index * 8 + (index & 7), bit)
                                } else {
                                    (index, bit)
                                }
                            })
                            .all(|(index, &bit)| {
                                bit == if signed && index == order.len() - 1 {
                                    if negative {
                                        ImmBitOrder::Positive(index)
                                    } else {
                                        ImmBitOrder::Negative(index)
                                    }
                                } else {
                                    if negative {
                                        ImmBitOrder::Negative(index)
                                    } else {
                                        ImmBitOrder::Positive(index)
                                    }
                                }
                            })
                    }) else {
                        panic!("We only support signed/unsigned and little-endian/big-endian in the bitorder: {order:?}");
                    };

                    trace!("Convert {order:?} (signed={signed}, negative={negative}, little endian={little_endian})");

                    let val = self.core.builder.value_tree_builder.extract(part_val, 0, order.len() as u8);
                    let val = if little_endian {
                        match order.len() {
                            8 => val,
                            16 => self.core.builder.value_tree_builder.unop(UnOp::ByteSwap16, val),
                            32 => self.core.builder.value_tree_builder.unop(UnOp::ByteSwap32, val),
                            64 => self.core.builder.value_tree_builder.unop(UnOp::ByteSwap64, val),
                            _ => panic!("unsupported memory imm size: {}", order.len()),
                        }
                    } else {
                        val
                    };

                    let val = if signed {
                        self.core
                            .builder
                            .value_tree_builder
                            .unop(UnOp::SignExtend(order.len() as u8), val)
                    } else {
                        val
                    };

                    if negative {
                        let zero = self.core.builder.value_tree_builder.imm(0);
                        self.core.builder.value_tree_builder.binop(BinOp::Sub, [zero, val])
                    } else {
                        val
                    }
                } else {
                    part_val
                }
            },
            PartMapping::MemoryComputation {
                ..
            } => unreachable!("cannot load memory computation as value"),
            PartMapping::Register {
                ..
            } => self.core.builder.with_uncommitted_cpu_state(|value_tree_builder, state| {
                state.get_or_compute_part(index, value_tree_builder, &self.encoding)
            }),
        }
    }

    fn emit_store(&mut self, to: &Val<Intel386>, new_val: ValId) {
        trace!("Storing {new_val:?} in {to:?}");
        match *to {
            Val::Temp(n) => self.tmp[n] = new_val,
            Val::Loc(par_loc) => match self.instantiate_dest(par_loc.loc, false) {
                Loc(UnsizedLoc::Reg(reg)) => {
                    let old_val = self.core.emit_reg(reg);
                    let combined = self
                        .core
                        .builder
                        .value_tree_builder
                        .combine_old_and_new(old_val, new_val, par_loc.size);

                    self.core
                        .builder
                        .with_uncommitted_cpu_state(|value_tree_builder, state| state.store(reg, value_tree_builder, combined));
                },
                Loc(UnsizedLoc::Memory(index)) => {
                    let old_val = self.memory[index].value;
                    let combined = self
                        .core
                        .builder
                        .value_tree_builder
                        .combine_old_and_new(old_val, new_val, par_loc.size);
                    self.memory[index].value = combined;
                    self.memory[index].needs_write = true;
                },
                Part(index) => match &self.encoding.parts[index].mapping {
                    PartMapping::Imm {
                        ..
                    } => unreachable!("cannot store into immediate"),
                    PartMapping::MemoryComputation {
                        ..
                    } => unreachable!("cannot store into memory computation"),
                    PartMapping::Register {
                        ..
                    } => {
                        let old_val = self.emit_part(index, false);
                        let combined = self
                            .core
                            .builder
                            .value_tree_builder
                            .combine_old_and_new(old_val, new_val, par_loc.size);
                        self.core.builder.with_uncommitted_cpu_state(|value_tree_builder, state| {
                            state.store_part(index, value_tree_builder, &self.encoding, combined)
                        });
                    },
                },
                InstrLen => panic!("cannot write into instruction length"),
                Const(val) => panic!("cannot write into constant value {val}"),
            },
            Val::Conv {
                ..
            } => unreachable!(),
        }
    }

    fn increment_k(&mut self) {
        self.core
            .builder
            .with_uncommitted_cpu_state(|value_tree_builder, state| state.increment_k(value_tree_builder));
    }

    fn emit_jump(&mut self) -> Option<ValId> {
        let (condition, new_val, clear_upper) = match self.encoding.semantics.jump {
            Jump::Sequential => {
                let ip = self.core.emit_reg(Intel386::PC.into());
                let instr_len = self.emit_instr_len();
                (
                    None,
                    self.core.builder.value_tree_builder.binop(BinOp::Add, [ip, instr_len]),
                    false,
                )
            },
            Jump::Repeat {
                condition,
            } => {
                let condition = self.emit_val(*condition, false);

                let ip = self.core.emit_reg(Intel386::PC.into());
                let instr_len = self.emit_instr_len();
                let next_instr = self.core.builder.value_tree_builder.binop(BinOp::Add, [ip, instr_len]);

                (
                    Some(condition),
                    self.core.builder.value_tree_builder.ite(condition, next_instr, ip),
                    false,
                )
            },
            Jump::NearRelativeOffset {
                condition,
                offset,
            } => {
                let condition = self.emit_val(*condition, false);

                let offset = offset
                    .iter()
                    .map(|val| self.emit_val(*val, false))
                    .collect::<Vec<_>>()
                    .into_iter()
                    .reduce(|a, b| self.core.builder.value_tree_builder.binop(BinOp::Add, [a, b]))
                    .unwrap();

                let ip = self.core.emit_reg(Intel386::PC.into());
                let instr_len = self.emit_instr_len();
                let next_instr_false = self.core.builder.value_tree_builder.binop(BinOp::Add, [ip, instr_len]);
                let next_instr_true = self
                    .core
                    .builder
                    .value_tree_builder
                    .binop(BinOp::Add, [next_instr_false, offset]);

                (
                    Some(condition),
                    self.core
                        .builder
                        .value_tree_builder
                        .ite(condition, next_instr_false, next_instr_true),
                    true,
                )
            },
            Jump::NearAbsolute(val) => (None, self.emit_val(*val, false), true),
            Jump::Far => return None,
        };

        let new_val = self
            .core
            .builder
            .value_tree_builder
            .extract(new_val, 0, if self.is_cs32 { 32 } else { 16 });

        self.emit_store(
            &Val::Loc(ParLoc {
                loc: UnsizedParLoc::Reg(GpReg::Ip.into()),
                size: Size::from_bytes(if self.is_cs32 || clear_upper { 4 } else { 2 }),
            }),
            new_val,
        );

        condition
    }
}

fn compute_const_scale_offset(mapping: &[Option<Reg>]) -> Option<(usize, i64)> {
    (1..64).find_map(|scale| {
        mapping
            .iter()
            .enumerate()
            .flat_map(|(index, reg)| reg.map(|reg| (index * scale, State::byte_offset_of(reg))))
            .map(|(a, b)| Some(b as i64 - a as i64))
            .reduce(|a, b| if a == b { a } else { None })
            .unwrap()
            .map(|offset| (scale, offset))
    })
}

struct MemEntryBackup {
    value: ValId,
    needs_write: bool,
}

struct Backup {
    cpu_state: UncommittedState,
    tmp: [ValId; 256],
    memory: ArrayVec<MemEntryBackup, 16>,
}

impl Backup {
    pub fn make(state: &mut EncodingEmitter) -> Self {
        Self {
            cpu_state: state.core.builder.uncommitted_cpu_state().clone(),
            tmp: state.tmp,
            memory: state
                .memory
                .iter()
                .map(|m| MemEntryBackup {
                    value: m.value,
                    needs_write: m.needs_write,
                })
                .collect(),
        }
    }

    pub fn restore(self, state: &mut EncodingEmitter) {
        *state.core.builder.uncommitted_cpu_state() = self.cpu_state;
        state.tmp = self.tmp;
        for (mem, backup) in state.memory.iter_mut().zip(self.memory.iter()) {
            mem.value = backup.value;
            mem.needs_write = backup.needs_write;
        }
    }

    pub fn merge(
        after_if_zero: Self, after_if_nonzero: Self, state: &mut EncodingEmitter,
    ) -> (Vec<(VarId, ValId)>, Vec<(VarId, ValId)>) {
        let (uc, mut if_zero_updates, mut if_nonzero_updates) = UncommittedState::merge(
            after_if_zero.cpu_state,
            after_if_nonzero.cpu_state,
            &mut state.core.builder.value_tree_builder,
            || state.core.builder.vars.alloc_var(),
        );

        *state.core.builder.uncommitted_cpu_state() = uc;

        // Unify temporary variables
        for (tmp, (&if_zero_val, &if_nonzero_val)) in state
            .tmp
            .iter_mut()
            .zip(after_if_zero.tmp.iter().zip(after_if_nonzero.tmp.iter()))
        {
            if if_zero_val != if_nonzero_val {
                let var = state.core.builder.vars.alloc_var();
                if_zero_updates.push((var, if_zero_val));
                if_nonzero_updates.push((var, if_nonzero_val));
                *tmp = state.core.builder.value_tree_builder.use_var(var);
            }
        }

        // Unify memory
        for (mem, (if_zero_mem, if_nonzero_mem)) in state
            .memory
            .iter_mut()
            .zip(after_if_zero.memory.iter().zip(after_if_nonzero.memory.iter()))
        {
            if if_zero_mem.value != if_nonzero_mem.value {
                let var = state.core.builder.vars.alloc_var();
                if_zero_updates.push((var, if_zero_mem.value));
                if_nonzero_updates.push((var, if_nonzero_mem.value));
                mem.value = state.core.builder.value_tree_builder.use_var(var);
            }

            mem.needs_write = if_zero_mem.needs_write || if_nonzero_mem.needs_write;
        }

        (if_zero_updates, if_nonzero_updates)
    }
}
