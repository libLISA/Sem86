pub mod dominator;

use std::collections::HashMap;
use std::fmt::Debug;
use std::iter::once;
use std::ops::Index;

use itertools::Itertools;
use liblisa::arch::Register;
use liblisa::utils::bitmap::GrowingBitmap;
use log::{debug, trace};
use sem86_arch::exceptions::Exception;
use serde::{Deserialize, Serialize};

use super::mir::val::ValNode;
use crate::arch::intel386::{HandlerId, State};
use crate::codegen::components::StronglyConnectedComponents;
use crate::codegen::lir::dominator::DominatorTree;
use crate::codegen::mir::Mir;
use crate::codegen::mir::bb::{Bb, BbFallible, BbId, BbSeq, CommitDest};
use crate::codegen::mir::val::{ValId, VarId};
use crate::codegen::{DataSize, Ptr};
use crate::il::{BinOp, FpBinOp, FpUnOp, UnOp};

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct BlockId(u32);

impl BlockId {
    pub const ROOT: BlockId = BlockId(0);

    pub fn from_usize(n: usize) -> Self {
        Self(n.try_into().unwrap())
    }

    pub fn index(&self) -> usize {
        self.0 as usize
    }
}

impl Debug for BlockId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "!{}", self.0)
    }
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ConstId(u32);

impl ConstId {
    pub fn from_usize(n: usize) -> Self {
        Self(n.try_into().unwrap())
    }

    pub fn index(&self) -> usize {
        self.0 as usize
    }
}

impl Debug for ConstId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "c{}", self.0)
    }
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DefId(u32);

impl DefId {
    pub fn from_usize(n: usize) -> Self {
        Self(n.try_into().unwrap())
    }

    pub fn index(&self) -> usize {
        self.0 as usize
    }
}

impl Debug for DefId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "def.{}", self.0)
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LirOp {
    /// Pushes constant onto stack.
    Const(ConstId),

    /// Pushes value of variable onto stack.
    Load(DefId),

    /// Pops value from stack and stores it in variable.
    Store(DefId),

    /// Pops two values from stack, applies op, and pushes result.
    BinOp(BinOp),

    /// Pops [lhs, rhs, rc] from stack, applies op, and pushes result.
    FpBinOp(FpBinOp),

    /// Pops [val, rc] from stack, applies op, and pushes result.
    FpUnOp(FpUnOp),

    /// Pops [lhs, rhs] from stack, and blends values according to mask.
    /// Each bit in the mask selects the corresponding bit from lhs (if 0), or rhs (if 1).
    Blend(ConstId),

    /// Pops [val] from stack, then computes `(val >> skip) & bitmask_u128(take)`.
    Extract { skip: u8, take: u8 },

    /// Pops one value from stack, applies op, and pushes result.
    UnOp(UnOp),

    /// Pops [if_nonzero, if_zero, condition] from stack.
    /// Pushes if_zero/if_nonzero depending on value of condition.
    Ite,

    /// Pops offset from stack.
    /// Pushes loaded value.
    LoadPtrWithOffset { ptr: Ptr, size: DataSize },

    /// Pushes loaded value.
    LoadPtrImm { ptr: Ptr, size: DataSize, offset: u16 },

    /// Pops `[offset, value]` from stack.
    /// Writes `value` to memory at the specified offset from the pointer.
    StorePtrWithOffset { ptr: Ptr, size: DataSize },

    /// Pops `value` from stack.
    /// Writes `value` to memory at the specified offset from the pointer.
    StorePtrImm { ptr: Ptr, size: DataSize, offset: u16 },

    /// Pops code from stack.
    /// Sets failure return value to exception.
    SetExceptionWithCode { exception: Exception },

    /// Pops [arg0, arg1] from stack.
    /// Sets failure return value to handler invocation.
    SetHandler { id: HandlerId },

    /// Pops [addr] from stack, pushes [successful, value].
    ReadMemory { num_bytes: u8 },

    /// Pops [addr, value] from stack, pushes successful.
    WriteMemory { num_bytes: u8 },

    /// Pops [port, data] from stack, pushes successful.
    PortOut { len: u8 },

    /// Pops port from stack, pushes [successful, value].
    PortIn { len: u8 },

    /// Pops selector from stack, pushes [successful, ok, base, limit, access_rights]
    ReadDescriptor { force: bool, mark_accessed: bool },

    /// Push the instruction length on the stack.
    InstrLen,

    /// Push the packed part values
    PartValues,
}

impl Debug for LirOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LirOp::Const(c) => write!(f, "push {c:?}"),
            LirOp::Load(def_id) => write!(f, "load {def_id:?}"),
            LirOp::Store(def_id) => write!(f, "store {def_id:?}"),
            LirOp::BinOp(bin_op) => write!(f, "{bin_op:?}"),
            LirOp::FpBinOp(bin_op) => write!(f, "{bin_op:?}"),
            LirOp::FpUnOp(un_op) => write!(f, "{un_op:?}"),
            LirOp::UnOp(un_op) => write!(f, "{un_op:?}"),
            LirOp::Ite => write!(f, "ite"),
            LirOp::LoadPtrWithOffset {
                ptr,
                size,
            } => write!(f, "load_ptr.{size:?} {ptr:?}"),
            LirOp::LoadPtrImm {
                ptr,
                size,
                offset,
            } => write!(f, "load_ptr.{size:?} {ptr:?}+0x{offset:X} ({})", ptr.ptr_hint(*offset)),
            LirOp::StorePtrWithOffset {
                ptr,
                size,
            } => write!(f, "store_ptr.{size:?} {ptr:?}"),
            LirOp::StorePtrImm {
                ptr,
                size,
                offset,
            } => write!(f, "store_ptr.{size:?} {ptr:?}+0x{offset:X} ({})", ptr.ptr_hint(*offset)),
            LirOp::SetExceptionWithCode {
                exception,
            } => write!(f, "set_exception {exception:?}"),
            LirOp::SetHandler {
                id,
            } => write!(f, "set_handler {id:?}"),
            LirOp::ReadMemory {
                num_bytes,
            } => write!(f, "read_bytes {num_bytes}"),
            LirOp::WriteMemory {
                num_bytes,
            } => write!(f, "write_bytes {num_bytes} "),
            LirOp::PortOut {
                len,
            } => write!(f, "out {len}"),
            LirOp::PortIn {
                len,
            } => write!(f, "in {len}"),
            LirOp::ReadDescriptor {
                force,
                mark_accessed,
            } => write!(f, "read_descriptor force={force}, mark_accessed={mark_accessed}"),
            LirOp::Blend(c) => write!(f, "blend {c:?}"),
            LirOp::Extract {
                skip,
                take,
            } => write!(f, "extract {skip}:{}", skip + take),
            LirOp::InstrLen => write!(f, "push $instr_len"),
            LirOp::PartValues => write!(f, "push $part_values"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum Jump {
    Next(BlockId),
    Cond {
        if_zero: BlockId,
        if_nonzero: BlockId,
    },
    Exit {
        success: bool,
        metadata: Option<u64>,
        with_last_jump_condition: bool,
    },
    #[default]
    Unreachable,
}

impl Jump {
    pub fn iter(&self) -> impl Iterator<Item = &BlockId> {
        match self {
            Jump::Next(block_id) => vec![block_id],
            Jump::Cond {
                if_zero,
                if_nonzero,
            } => vec![if_zero, if_nonzero],
            Jump::Exit {
                ..
            }
            | Jump::Unreachable => vec![],
        }
        .into_iter()
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut BlockId> {
        match self {
            Jump::Next(block_id) => vec![block_id],
            Jump::Cond {
                if_zero,
                if_nonzero,
            } => vec![if_zero, if_nonzero],
            Jump::Exit {
                ..
            }
            | Jump::Unreachable => vec![],
        }
        .into_iter()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LirBlock {
    operations: Vec<LirOp>,
    next: Jump,
}

impl LirBlock {
    pub fn new(operations: Vec<LirOp>, next: Jump) -> Self {
        Self {
            operations,
            next,
        }
    }

    pub fn operations(&self) -> &[LirOp] {
        &self.operations
    }

    pub fn next(&self) -> &Jump {
        &self.next
    }
}

struct DebugLirBlock<'a> {
    lir: &'a Lir,
    block: &'a LirBlock,
}

struct ConstOp {
    op: &'static str,
    val: u128,
}

impl Debug for ConstOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} 0x{:X}", self.op, self.val)
    }
}

impl Debug for DebugLirBlock<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut f = f.debug_list();
        for op in self.block.operations.iter() {
            match op {
                LirOp::Const(c) => {
                    f.entry(&ConstOp {
                        op: "push",
                        val: self.lir[*c],
                    });
                },
                LirOp::Blend(c) => {
                    f.entry(&ConstOp {
                        op: "blend",
                        val: self.lir[*c],
                    });
                },
                _ => {
                    f.entry(op);
                },
            }
        }

        f.entry(&self.block.next);
        f.finish()
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Lir {
    pub(super) consts: Vec<u128>,
    pub(super) blocks: Vec<LirBlock>,
    num_defs: usize,
}

impl Debug for Lir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut identical_node_map = HashMap::new();
        for (index, item) in self.blocks.iter().enumerate() {
            identical_node_map
                .entry(item)
                .or_insert_with(Vec::new)
                .push(BlockId::from_usize(index));
        }

        let mut f = f.debug_map();
        // for (index, c) in self.consts.iter().enumerate() {
        //     f.entry(&ConstId::from_usize(index), &HexVal(*c));
        // }

        let mut seen = GrowingBitmap::new_all_zeros(self.blocks.len());
        let mut frontier = (0..self.blocks.len()).rev().map(BlockId::from_usize).collect::<Vec<_>>();
        while let Some(id) = frontier.pop() {
            if seen.set(id.index()) {
                let bb = &self.blocks[id.index()];
                let ids = &identical_node_map[bb];
                let debug_bb = DebugLirBlock {
                    lir: self,
                    block: bb,
                };

                if ids.len() > 1 {
                    for id in ids.iter() {
                        seen.set(id.index());
                    }

                    f.entry(&ids, &debug_bb);
                } else {
                    f.entry(&id, &debug_bb);
                }

                frontier.extend(bb.next().iter().copied());
            }
        }

        f.finish()
    }
}

impl Index<BlockId> for Lir {
    type Output = LirBlock;

    fn index(&self, index: BlockId) -> &Self::Output {
        &self.blocks[index.index()]
    }
}

impl Index<ConstId> for Lir {
    type Output = u128;

    fn index(&self, index: ConstId) -> &Self::Output {
        &self.consts[index.index()]
    }
}

impl Lir {
    pub fn new(consts: Vec<u128>, blocks: Vec<LirBlock>, num_defs: usize) -> Self {
        Self {
            consts,
            blocks,
            num_defs,
        }
    }

    /// Returns the maximum number of operations this LIR could execute in one execution.
    /// In other words, returns the maximum depth in number of operations.
    pub fn max_ops_executed(&self) -> usize {
        let mut depth = vec![0; self.blocks.len()];
        StronglyConnectedComponents::iterate(self, |nodes| {
            assert_eq!(nodes.len(), 1, "loops not supported");
            for &node in nodes {
                let block = &self.blocks[node.index()];
                depth[node.index()] = block.operations.len() + block.next.iter().map(|n| depth[n.index()]).max().unwrap_or(0);
            }
        });

        depth[BlockId::ROOT.index()]
    }

    pub fn optimize(&mut self) {
        self.merge_identical_blocks();

        // Concatenate unconditional jumps
        let mut changed = false;
        let mut num_entries = vec![0; self.blocks.len()];
        num_entries[0] = 1;
        for block in self.blocks.iter() {
            for block in block.next.iter() {
                num_entries[block.index()] += 1;
            }
        }

        for index in 0..self.blocks.len() {
            if let Jump::Next(next) = self.blocks[index].next
                && num_entries[next.index()] == 1
            {
                let [lhs, rhs] = self.blocks.get_disjoint_mut([index, next.index()]).unwrap();
                lhs.operations.append(&mut rhs.operations);
                lhs.next = rhs.next.clone();
                *rhs = Default::default();
                changed = true;
            }
        }

        self.prune_unreachable_blocks();

        if changed {
            self.merge_identical_blocks();
            self.prune_unreachable_blocks();
        }
    }

    fn merge_identical_blocks(&mut self) -> bool {
        let mut changed = false;

        // We first detect candidates that can be potentially equal based on the `next` field.
        let mut candidates = HashMap::with_capacity(self.blocks.len());
        for (index, block) in self.blocks.iter().enumerate() {
            candidates.entry(block.next()).or_insert_with(Vec::new).push(index);
        }

        // For every set of candidates that has the same `next` field,
        // we split the candidates into groups where the entire block is equivalent.
        let mut remap = (0..self.blocks.len()).map(BlockId::from_usize).collect::<Vec<_>>();
        for set in candidates.into_values() {
            if set.len() > 1 {
                let mut groups = HashMap::with_capacity(set.len());
                for &index in set.iter() {
                    groups.entry(&self.blocks[index]).or_insert_with(Vec::new).push(index);
                }

                for group in groups.into_values() {
                    if group.len() > 1 {
                        changed = true;
                        for &index in group[1..].iter() {
                            remap[index] = BlockId::from_usize(group[0]);
                        }
                    }
                }
            }
        }

        if changed {
            for (index, block) in self.blocks.iter_mut().enumerate() {
                if remap[index].index() != index {
                    *block = Default::default();
                } else {
                    for next in block.next.iter_mut() {
                        *next = remap[next.index()];
                    }
                }
            }
        }

        changed
    }

    pub fn reuse_defs(&mut self) {}

    fn prune_unreachable_blocks(&mut self) {
        let mut n = 0;
        let mut index = 0;
        let mut block_remap = vec![0; self.blocks.len()];
        self.blocks.retain(|block| {
            let keep = !matches!(block.next, Jump::Unreachable);
            if keep {
                block_remap[index] = n;
                n += 1;
            }

            index += 1;
            keep
        });

        for block in self.blocks.iter_mut() {
            for block in block.next.iter_mut() {
                *block = BlockId::from_usize(block_remap[block.index()]);
            }
        }
    }

    pub fn num_ops(&self) -> usize {
        self.blocks.iter().map(|b| b.operations.len() + 1).sum()
    }

    pub fn num_defs(&self) -> usize {
        self.num_defs
    }

    pub fn get(&self, current_block: BlockId) -> Option<&LirBlock> {
        self.blocks.get(current_block.index())
    }

    pub fn get_unassigned_defs(&self) -> Vec<DefId> {
        let mut store_requirements = vec![Vec::new(); self.blocks.len()];
        StronglyConnectedComponents::iterate(self, |blocks| {
            assert!(blocks.len() == 1, "loops not supported");
            let id = blocks[0];
            let block = &self[id];
            let mut requirements = block
                .next
                .iter()
                .flat_map(|id| store_requirements[id.index()].iter().copied())
                .collect::<Vec<_>>();

            requirements.sort();
            requirements.dedup();

            for op in block.operations.iter().rev() {
                match *op {
                    LirOp::Load(def_id) => {
                        if !requirements.contains(&def_id) {
                            requirements.push(def_id);
                        }
                    },
                    LirOp::Store(def_id) => requirements.retain(|&id| id != def_id),
                    _ => (),
                }
            }

            store_requirements[id.index()] = requirements;
        });

        store_requirements.remove(BlockId::ROOT.index())
    }

    pub fn performs_io(&self) -> bool {
        for block in self.blocks.iter() {
            for op in block.operations() {
                if matches!(
                    op,
                    LirOp::ReadMemory { .. }
                        | LirOp::WriteMemory { .. }
                        | LirOp::PortOut { .. }
                        | LirOp::PortIn { .. }
                        | LirOp::ReadDescriptor { .. }
                ) {
                    return true
                }
            }
        }

        false
    }

    pub fn reads_memory(&self) -> bool {
        for block in self.blocks.iter() {
            for op in block.operations() {
                if let LirOp::ReadMemory {
                    ..
                } = op
                {
                    return true
                }
            }
        }

        false
    }

    pub fn reads_descriptors(&self) -> bool {
        for block in self.blocks.iter() {
            for op in block.operations() {
                if let LirOp::ReadDescriptor {
                    ..
                } = op
                {
                    return true
                }
            }
        }

        false
    }
}

impl crate::codegen::graph_traits::Graph for Lir {
    type Index = BlockId;
    type Node = LirBlock;
    const ROOT: Self::Index = BlockId::ROOT;

    fn num_nodes(&self) -> usize {
        self.blocks.len()
    }

    fn node(&self, index: Self::Index) -> &Self::Node {
        &self.blocks[index.index()]
    }
}

impl crate::codegen::graph_traits::Index for BlockId {
    fn index(&self) -> usize {
        self.index()
    }

    fn from_usize(val: usize) -> Self {
        Self::from_usize(val)
    }
}

impl crate::codegen::graph_traits::Node<BlockId> for LirBlock {
    fn transitions(&self) -> impl Iterator<Item = BlockId> {
        self.next.iter().copied()
    }
}

pub struct LirBuilder {
    consts: Vec<u128>,
    const_map: HashMap<u128, ConstId>,
    blocks: Vec<LirBlock>,
    current_block: BlockId,
    next_def_id: usize,
}

impl Default for LirBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl LirBuilder {
    pub fn new() -> Self {
        Self {
            blocks: vec![LirBlock::default()],
            consts: Vec::new(),
            const_map: HashMap::new(),
            current_block: BlockId::from_usize(0),
            next_def_id: 0,
        }
    }

    pub fn imm(&mut self, val: u128) -> ConstId {
        *self.const_map.entry(val).or_insert_with(|| {
            let id = ConstId::from_usize(self.consts.len());
            self.consts.push(val);
            id
        })
    }

    pub fn define_var(&mut self) -> DefId {
        let id = DefId::from_usize(self.next_def_id);
        self.next_def_id += 1;
        id
    }

    pub fn emit(&mut self, op: LirOp) {
        assert!(
            matches!(self.blocks[self.current_block.index()].next, Jump::Unreachable),
            "cannot emit to sealed block"
        );
        self.blocks[self.current_block.index()].operations.push(op);
    }

    pub fn emit_many(&mut self, ops: impl IntoIterator<Item = LirOp>) {
        assert!(
            matches!(self.blocks[self.current_block.index()].next, Jump::Unreachable),
            "cannot emit to sealed block"
        );
        self.blocks[self.current_block.index()].operations.extend(ops);
    }

    pub fn seal_block(&mut self, next: Jump) {
        assert!(
            matches!(self.blocks[self.current_block.index()].next, Jump::Unreachable),
            "cannot seal block twice"
        );
        self.blocks[self.current_block.index()].next = next;
    }

    pub fn switch_to(&mut self, next: BlockId) {
        self.current_block = next;
    }

    pub fn seal_and_switch_to(&mut self, jump: Jump, next: BlockId) {
        self.seal_block(jump);
        self.switch_to(next);
    }

    pub fn create_block(&mut self) -> BlockId {
        let id = BlockId::from_usize(self.blocks.len());
        self.blocks.push(LirBlock::default());
        id
    }

    pub fn build(self) -> Lir {
        let mut lir = Lir {
            consts: self.consts,
            blocks: self.blocks,
            num_defs: self.next_def_id,
        };
        lir.optimize();
        lir
    }
}

pub struct MirToLir<'mir> {
    mir: &'mir Mir,
    builder: LirBuilder,
    value_defs: Vec<Option<DefId>>,
    var_defs: HashMap<VarId, DefId>,
}

impl<'mir> MirToLir<'mir> {
    pub fn new(mir: &'mir Mir) -> Self {
        trace!("Lowering MIR to LIR:\n{mir}");
        Self {
            mir,
            builder: LirBuilder::new(),
            value_defs: vec![None; mir.value_tree.len()],
            var_defs: HashMap::new(),
        }
    }

    fn emit_value(&mut self, val: ValId) {
        if let Some(def_id) = self.value_defs[val.index()] {
            self.builder.emit(LirOp::Load(def_id));
        } else {
            self.emit_value_nodef(val);
        }
    }

    fn emit_value_nodef(&mut self, val: ValId) {
        match self.mir.value_tree[val] {
            ValNode::Const(n) => {
                let id = self.builder.imm(n);
                self.builder.emit(LirOp::Const(id))
            },
            ValNode::Var(var_id) => {
                let def_id = self.get_var(&var_id);
                self.builder.emit(LirOp::Load(def_id))
            },
            ValNode::BinOp {
                args,
                op,
            } => {
                for arg in args {
                    self.emit_value(arg);
                }

                self.builder.emit(LirOp::BinOp(op));
            },
            ValNode::FpBinOp {
                args,
                rc,
                op,
            } => {
                for arg in args {
                    self.emit_value(arg);
                }

                self.emit_value(rc);
                self.builder.emit(LirOp::FpBinOp(op));
            },
            ValNode::FpUnOp {
                arg,
                rc,
                op,
            } => {
                self.emit_value(arg);
                self.emit_value(rc);
                self.builder.emit(LirOp::FpUnOp(op));
            },
            ValNode::Blend {
                lhs,
                rhs,
                mask,
            } => {
                self.emit_value(lhs);
                self.emit_value(rhs);
                let id = self.builder.imm(mask);
                self.builder.emit(LirOp::Blend(id));
            },
            ValNode::Extract {
                val,
                skip,
                take,
            } => {
                self.emit_value(val);
                self.builder.emit(LirOp::Extract {
                    skip,
                    take,
                });
            },
            ValNode::UnOp {
                arg,
                op,
            } => {
                self.emit_value(arg);
                self.builder.emit(LirOp::UnOp(op));
            },
            ValNode::Ite {
                cond,
                if_zero,
                if_nonzero,
            } => {
                self.emit_value(if_nonzero);
                self.emit_value(if_zero);
                self.emit_value(cond);
                self.builder.emit(LirOp::Ite);
            },
            ValNode::LoadPtr {
                ptr,
                offset,
                size,
            } => {
                self.emit_value(offset);
                self.builder.emit(LirOp::LoadPtrWithOffset {
                    ptr,
                    size,
                });
            },
            ValNode::LoadPtrImm {
                ptr,
                offset,
                size,
            } => {
                self.builder.emit(LirOp::LoadPtrImm {
                    ptr,
                    size,
                    offset,
                });
            },
            ValNode::InstrLen => self.builder.emit(LirOp::InstrLen),
            ValNode::PartValues => self.builder.emit(LirOp::PartValues),
        }
    }

    fn define_value(&mut self, val: ValId) {
        self.emit_value_nodef(val);

        let var = self.value_defs[val.index()].unwrap();

        debug!("Storing value {val:?} in definition {var:?}");
        self.builder.emit(LirOp::Store(var));
    }

    fn compute_bb_reachability(&self) -> GrowingBitmap {
        let mut seen = GrowingBitmap::new_all_zeros(self.mir.control_flow.len());
        let mut frontier = vec![BbId::ROOT];
        seen.set(BbId::ROOT.index());

        while let Some(id) = frontier.pop() {
            for &child in self.mir.control_flow[id].next_blocks() {
                if seen.set(child.index()) {
                    frontier.push(child);
                }
            }
        }

        seen
    }

    pub fn build(mut self) -> Lir {
        let mut val_value_usage = vec![Vec::<ValId>::new(); self.mir.value_tree.len()];
        let mut val_costs = vec![0; self.mir.value_tree.len()];
        let val_num_references = {
            let mut v = vec![0; self.mir.value_tree.len()];
            let mut bb_frontier = vec![BbId::ROOT];
            let mut val_frontier = Vec::new();
            let mut bbs_seen = GrowingBitmap::new_all_zeros(self.mir.control_flow.len());
            let mut vals_seen = GrowingBitmap::new_all_zeros(self.mir.value_tree.len());

            while let Some(bb) = bb_frontier.pop() {
                let bb = &self.mir.control_flow[bb];
                val_frontier.extend(bb.referenced_values().copied().filter(|val| {
                    v[val.index()] += 1;
                    vals_seen.set(val.index())
                }));

                bb_frontier.extend(bb.next_blocks().copied().filter(|next| bbs_seen.set(next.index())));
            }

            while let Some(val) = val_frontier.pop() {
                val_frontier.extend(self.mir.value_tree[val].referenced_nodes().copied().filter(|val| {
                    v[val.index()] += 1;
                    vals_seen.set(val.index())
                }));
            }

            v
        };

        let should_store_val = {
            let mut b = GrowingBitmap::new();
            for (id, val) in self.mir.value_tree.iter() {
                let num = val_num_references[id.index()];
                debug!("- {id:?}: {val:X?} is referenced {num}x");
                if num > 1 {
                    b.set(id.index());
                }
            }

            b
        };

        let mut reverse_topological_order = Vec::new();
        StronglyConnectedComponents::iterate(&self.mir.control_flow, |nodes| {
            assert_eq!(nodes.len(), 1, "TODO: support loops");
            let node = nodes[0];
            reverse_topological_order.push(node);
        });

        let blocks = self
            .mir
            .control_flow
            .iter()
            .map(|_| self.builder.create_block())
            .collect::<Vec<_>>();
        self.builder.seal_block(Jump::Next(blocks[0]));

        // TODO: How can we optimize this?
        StronglyConnectedComponents::iterate(&self.mir.value_tree, |nodes| {
            assert_eq!(nodes.len(), 1, "TODO: support loops");
            let node = nodes[0];
            let extra_weight = match self.mir.value_tree[node] {
                ValNode::Const(_) | ValNode::InstrLen | ValNode::PartValues => 0,
                ValNode::Var(_) => 0,
                ValNode::LoadPtr {
                    ..
                }
                | ValNode::LoadPtrImm {
                    ..
                } => 10,
                _ => 1,
            };
            val_costs[node.index()] = self.mir.value_tree[node]
                .referenced_nodes()
                .map(|n| val_costs[n.index()])
                .sum::<usize>()
                + extra_weight;
            val_value_usage[node.index()] = self.mir.value_tree[node]
                .referenced_nodes()
                .flat_map(|n| val_value_usage[n.index()].iter())
                .copied()
                .chain(once(node))
                .sorted()
                .dedup()
                .collect();
        });

        let bbs_reachable = self.compute_bb_reachability();

        trace!("Value usage: {val_value_usage:?}");
        trace!("Value costs: {val_costs:?}");

        let mut val_bb_appearances = vec![Vec::<BbId>::new(); self.mir.value_tree.len()];
        for (id, bb) in self.mir.control_flow.iter() {
            if bbs_reachable[id.index()] {
                for val in bb.referenced_values() {
                    val_bb_appearances[val.index()].push(id);
                    for &child_val in val_value_usage[val.index()].iter() {
                        val_bb_appearances[child_val.index()].push(id);
                    }
                }
            }
        }

        let dt = DominatorTree::new(&self.mir.control_flow);
        let mut bb_val_defs = vec![Vec::<ValId>::new(); self.mir.control_flow.len()];
        for (val_index, bbs) in val_bb_appearances.iter().enumerate() {
            let val_id = ValId::from_usize(val_index);
            if !bbs.is_empty() && should_store_val[val_index] {
                let lca = dt.lca_many(bbs.iter().copied());
                bb_val_defs[lca.index()].push(val_id);
                trace!("{val_id:?} is used in {bbs:?} which has LCA {lca:?}");
            }
        }

        for vals in bb_val_defs.iter_mut() {
            vals.sort();
            vals.dedup();
            vals.retain(|val| val_costs[val.index()] > 1);
        }

        for val in bb_val_defs.iter().flatten() {
            self.value_defs[val.index()] = Some(self.builder.define_var());
        }

        for &id in reverse_topological_order.iter().rev() {
            let bb = &self.mir.control_flow[id];
            let block_id = blocks[id.index()];
            let vals_to_define = &bb_val_defs[id.index()];
            if !bbs_reachable[id.index()] {
                continue
            }

            trace!("Emitting BB {id:?}");
            self.builder.switch_to(block_id);

            for &val in vals_to_define {
                debug!("Defining val {val:?} in block {id:?}");
                self.define_value(val);
            }

            match bb {
                Bb::Jump(next) => self.builder.seal_block(Jump::Next(blocks[next.index()])),
                Bb::Br {
                    cond,
                    if_zero,
                    if_nonzero,
                } => {
                    self.emit_value(*cond);
                    self.builder.seal_block(Jump::Cond {
                        if_zero: blocks[if_zero.index()],
                        if_nonzero: blocks[if_nonzero.index()],
                    })
                },
                Bb::Seq {
                    entry,
                    next,
                } => {
                    match entry {
                        BbSeq::Store {
                            values,
                        } => {
                            for (var, val) in values.iter() {
                                self.emit_value(*val);
                                let var = self.get_var(var);
                                self.builder.emit(LirOp::Store(var));
                            }
                        },
                        BbSeq::ReadMemoryUnchecked {
                            ..
                        } => todo!(),
                        BbSeq::WriteMemoryUnchecked {
                            ..
                        } => todo!(),
                        BbSeq::Log {
                            ..
                        } => (),
                        BbSeq::SetException {
                            exception,
                            code,
                        } => {
                            self.emit_value(*code);
                            self.builder.emit(LirOp::SetExceptionWithCode {
                                exception: *exception,
                            });
                        },
                        BbSeq::SetHandler {
                            id,
                            args,
                        } => {
                            for arg in args.iter() {
                                self.emit_value(*arg);
                            }

                            self.builder.emit(LirOp::SetHandler {
                                id: *id,
                            });
                        },
                        BbSeq::Commit {
                            values,
                        } => self.commit(values),
                    };
                    self.builder.seal_block(Jump::Next(blocks[next.index()]));
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
                        } => {
                            self.emit_value(*port);
                            self.emit_value(*data);
                            self.builder.emit(LirOp::PortOut {
                                len: *len,
                            });
                        },
                        BbFallible::PortIn {
                            port,
                            data,
                            len,
                        } => {
                            self.emit_value(*port);
                            self.builder.emit(LirOp::PortIn {
                                len: *len,
                            });
                            let var = self.get_var(data);
                            self.builder.emit(LirOp::Store(var));
                        },
                        BbFallible::ReadMemory {
                            value,
                            addr,
                            num_bytes,
                        } => {
                            self.emit_value(*addr);
                            self.builder.emit(LirOp::ReadMemory {
                                num_bytes: *num_bytes,
                            });
                            let var = self.get_var(value);
                            self.builder.emit(LirOp::Store(var));
                        },
                        BbFallible::WriteMemory {
                            value,
                            addr,
                            num_bytes,
                        } => {
                            self.emit_value(*addr);
                            self.emit_value(*value);
                            self.builder.emit(LirOp::WriteMemory {
                                num_bytes: *num_bytes,
                            });
                        },
                        BbFallible::ReadDescriptor {
                            selector,
                            force,
                            mark_accessed,
                            ok,
                            base,
                            limit,
                            ar,
                        } => {
                            self.emit_value(*selector);
                            self.builder.emit(LirOp::ReadDescriptor {
                                force: *force,
                                mark_accessed: *mark_accessed,
                            });

                            let ok_var = self.get_var(ok);
                            let base_var = self.get_var(base);
                            let limit_var = self.get_var(limit);
                            let ar_var = self.get_var(ar);

                            self.builder.emit_many([
                                LirOp::Store(ar_var),
                                LirOp::Store(limit_var),
                                LirOp::Store(base_var),
                                LirOp::Store(ok_var),
                            ])
                        },
                    }

                    self.builder.seal_block(Jump::Cond {
                        if_zero: blocks[if_exception.index()],
                        if_nonzero: blocks[if_ok.index()],
                    });
                },
                Bb::CommitAndExit {
                    values,
                    k,
                    success,
                    metadata,
                    last_jump_condition,
                } => {
                    self.commit(values);

                    if let Some(k) = k {
                        self.emit_value(*k);
                        self.builder.emit(LirOp::StorePtrImm {
                            ptr: Ptr::K,
                            size: DataSize::Qword,
                            offset: 0,
                        })
                    }

                    if let Some(last_jump_condition) = last_jump_condition {
                        self.emit_value(*last_jump_condition);
                    }

                    self.builder.seal_block(Jump::Exit {
                        success: *success,
                        metadata: *metadata,
                        with_last_jump_condition: last_jump_condition.is_some(),
                    });
                },
            }
        }

        let lir = self.builder.build();
        let unassigned = lir.get_unassigned_defs();

        assert!(
            unassigned.is_empty(),
            "Definitions {unassigned:?} should always be set before reading in LIR: {lir:#?}\n\nMIR: {:#?}\n\nVariable assignments: {:?}",
            self.mir,
            self.var_defs
        );

        lir
    }

    fn commit(&mut self, values: &[(CommitDest, ValId)]) {
        for (dest, val) in values.iter().rev() {
            if let CommitDest::Dynamic {
                offset, ..
            } = dest
            {
                self.emit_value(*offset);
            }

            self.emit_value(*val);
        }

        for (dest, _) in values.iter() {
            match dest {
                CommitDest::Reg(reg) => self.builder.emit(LirOp::StorePtrImm {
                    ptr: Ptr::CpuState,
                    size: reg.byte_size().try_into().unwrap(),
                    offset: State::byte_offset_of(*reg).try_into().unwrap(),
                }),
                CommitDest::Fixed {
                    size,
                    offset,
                } => self.builder.emit(LirOp::StorePtrImm {
                    ptr: Ptr::CpuState,
                    size: *size,
                    offset: *offset,
                }),
                CommitDest::Dynamic {
                    size, ..
                } => self.builder.emit(LirOp::StorePtrWithOffset {
                    ptr: Ptr::CpuState,
                    size: *size,
                }),
            }
        }
    }

    fn get_var(&mut self, var: &VarId) -> DefId {
        *self.var_defs.entry(*var).or_insert_with(|| {
            let def_id = self.builder.define_var();
            debug!("Variable {var:?} = definition {def_id:?}");
            def_id
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use test_log::test;

    use crate::codegen::lir::{Lir, MirToLir};

    #[test]
    fn mir_to_lir_should_be_fast() {
        let mir = serde_json::from_str(include_str!("testdata/mir1.json")).unwrap();

        let start = Instant::now();
        let conv = MirToLir::new(&mir);
        let lir = conv.build();
        let elapsed = start.elapsed();

        println!("LIR in {}ms: {lir:#?}", elapsed.as_millis());

        // This should really take <100ms, but we add a huge margin to make this test consistent.
        assert!(elapsed < Duration::from_millis(1000));
    }

    #[test]
    fn mir_to_lir_should_not_explode_size() {
        let mir = serde_json::from_str(include_str!("testdata/mir2.json")).unwrap();

        let start = Instant::now();
        let conv = MirToLir::new(&mir);
        let lir = conv.build();
        let elapsed = start.elapsed();

        println!("LIR with {} operations in {}ms: {lir:#?}", lir.num_ops(), elapsed.as_millis());

        assert!(lir.num_ops() < 1500);
    }

    #[test]
    fn ensure_all_variables_assigned() {
        let lir: Lir = serde_json::from_str(include_str!("testdata/lir1.json")).unwrap();
        lir.get_unassigned_defs();
    }
}
