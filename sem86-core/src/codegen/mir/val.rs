use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fmt::{Debug, Display};
use std::iter::once;
use std::ops::Index;

use liblisa::state::Size;
use liblisa::utils::bitmap::GrowingBitmap;
use liblisa::utils::bitmask_u128;
use log::trace;
use serde::{Deserialize, Serialize};

use crate::codegen::components::StronglyConnectedComponents;
use crate::codegen::{DataSize, Ptr};
use crate::il::{BinOp, FpBinOp, FpUnOp, UnOp};

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ValId(u32);

impl ValId {
    pub fn from_usize(n: usize) -> Self {
        Self(n.try_into().unwrap())
    }

    pub fn index(&self) -> usize {
        self.0 as usize
    }
}

impl Debug for ValId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "${}", self.0)
    }
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct VarId(u32);

impl VarId {
    pub fn from_usize(n: usize) -> Self {
        Self(n.try_into().unwrap())
    }

    pub fn index(&self) -> usize {
        self.0 as usize
    }
}

impl Debug for VarId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "v{}", self.0)
    }
}

/// Represents a value.
/// These values are expected to be constant during the execution of the MIR.
/// This means that the code loading the value can be moved to anywhere in the program.
/// Variables are in SSA form, so they will not update once they have been initialized.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ValNode {
    Const(u128),
    Var(VarId),
    BinOp {
        args: [ValId; 2],
        op: BinOp,
    },
    FpBinOp {
        args: [ValId; 2],
        rc: ValId,
        op: FpBinOp,
    },
    FpUnOp {
        arg: ValId,
        rc: ValId,
        op: FpUnOp,
    },
    UnOp {
        arg: ValId,
        op: UnOp,
    },
    /// Takes bit N from lhs if `mask[N]` is 0, or from rhs if `mask[N]` is 1.
    ///
    /// In other words, returns the value of `lhs` where all masked bits are replaced
    /// with the corresponding bit from `rhs`.
    Blend {
        lhs: ValId,
        rhs: ValId,
        mask: u128,
    },

    /// Computes `(val >> skip) & bitmask_u128(take)`
    Extract {
        val: ValId,
        skip: u8,
        take: u8,
    },

    Ite {
        cond: ValId,
        if_zero: ValId,
        if_nonzero: ValId,
    },

    /// Loads a pointer from the CPU state structure.
    ///
    /// MIR has been designed such that changes are only committed when returning.
    /// This means that the value in the CPU state structure does not change during execution of the MIR.
    LoadPtr {
        ptr: Ptr,
        offset: ValId,
        size: DataSize,
    },

    /// See `ValNode::LoadPtr`
    LoadPtrImm {
        ptr: Ptr,
        offset: u16,
        size: DataSize,
    },

    InstrLen,

    PartValues,
}

impl Debug for ValNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Const(n) => write!(f, "0x{n:X}"),
            Self::Var(var) => Debug::fmt(var, f),
            Self::BinOp {
                args,
                op,
            } => write!(f, "{op:?}({:?}, {:?})", args[0], args[1]),
            Self::FpBinOp {
                args,
                rc,
                op,
            } => write!(f, "{op:?}({:?}, {:?}, {rc:?})", args[0], args[1]),
            Self::FpUnOp {
                arg,
                rc,
                op,
            } => write!(f, "{op:?}({arg:?}, {rc:?})"),
            Self::UnOp {
                arg,
                op,
            } => write!(f, "{op:?}({arg:?})"),
            Self::Ite {
                cond,
                if_zero,
                if_nonzero,
            } => f
                .debug_struct("Ite")
                .field("cond", cond)
                .field("if_zero", if_zero)
                .field("if_nonzero", if_nonzero)
                .finish(),
            Self::LoadPtr {
                ptr,
                offset,
                size,
            } => f
                .debug_struct("LoadPtr")
                .field("ptr", ptr)
                .field("offset", offset)
                .field("size", size)
                .finish(),
            Self::LoadPtrImm {
                ptr,
                offset,
                size,
            } => f
                .debug_struct("LoadPtr")
                .field("ptr", ptr)
                .field("offset", offset)
                .field("size", size)
                .finish(),
            Self::Blend {
                lhs,
                rhs,
                mask,
            } => write!(f, "blend.0x{mask:X} [{lhs:?}, {rhs:?}]"),
            Self::Extract {
                val,
                skip,
                take,
            } => write!(f, "{val:?}[{skip}:{}]", skip + take),
            Self::InstrLen => write!(f, "InstrLen"),
            Self::PartValues => write!(f, "PartValues"),
        }
    }
}

impl Display for ValNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Debug::fmt(self, f)
    }
}

impl ValNode {
    pub fn referenced_nodes(&self) -> impl Iterator<Item = &ValId> {
        // TODO: Avoid allocation
        match self {
            ValNode::Const(_) | ValNode::Var(_) | ValNode::InstrLen | ValNode::PartValues => vec![],
            ValNode::BinOp {
                args, ..
            } => args.iter().collect(),
            ValNode::FpBinOp {
                args,
                rc,
                ..
            } => args.iter().chain(once(rc)).collect(),
            ValNode::FpUnOp {
                arg,
                rc,
                ..
            } => vec![arg, rc],
            ValNode::UnOp {
                arg, ..
            } => vec![arg],
            ValNode::Ite {
                cond,
                if_zero,
                if_nonzero,
            } => vec![cond, if_zero, if_nonzero],
            ValNode::LoadPtr {
                offset, ..
            } => vec![offset],
            ValNode::LoadPtrImm {
                ..
            } => vec![],
            ValNode::Blend {
                lhs,
                rhs,
                ..
            } => vec![lhs, rhs],
            ValNode::Extract {
                val, ..
            } => vec![val],
        }
        .into_iter()
    }

    pub fn referenced_nodes_mut(&mut self) -> impl Iterator<Item = &mut ValId> {
        // TODO: Avoid allocation
        match self {
            ValNode::Const(_) | ValNode::Var(_) | ValNode::InstrLen | ValNode::PartValues => vec![],
            ValNode::BinOp {
                args, ..
            } => args.iter_mut().collect(),
            ValNode::FpBinOp {
                args,
                rc,
                ..
            } => args.iter_mut().chain(once(rc)).collect(),
            ValNode::FpUnOp {
                arg,
                rc,
                ..
            } => vec![arg, rc],
            ValNode::UnOp {
                arg, ..
            } => vec![arg],
            ValNode::Ite {
                cond,
                if_zero,
                if_nonzero,
            } => vec![cond, if_zero, if_nonzero],
            ValNode::LoadPtr {
                offset, ..
            } => vec![offset],
            ValNode::LoadPtrImm {
                ..
            } => vec![],
            ValNode::Blend {
                lhs,
                rhs,
                ..
            } => vec![lhs, rhs],
            ValNode::Extract {
                val, ..
            } => vec![val],
        }
        .into_iter()
    }

    fn remap(self, b: &mut ValBuilder, map: &HashMap<ValId, ValId>) -> Option<ValId> {
        Some(match self {
            ValNode::BinOp {
                args,
                op,
            } => b.binop(op, args.map(|arg| map[&arg])),
            ValNode::FpBinOp {
                args,
                rc,
                op,
            } => b.fp_binop(op, args.map(|arg| map[&arg]), map[&rc]),
            ValNode::FpUnOp {
                arg,
                rc,
                op,
            } => b.fp_unop(op, map[&arg], map[&rc]),
            ValNode::UnOp {
                arg,
                op,
            } => b.unop(op, map[&arg]),
            ValNode::Extract {
                val,
                skip,
                take,
            } => b.extract(map[&val], skip, take),
            ValNode::Ite {
                cond,
                if_zero,
                if_nonzero,
            } => b.ite(map[&cond], map[&if_zero], map[&if_nonzero]),
            ValNode::LoadPtr {
                ptr,
                offset,
                size,
            } => b.load_ptr_offset(ptr, size, map[&offset]),
            // TODO: ValNode::Blend { lhs, rhs, mask } => b.combine_old_and_new(lhs, rhs, size),
            _ => return None,
        })
    }
}

// TODO: Make this an egraph.
// TODO: Fold constant values.
#[derive(Clone, Serialize, Deserialize)]
pub struct ValTree {
    tree: Vec<ValNode>,
}

impl Debug for ValTree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut f = f.debug_map();
        for (index, val) in self.tree.iter().enumerate() {
            f.entry(&ValId::from_usize(index), val);
        }

        f.finish()
    }
}

impl Index<ValId> for ValTree {
    type Output = ValNode;

    fn index(&self, index: ValId) -> &Self::Output {
        &self.tree[index.index()]
    }
}

impl ValTree {
    pub fn iter(&self) -> impl Iterator<Item = (ValId, &ValNode)> {
        self.tree
            .iter()
            .enumerate()
            .map(|(index, val)| (ValId::from_usize(index), val))
    }

    pub fn len(&self) -> usize {
        self.tree.len()
    }

    pub fn display(&self, id: ValId) -> impl Display {
        ValDisplay {
            id,
            tree: self,
        }
    }

    pub fn walk(&self, val: ValId, mut f: impl FnMut(ValId, &ValNode)) {
        let mut frontier = vec![val];
        let mut seen = GrowingBitmap::new();
        while let Some(val) = frontier.pop() {
            f(val, &self[val]);
            frontier.extend(self[val].referenced_nodes().copied().filter(|n| seen.set(n.index())));
        }
    }
}

struct ValDisplay<'a> {
    id: ValId,
    tree: &'a ValTree,
}

impl Display for ValDisplay<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.tree[self.id] {
            ValNode::Const(c) => write!(f, "0x{c:X}"),
            ValNode::Var(var_id) => write!(f, "v{}", var_id.index()),
            ValNode::BinOp {
                args,
                op,
            } => write!(f, "{op:?}({}, {})", self.tree.display(args[0]), self.tree.display(args[1])),
            ValNode::FpBinOp {
                args,
                rc,
                op,
            } => write!(
                f,
                "{op:?}({}, {}, {})",
                self.tree.display(args[0]),
                self.tree.display(args[1]),
                self.tree.display(rc)
            ),
            ValNode::FpUnOp {
                arg,
                rc,
                op,
            } => write!(f, "{op:?}({}, {})", self.tree.display(arg), self.tree.display(rc)),
            ValNode::UnOp {
                arg,
                op,
            } => write!(f, "{op:?}({})", self.tree.display(arg)),
            ValNode::Blend {
                lhs,
                rhs,
                mask,
            } => write!(f, "blend[0x{mask:X}]({}, {})", self.tree.display(lhs), self.tree.display(rhs)),
            ValNode::Extract {
                val,
                skip,
                take,
            } => write!(f, "extract[{}:{}]({})", skip, skip + take, self.tree.display(val)),
            ValNode::Ite {
                cond,
                if_zero,
                if_nonzero,
            } => write!(
                f,
                "ite({}, {}, {})",
                self.tree.display(cond),
                self.tree.display(if_zero),
                self.tree.display(if_nonzero)
            ),
            ValNode::LoadPtr {
                ptr,
                offset,
                size,
            } => write!(f, "load_ptr.{size:?} {ptr:?}+{}", self.tree.display(offset)),
            ValNode::LoadPtrImm {
                ptr,
                offset,
                size,
            } => {
                write!(f, "load_ptr.{size:?} {ptr:?}")?;
                if offset != 0 {
                    write!(f, "+0x{offset:X}")?;
                }

                Ok(())
            },
            ValNode::InstrLen => write!(f, "$instr_len"),
            ValNode::PartValues => write!(f, "$part_values"),
        }
    }
}

impl crate::codegen::graph_traits::Graph for ValTree {
    type Index = ValId;
    type Node = ValNode;
    const ROOT: Self::Index = ValId(0);

    fn num_nodes(&self) -> usize {
        self.tree.len()
    }

    fn node(&self, index: Self::Index) -> &Self::Node {
        &self.tree[index.index()]
    }
}

impl crate::codegen::graph_traits::Index for ValId {
    fn index(&self) -> usize {
        self.index()
    }

    fn from_usize(val: usize) -> Self {
        Self::from_usize(val)
    }
}

impl crate::codegen::graph_traits::Node<ValId> for ValNode {
    fn transitions(&self) -> impl Iterator<Item = ValId> {
        self.referenced_nodes().copied()
    }
}

pub struct ValBuilder {
    value_tree: Vec<ValNode>,
    map: HashMap<ValNode, ValId>,
}

impl crate::codegen::graph_traits::Graph for ValBuilder {
    type Index = ValId;
    type Node = ValNode;
    const ROOT: Self::Index = ValId(0);

    fn num_nodes(&self) -> usize {
        self.value_tree.len()
    }

    fn node(&self, index: Self::Index) -> &Self::Node {
        &self.value_tree[index.index()]
    }
}

impl Index<ValId> for ValBuilder {
    type Output = ValNode;

    fn index(&self, index: ValId) -> &Self::Output {
        &self.value_tree[index.index()]
    }
}

impl Default for ValBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ValBuilder {
    pub fn new() -> Self {
        ValBuilder {
            value_tree: Vec::new(),
            map: HashMap::new(),
        }
    }

    fn get_or_insert(&mut self, val: ValNode) -> ValId {
        match self.map.entry(val) {
            Entry::Occupied(e) => *e.get(),
            Entry::Vacant(e) => {
                let id = ValId::from_usize(self.value_tree.len());
                self.value_tree.push(val);
                e.insert(id);
                id
            },
        }
    }

    pub fn imm(&mut self, val: u128) -> ValId {
        self.get_or_insert(ValNode::Const(val))
    }

    pub fn unop(&mut self, op: UnOp, arg: ValId) -> ValId {
        if let ValNode::Const(c) = self.value_tree[arg.index()] {
            self.get_or_insert(ValNode::Const(op.execute(c)))
        } else {
            self.get_or_insert(ValNode::UnOp {
                arg,
                op,
            })
        }
    }

    pub fn fp_binop(&mut self, op: FpBinOp, args: [ValId; 2], rc: ValId) -> ValId {
        self.get_or_insert(ValNode::FpBinOp {
            args,
            rc,
            op,
        })
    }

    pub fn fp_unop(&mut self, op: FpUnOp, arg: ValId, rc: ValId) -> ValId {
        self.get_or_insert(ValNode::FpUnOp {
            arg,
            rc,
            op,
        })
    }

    pub fn binop(&mut self, op: BinOp, args: [ValId; 2]) -> ValId {
        let nodes = args.map(|arg| self.value_tree[arg.index()]);
        trace!("binop: {op:?} with args {args:?} = {nodes:?}");
        let node = match (op, nodes) {
            // constant operations can be computed
            (_, [ValNode::Const(lhs), ValNode::Const(rhs)]) => ValNode::Const(op.execute(lhs, rhs)),

            (BinOp::CmpEq, _) if args[0] == args[1] => return self.imm(1),
            (BinOp::CmpEq, _) => {
                if let Some(eq) = self.try_solve_equality(args[0], args[1]) {
                    return self.imm(eq as u128)
                } else {
                    ValNode::BinOp {
                        op: BinOp::CmpEq,
                        args,
                    }
                }
            },

            // or/xor/add with zero is value
            (BinOp::Or | BinOp::Xor | BinOp::Add, [ValNode::Const(0), other] | [other, ValNode::Const(0)]) => other,
            // shl/shr/sub with 0 is identity
            (BinOp::Shl | BinOp::Shr | BinOp::Sub, [other, ValNode::Const(0)]) => other,
            // and/mul with 0 is zero
            (BinOp::And | BinOp::Mul, [ValNode::Const(0), _] | [_, ValNode::Const(0)]) => ValNode::Const(0),
            // a ^ a = 0 and a - a = 0
            (BinOp::Xor | BinOp::Sub, [lhs, rhs]) if lhs == rhs => ValNode::Const(0),

            (BinOp::Shr, [_other, ValNode::Const(c)]) => {
                let num = c as u8;
                return self.extract(args[0], num, 128 - num)
            },

            (
                BinOp::And,
                [
                    lhs @ ValNode::UnOp {
                        op: UnOp::Parity | UnOp::IsZero | UnOp::SelectBit(_),
                        ..
                    },
                    ValNode::Const(c),
                ],
            ) => {
                if c & 1 != 0 {
                    lhs
                } else {
                    ValNode::Const(0)
                }
            },

            (BinOp::And, [_lhs, ValNode::Const(c)]) if c.trailing_ones() + c.leading_zeros() == 128 => {
                return self.extract(args[0], 0, c.trailing_ones() as u8)
            },

            (
                BinOp::And,
                [
                    ValNode::Const(and_const),
                    ValNode::BinOp {
                        op: BinOp::Shl,
                        args: inner_args,
                    },
                ],
            ) => match (inner_args.map(|arg| self.value_tree[arg.index()]), inner_args) {
                // (a << c1) & c2 == (a & (c2 >> c1)) << c1
                ([_, ValNode::Const(shift_amount)], [inner_lhs, shift_amount_val]) => {
                    let shifted_const = self.imm(and_const >> shift_amount as u32);
                    ValNode::BinOp {
                        op: BinOp::Shl,
                        args: [self.binop(BinOp::And, [inner_lhs, shifted_const]), shift_amount_val],
                    }
                },
                _ => ValNode::BinOp {
                    args,
                    op,
                },
            },

            (
                BinOp::And,
                [
                    ValNode::BinOp {
                        args: inner_args,
                        op: BinOp::And,
                    },
                    ValNode::Const(c1),
                ],
            ) => match (inner_args.map(|arg| self.value_tree[arg.index()]), inner_args) {
                // (a & c2) & c1 == a & (c1 & c2)
                ([ValNode::Const(c2), _], [_, other]) | ([_, ValNode::Const(c2)], [other, _]) => ValNode::BinOp {
                    op: BinOp::And,
                    args: [other, self.imm(c1 & c2)],
                },
                _ => ValNode::BinOp {
                    args,
                    op,
                },
            },

            (
                BinOp::Add,
                [
                    ValNode::BinOp {
                        args: inner_args,
                        op: BinOp::Add,
                    },
                    ValNode::Const(c1),
                ],
            ) => match (inner_args.map(|arg| self.value_tree[arg.index()]), inner_args) {
                // (a + c2) + c1 == a + (c1 + c2)
                ([ValNode::Const(c2), _], [_, other]) | ([_, ValNode::Const(c2)], [other, _]) => {
                    let c = self.imm(c1.wrapping_add(c2));
                    return self.binop(BinOp::Add, [other, c])
                },
                _ => ValNode::BinOp {
                    args,
                    op,
                },
            },

            (
                BinOp::Add,
                [
                    ValNode::BinOp {
                        args: inner_args,
                        op: BinOp::Sub,
                    },
                    ValNode::Const(c1),
                ],
            ) => match (inner_args.map(|arg| self.value_tree[arg.index()]), inner_args) {
                // (a - c2) + c1 == a + (c1 - c2)
                ([_, ValNode::Const(c2)], [other, _]) => {
                    let c = self.imm(c1.wrapping_sub(c2));
                    return self.binop(BinOp::Add, [other, c])
                },
                // (c2 - a) + c1 == (c1 + c2) - a
                ([ValNode::Const(c2), _], [_, other]) => {
                    let c = self.imm(c1.wrapping_add(c2));
                    return self.binop(BinOp::Sub, [c, other])
                },
                _ => ValNode::BinOp {
                    args,
                    op,
                },
            },

            (
                BinOp::Sub,
                [
                    ValNode::BinOp {
                        args: inner_args,
                        op: BinOp::Sub,
                    },
                    ValNode::Const(c1),
                ],
            ) => match (inner_args.map(|arg| self.value_tree[arg.index()]), inner_args) {
                // (a - c2) - c1 == a - (c1 + c2)
                ([_, ValNode::Const(c2)], [other, _]) => {
                    let c = self.imm(c1.wrapping_add(c2));
                    return self.binop(BinOp::Sub, [other, c])
                },
                // (c2 - a) - c1 == (c2 - c1) - a
                ([ValNode::Const(c2), _], [_, other]) => {
                    let c = self.imm(c2.wrapping_sub(c1));
                    return self.binop(BinOp::Sub, [c, other])
                },
                _ => ValNode::BinOp {
                    args,
                    op,
                },
            },

            (
                BinOp::Sub,
                [
                    ValNode::BinOp {
                        args: inner_args,
                        op: BinOp::Add,
                    },
                    ValNode::Const(c1),
                ],
            ) => match (inner_args.map(|arg| self.value_tree[arg.index()]), inner_args) {
                // (a + c2) - c1 == a + (c2 - c1)
                ([ValNode::Const(c2), _], [_, other]) | ([_, ValNode::Const(c2)], [other, _]) => {
                    let c = self.imm(c2.wrapping_sub(c1));
                    return self.binop(BinOp::Add, [other, c])
                },
                _ => ValNode::BinOp {
                    args,
                    op,
                },
            },

            (
                BinOp::Or,
                [
                    ValNode::BinOp {
                        args: inner_args,
                        op: BinOp::Or,
                    },
                    ValNode::Const(c1),
                ],
            ) => match (inner_args.map(|arg| self.value_tree[arg.index()]), inner_args) {
                // (a | c2) | c1 == a | (c1 | c2)
                ([ValNode::Const(c2), _], [_, other]) | ([_, ValNode::Const(c2)], [other, _]) => ValNode::BinOp {
                    op: BinOp::Or,
                    args: [other, self.imm(c1 | c2)],
                },
                _ => ValNode::BinOp {
                    args,
                    op,
                },
            },

            (
                BinOp::And,
                [
                    ValNode::BinOp {
                        args: inner_args,
                        op: BinOp::Or,
                    },
                    ValNode::Const(c1),
                ],
            ) => match inner_args.map(|arg| (arg, self.value_tree[arg.index()])) {
                // (a | c2) & c1 == (a & c1) | (c2 & c1)
                [(_, ValNode::Const(c2)), (other, _)] | [(other, _), (_, ValNode::Const(c2))] => {
                    trace!("Substituting ({other:?} | 0x{c2:X}) & 0x{c1:X} for ({other:?} & {c1:X}) | ({c2:X} & {c1:X})");
                    let imm = self.imm(c1);
                    let val = self.binop(BinOp::And, [other, imm]);
                    ValNode::BinOp {
                        op: BinOp::Or,
                        args: [val, self.imm(c1 & c2)],
                    }
                },
                _ => ValNode::BinOp {
                    args,
                    op,
                },
            },

            // ((lhs & !mask) | (rhs & mask)) & c = rhs & c if mask is superset of c
            // If we only take the new bits from a blended value, we can skip the blend.
            (
                BinOp::And,
                [
                    ValNode::Blend {
                        rhs,
                        mask,
                        ..
                    },
                    ValNode::Const(c1),
                ],
            ) if mask & c1 == c1 => ValNode::BinOp {
                args: [rhs, self.imm(c1)],
                op: BinOp::And,
            },

            (
                BinOp::And,
                [
                    ValNode::Extract {
                        take, ..
                    },
                    ValNode::Const(c1),
                ],
            ) if c1 == bitmask_u128(take as u32) => return args[0],

            (
                BinOp::And,
                [
                    ValNode::Extract {
                        skip: 0,
                        take,
                        val,
                    },
                    ValNode::Const(c1),
                ],
            ) => {
                let mask = self.imm(c1 & bitmask_u128(take as u32));
                return self.binop(BinOp::And, [val, mask])
            },

            (
                BinOp::And,
                [
                    ValNode::LoadPtr {
                        size, ..
                    }
                    | ValNode::LoadPtrImm {
                        size, ..
                    },
                    ValNode::Const(c1),
                ],
            ) if c1.trailing_ones() as usize >= size.num_bits() => return args[0],

            (
                BinOp::And,
                [
                    ValNode::LoadPtr {
                        size, ..
                    }
                    | ValNode::LoadPtrImm {
                        size, ..
                    },
                    ValNode::Const(c1),
                ],
            ) if c1.trailing_zeros() as usize >= size.num_bits() => return self.imm(0),

            _ => ValNode::BinOp {
                args,
                op,
            },
        };

        let val = self.get_or_insert(node);
        trace!("resolved {val:?} = {node:#?} for binop: {op:?} with args {args:?}");
        val
    }

    pub fn ite(&mut self, cond: ValId, if_zero: ValId, if_nonzero: ValId) -> ValId {
        if let ValNode::Const(c) = self[cond] {
            return if c == 0 { if_zero } else { if_nonzero }
        }

        // is zero propagation: ite(c, ite(c, x, y), z) = ite(c, x, z)
        if let ValNode::Ite {
            cond: inner_cond,
            if_zero: inner_if_zero,
            ..
        } = self[if_zero]
            && cond == inner_cond
        {
            return self.ite(cond, inner_if_zero, if_nonzero)
        }

        // is not zero propagation: ite(c, x, ite(c, y, z)) = ite(c, x, z)
        if let ValNode::Ite {
            cond: inner_cond,
            if_nonzero: inner_if_nonzero,
            ..
        } = self[if_nonzero]
            && cond == inner_cond
        {
            return self.ite(cond, if_zero, inner_if_nonzero)
        }

        self.get_or_insert(ValNode::Ite {
            cond,
            if_zero,
            if_nonzero,
        })
    }

    pub fn load_ptr_imm(&mut self, ptr: Ptr, size: DataSize, offset: u16) -> ValId {
        self.get_or_insert(ValNode::LoadPtrImm {
            ptr,
            size,
            offset,
        })
    }

    pub fn load_ptr_offset(&mut self, ptr: Ptr, size: DataSize, offset: ValId) -> ValId {
        self.get_or_insert(ValNode::LoadPtr {
            ptr,
            size,
            offset,
        })
    }

    pub fn load_ptr(&mut self, ptr: Ptr, size: DataSize) -> ValId {
        self.load_ptr_imm(ptr, size, 0)
    }

    /// Computes blend[mask_from_size(size)](old, new << size.start_byte()).
    pub fn combine_old_and_new(&mut self, old: ValId, mut new: ValId, size: Size) -> ValId {
        trace!("blend {old:?} with {size:?} from {new:?}");
        let unshifted_mask = bitmask_u128(size.num_bytes() as u32 * 8);
        let mask = unshifted_mask << (size.start_byte() * 8);
        match (self.value_tree[old.index()], self.value_tree[new.index()]) {
            (
                ValNode::Extract {
                    take, ..
                },
                _,
            ) if (!mask) & bitmask_u128(take as u32) == 0 => {
                let shift = self.imm(size.start_byte() as u128 * 8);
                let mask_val = self.imm(unshifted_mask);
                let new = self.binop(BinOp::And, [new, mask_val]);
                return self.binop(BinOp::Shl, [new, shift]);
            },
            (ValNode::Const(0), _) => {
                let shift = self.imm(size.start_byte() as u128 * 8);
                let mask_val = self.imm(unshifted_mask);
                let new = self.binop(BinOp::And, [new, mask_val]);
                return self.binop(BinOp::Shl, [new, shift]);
            },
            (_, ValNode::Const(c)) => {
                let new = (c << (size.start_byte() * 8)) & mask;
                let inverted_mask = self.imm(!mask);
                let old = self.binop(BinOp::And, [old, inverted_mask]);
                let new = self.imm(new);
                return self.binop(BinOp::Or, [old, new])
            },
            // Inner blend only replaces values that we will also update, so we can eliminate the inner blend.
            (
                ValNode::Blend {
                    lhs: lhs_inner,
                    mask: mask_inner,
                    ..
                },
                _,
            ) if mask & mask_inner == mask_inner => return self.combine_old_and_new(lhs_inner, new, size),
            // Blends update distinct parts, so we can do both at the same time.
            (
                ValNode::Blend {
                    lhs: old_inner,
                    mask: mask_inner,
                    rhs: new_inner,
                },
                _,
            ) if mask & mask_inner == 0 => {
                let mask_inner_val = self.imm(mask_inner);
                let new_inner = self.binop(BinOp::And, [mask_inner_val, new_inner]);

                let shift = self.imm(size.start_byte() as u128 * 8);
                let mask_val = self.imm(unshifted_mask);
                let new = self.binop(BinOp::And, [new, mask_val]);
                let new = self.binop(BinOp::Shl, [new, shift]);

                let both = self.binop(BinOp::Or, [new_inner, new]);
                return self.combine_old_and_new_mask(old_inner, both, mask_inner | mask)
            },
            (
                ValNode::LoadPtr {
                    size: ds, ..
                }
                | ValNode::LoadPtrImm {
                    size: ds, ..
                },
                _,
            ) if mask.trailing_ones() as usize >= ds.num_bits() => {
                trace!("inserting blend as mask-and-shift of pointer load");
                let shift = self.imm(size.start_byte() as u128 * 8);
                let mask_val = self.imm(unshifted_mask);
                let new = self.binop(BinOp::And, [new, mask_val]);
                return self.binop(BinOp::Shl, [new, shift]);
            },
            (
                _,
                ValNode::BinOp {
                    op: BinOp::And,
                    args,
                },
            ) => match args.map(|arg| (arg, self.value_tree[arg.index()])) {
                [(_, ValNode::Const(c)), (other, _)] | [(other, _), (_, ValNode::Const(c))]
                    if c.trailing_ones() >= size.num_bytes() as u32 * 8 =>
                {
                    return self.combine_old_and_new(old, other, size)
                },
                _ => (),
            },
            // TODO: Also match BinOp::And here
            (
                _,
                ValNode::BinOp {
                    op: op @ (BinOp::Add | BinOp::Or | BinOp::Xor),
                    args,
                },
            ) => match args.map(|arg| (arg, self.value_tree[arg.index()])) {
                [
                    (
                        _,
                        ValNode::Extract {
                            val,
                            skip,
                            take,
                        },
                    ),
                    (other, _),
                ]
                | [
                    (other, _),
                    (
                        _,
                        ValNode::Extract {
                            val,
                            skip,
                            take,
                        },
                    ),
                ] if skip == 0 && take as usize >= size.num_bytes() * 8 => {
                    new = self.binop(op, [val, other]);
                },
                [
                    (
                        _,
                        ValNode::BinOp {
                            op: BinOp::And,
                            args: inner_args,
                        },
                    ),
                    (rhs, _),
                ] => match inner_args.map(|arg| (arg, self.value_tree[arg.index()])) {
                    [(_, ValNode::Const(c)), (lhs, _)] | [(lhs, _), (_, ValNode::Const(c))]
                        if c.trailing_ones() >= size.num_bytes() as u32 * 8 =>
                    {
                        new = self.binop(op, [lhs, rhs]);
                    },
                    _ => (),
                },
                _ => (),
            },
            _ => (),
        }

        trace!("Shifting {new:?}");
        let shift = self.imm(size.start_byte() as u128 * 8);
        let new = self.binop(BinOp::Shl, [new, shift]);

        self.combine_old_and_new_mask(old, new, mask)
    }

    /// Does not perform any shifting of the new value.
    pub fn combine_old_and_new_mask(&mut self, old: ValId, new: ValId, mask: u128) -> ValId {
        // TODO: Implement these
        match (self.value_tree[old.index()], self.value_tree[new.index()]) {
            // (ValNode::Extract { val, skip, take }, _) if (!mask) & bitmask_u128(take as u32) == 0 => {
            // TODO: Implement this
            // }
            // (ValNode::Const(0), _) => {
            // TODO: Implement this
            // }
            // (_, ValNode::Const(c)) => {
            // TODO: Implement this
            // }
            // Inner blend only replaces values that we will also update, so we can eliminate the inner blend.
            (
                ValNode::Blend {
                    lhs: lhs_inner,
                    mask: mask_inner,
                    ..
                },
                _,
            ) if mask & mask_inner == mask_inner => return self.combine_old_and_new_mask(lhs_inner, new, mask),
            // Blends update distinct parts, so we can do both at the same time.
            (
                ValNode::Blend {
                    lhs: old_inner,
                    mask: mask_inner,
                    rhs: new_inner,
                },
                _,
            ) if mask & mask_inner == 0 => {
                let mask_inner_val = self.imm(mask_inner);
                let new_inner = self.binop(BinOp::And, [mask_inner_val, new_inner]);

                let mask_val = self.imm(mask);
                let new = self.binop(BinOp::And, [new, mask_val]);

                let both = self.binop(BinOp::Or, [new_inner, new]);
                return self.combine_old_and_new_mask(old_inner, both, mask_inner | mask)
            },
            _ => (),
        }

        self.get_or_insert(ValNode::Blend {
            lhs: old,
            rhs: new,
            mask,
        })
    }

    pub fn extract(&mut self, val: ValId, skip: u8, take: u8) -> ValId {
        let node = self.value_tree[val.index()];
        trace!("extract[{skip}:{}] from {val:?} = {node:?}", skip + take);
        match node {
            ValNode::Const(n) => return self.imm((n >> skip) & bitmask_u128(take as u32)),
            ValNode::Extract {
                val,
                skip: skip_inner,
                take: take_inner,
            } => {
                if skip >= take_inner {
                    return self.imm(0)
                } else {
                    return self.extract(val, skip + skip_inner, take.min(take_inner - skip))
                }
            },
            ValNode::UnOp {
                op: UnOp::SelectBit(_) | UnOp::IsZero | UnOp::Parity,
                ..
            } => {
                if skip == 0 {
                    return val
                } else {
                    return self.imm(0)
                }
            },
            ValNode::LoadPtr {
                size, ..
            } if (skip as usize + take as usize) > size.num_bits() => {
                if skip as usize >= size.num_bits() {
                    return self.imm(0)
                } else {
                    // Crop extraction to data size
                    return self.extract(val, skip, (size.num_bits() - skip as usize) as u8)
                }
            },
            ValNode::LoadPtr {
                size, ..
            }
            | ValNode::LoadPtrImm {
                size, ..
            } if skip == 0 && size.num_bits() <= take as usize => return val,
            ValNode::LoadPtr {
                ptr,
                size,
                offset,
            } if skip == 0 && size.num_bits() > take as usize && DataSize::try_from_bits(take as usize).is_some() => {
                return self.load_ptr_offset(ptr, DataSize::try_from_bits(take as usize).unwrap(), offset)
            },
            ValNode::LoadPtrImm {
                size, ..
            } if skip as usize >= size.num_bits() => return self.imm(0),
            ValNode::LoadPtrImm {
                ptr,
                size,
                offset,
            } if skip.is_multiple_of(8)
                && [8, 16, 32, 64, 128].contains(&take)
                && (skip as usize + take as usize) < size.num_bits() =>
            {
                return self.load_ptr_imm(
                    ptr,
                    DataSize::try_from_bits(take as usize).unwrap(),
                    offset + (skip / 8) as u16,
                )
            },
            ValNode::Blend {
                lhs,
                rhs,
                mask,
            } => {
                let mask_taken = (mask >> skip) & bitmask_u128(take as u32);
                if mask_taken.count_ones() == take as u32 {
                    return self.extract(rhs, skip, take)
                } else if mask_taken.count_ones() == 0 {
                    return self.extract(lhs, skip, take)
                }
            },
            ValNode::BinOp {
                op: op @ (BinOp::Add | BinOp::Or | BinOp::And | BinOp::Xor),
                args,
            } => {
                let inner_nodes = args.map(|arg| (arg, self.value_tree[arg.index()]));
                trace!("Inner args for extract from {node:?}: {inner_nodes:?}");
                match inner_nodes {
                    [
                        (
                            _,
                            ValNode::Extract {
                                val,
                                skip: inner_skip,
                                take: inner_take,
                            },
                        ),
                        (other, _),
                    ]
                    | [
                        (other, _),
                        (
                            _,
                            ValNode::Extract {
                                val,
                                skip: inner_skip,
                                take: inner_take,
                            },
                        ),
                    ] if inner_skip == 0 && skip == 0 && inner_take >= take => {
                        trace!(
                            "Resolving extract from {node:?} by applying {op:?} to [{val:?}, {other:?}], then extracting the result"
                        );
                        let inner = self.binop(op, [val, other]);
                        return self.extract(inner, skip, take)
                    },
                    [
                        (
                            _,
                            ValNode::BinOp {
                                op: BinOp::And,
                                args: inner_args,
                            },
                        ),
                        (rhs, _),
                    ] => match inner_args.map(|arg| (arg, self.value_tree[arg.index()])) {
                        [(_, ValNode::Const(c)), (lhs, _)] if skip == 0 && c.trailing_ones() >= take as u32 => {
                            let inner = self.binop(op, [lhs, rhs]);
                            return self.extract(inner, skip, take)
                        },
                        _ => (),
                    },
                    // (x & c)[i..j] == x[i..j] & c[i..j]
                    [(_, ValNode::Const(c)), (inner, _)] | [(inner, _), (_, ValNode::Const(c))] if op == BinOp::And => {
                        let val = self.extract(inner, skip, take);
                        let mask = self.imm((c >> skip) & bitmask_u128(take as u32));
                        return self.binop(BinOp::And, [val, mask])
                    },
                    _ => (),
                }

                // if op == BinOp::Add
                //     && (
                //         !matches!(self[args[0]], ValNode::Extract { .. })
                //         || !matches!(self[args[1]], ValNode::Extract { .. })
                //     ) {
                //     let lhs = self.extract(args[0], 0, skip + take);
                //     let rhs = self.extract(args[1], 0, skip + take);
                //     let val = self.binop(BinOp::Add, [ lhs, rhs ]);

                //     // TODO: Call self.extract again, but limit recursion
                //     return self.get_or_insert(ValNode::Extract {
                //         val,
                //         skip,
                //         take,
                //     })
                // }

                if matches!(op, BinOp::Or | BinOp::And | BinOp::Xor) {
                    let lhs = self.extract(args[0], skip, take);
                    let rhs = self.extract(args[1], skip, take);
                    return self.binop(op, [lhs, rhs])
                }
            },
            // ValNode::BinOp { op: BinOp::Sub, args } if !matches!(self[args[0]], ValNode::Extract { .. })
            //     || !matches!(self[args[1]], ValNode::Extract { .. }) => {
            //     let lhs = self.extract(args[0], 0, skip + take);
            //     let rhs = self.extract(args[1], 0, skip + take);
            //     let val = self.binop(BinOp::Sub, [ lhs, rhs ]);

            //     // TODO: Call self.extract again, but limit recursion
            //     return self.get_or_insert(ValNode::Extract {
            //         val,
            //         skip,
            //         take,
            //     })
            // }
            ValNode::BinOp {
                op: BinOp::Shl,
                args,
            } => {
                let inner_nodes = args.map(|arg| (arg, self.value_tree[arg.index()]));
                trace!("Inner args for extract from {node:?}: {inner_nodes:?}");
                match inner_nodes {
                    // extract[i..j](a << c) [where c >= j] == 0
                    [(_inner, _), (_, ValNode::Const(c))] if c >= (skip + take) as u128 => return self.imm(0),
                    [(inner, _), (_, ValNode::Const(c))] if c <= skip as u128 => return self.extract(inner, skip - c as u8, take),
                    // extract[i..j](a << c) == extract[0..j-i](a << (c - i)) if c >= i
                    [(inner, _), (_, ValNode::Const(c))] => {
                        assert!((skip as u128) < c && c < (skip + take) as u128);
                        let shift = c as u8 - skip;
                        let shift = self.imm(shift as u128);
                        let inner = self.binop(BinOp::Shl, [inner, shift]);
                        return self.get_or_insert(ValNode::Extract {
                            val: inner,
                            skip: 0,
                            take,
                        })
                    },
                    _ => (),
                }
            },
            ValNode::Ite {
                cond,
                if_zero,
                if_nonzero,
            } => {
                let if_zero = self.extract(if_zero, skip, take);
                let if_nonzero = self.extract(if_nonzero, skip, take);
                return self.get_or_insert(ValNode::Ite {
                    cond,
                    if_zero,
                    if_nonzero,
                })
            },
            _ => (),
        }

        if let ValNode::BinOp {
            op: BinOp::Sub,
            args: [lhs, rhs],
        } = node
            && let ValNode::Const(c1) = self[rhs]
            && let ValNode::Extract {
                val,
                skip: inner_skip,
                take: inner_take,
            } = self[lhs]
            && skip == 0
            && inner_skip == skip
            && inner_take == take
            && let ValNode::BinOp {
                op: inner_op @ (BinOp::Add | BinOp::Sub),
                args: [inner_lhs, inner_rhs],
            } = self[val]
            && let ValNode::Const(c2) = self[inner_rhs]
        {
            let sum = self.imm(match inner_op {
                BinOp::Sub => c1.wrapping_add(c2),
                BinOp::Add => c1.wrapping_sub(c2),
                _ => unreachable!(),
            });
            let val = self.binop(BinOp::Sub, [inner_lhs, sum]);

            return self.extract(val, skip, take)
        }

        self.get_or_insert(ValNode::Extract {
            val,
            skip,
            take,
        })
    }

    pub fn use_var(&mut self, var: VarId) -> ValId {
        self.get_or_insert(ValNode::Var(var))
    }

    pub fn build(self) -> ValTree {
        ValTree {
            tree: self.value_tree,
        }
    }

    pub fn instr_len(&mut self) -> ValId {
        self.get_or_insert(ValNode::InstrLen)
    }

    pub fn determine_updated_bytes_in(&self, new_val: ValId, original_value: ValId) -> Option<usize> {
        match self.value_tree[new_val.index()] {
            ValNode::Blend {
                lhs,
                mask,
                ..
            } if lhs == original_value => return Some((128 - mask.leading_zeros() as usize) / 8),
            _ => (),
        }

        None
    }

    pub fn part_values(&mut self) -> ValId {
        self.get_or_insert(ValNode::PartValues)
    }

    pub fn iter(&self) -> impl Iterator<Item = (ValId, &ValNode)> {
        self.value_tree
            .iter()
            .enumerate()
            .map(|(index, val)| (ValId::from_usize(index), val))
    }

    pub fn len(&self) -> usize {
        self.value_tree.len()
    }

    pub fn walk(&self, val: ValId, mut f: impl FnMut(ValId, &ValNode)) {
        let mut frontier = vec![val];
        let mut seen = GrowingBitmap::new();
        while let Some(val) = frontier.pop() {
            f(val, &self[val]);
            frontier.extend(self[val].referenced_nodes().copied().filter(|n| seen.set(n.index())));
        }
    }

    pub fn remap(&mut self, val: ValId, mut f: impl FnMut(&mut Self, ValId, &ValNode) -> Option<ValId>) -> ValId {
        let mut order = Vec::new();
        let mut frontier = vec![val];
        let mut relevant = GrowingBitmap::new();
        relevant.set(val.index());

        // Collect all relevant nodes so we can walk through them in reverse topological order
        while let Some(val) = frontier.pop() {
            frontier.extend(self[val].referenced_nodes().copied().filter(|n| relevant.set(n.index())));
        }

        let mut map = HashMap::new();
        StronglyConnectedComponents::iterate_with_roots(self, once(val), |nodes| {
            assert_eq!(nodes.len(), 1);

            let val = nodes[0];
            if relevant[val.index()] {
                order.push(val);
            }
        });

        for original_val in order.into_iter() {
            let original_node = self.value_tree[original_val.index()];
            trace!("Remapping {original_val:?} = {original_node:?}");
            let remapped_val = original_node.remap(self, &map).unwrap_or(original_val);
            let node = self.value_tree[remapped_val.index()];

            let new = f(self, remapped_val, &node).unwrap_or(remapped_val);
            map.insert(original_val, new);
        }

        map[&val]
    }

    fn try_solve_equality(&self, lhs: ValId, rhs: ValId) -> Option<bool> {
        self.try_solve_equality_internal(
            EqualityState {
                val: lhs,
                offset: 0,
                skip: 0,
                take: 128,
            },
            EqualityState {
                val: rhs,
                offset: 0,
                skip: 0,
                take: 128,
            },
        )
    }

    fn try_solve_equality_internal(&self, lhs: EqualityState, rhs: EqualityState) -> Option<bool> {
        trace!("Trying to solve equality: {lhs:?} vs {rhs:?}");
        if lhs.val == rhs.val
            && lhs.skip == 0 // TODO
            && rhs.skip == 0 // TODO
            && lhs.take == rhs.take
        {
            return Some(lhs.offset & bitmask_u128(lhs.take as u32) == rhs.offset & bitmask_u128(rhs.take as u32))
        }

        // TODO: Allow additional extracts if it doesn't affect result of addition/subtraction
        if let ValNode::Extract {
            val,
            skip,
            take,
        } = self[lhs.val]
        {
            if lhs.offset != 0 {
                return None
            }

            return self.try_solve_equality_internal(
                EqualityState {
                    val,
                    offset: 0,
                    skip: lhs.skip + skip,
                    take: lhs.take.min(take - lhs.skip),
                },
                rhs,
            )
        }

        if let ValNode::Extract {
            val,
            skip,
            take,
        } = self[rhs.val]
        {
            if rhs.offset != 0 {
                return None
            }

            return self.try_solve_equality_internal(
                lhs,
                EqualityState {
                    val,
                    offset: 0,
                    skip: rhs.skip + skip,
                    take: rhs.take.min(take - rhs.skip),
                },
            )
        }

        if let ValNode::BinOp {
            op: BinOp::Add,
            args: [x, y],
        } = self[lhs.val]
            && let ValNode::Const(c) = self[y]
        {
            return self.try_solve_equality_internal(
                EqualityState {
                    val: x,
                    offset: lhs.offset.wrapping_add(c),
                    ..lhs
                },
                rhs,
            )
        }

        if let ValNode::BinOp {
            op: BinOp::Sub,
            args: [x, y],
        } = self[lhs.val]
            && let ValNode::Const(c) = self[y]
        {
            return self.try_solve_equality_internal(
                EqualityState {
                    val: x,
                    offset: lhs.offset.wrapping_sub(c),
                    ..lhs
                },
                rhs,
            )
        }

        if let ValNode::BinOp {
            op: BinOp::Add,
            args: [x, y],
        } = self[rhs.val]
            && let ValNode::Const(c) = self[y]
        {
            return self.try_solve_equality_internal(
                lhs,
                EqualityState {
                    val: x,
                    offset: rhs.offset.wrapping_add(c),
                    ..rhs
                },
            )
        }

        if let ValNode::BinOp {
            op: BinOp::Sub,
            args: [x, y],
        } = self[rhs.val]
            && let ValNode::Const(c) = self[y]
        {
            return self.try_solve_equality_internal(
                lhs,
                EqualityState {
                    val: x,
                    offset: rhs.offset.wrapping_sub(c),
                    ..rhs
                },
            )
        }

        None
    }

    pub fn optimize_as_if_zero(&self, cond: ValId) -> (ValId, bool) {
        if let ValNode::UnOp {
            arg: inner,
            op: UnOp::IsZero,
        } = self[cond]
        {
            let (val, flip) = self.optimize_as_if_zero(inner);

            return (val, !flip)
        }

        (cond, false)
    }
}

#[derive(Debug)]
struct EqualityState {
    val: ValId,
    offset: u128,
    skip: u8,
    take: u8,
}

#[cfg(test)]
mod tests {
    use liblisa::state::Size;
    use test_log::test;

    use crate::codegen::mir::val::{ValBuilder, ValNode};
    use crate::codegen::{DataSize, Ptr};
    use crate::il::BinOp;

    #[test]
    pub fn should_optimize_blend_extract() {
        let mut b = ValBuilder::new();
        let val = b.load_ptr(Ptr::CpuState, DataSize::Dword);
        let three = b.imm(3);
        let sum = b.binop(BinOp::Mul, [val, three]);
        let combined = b.combine_old_and_new(val, sum, Size::new(0, 3));
        let extracted = b.extract(combined, 0, 32);
        let sum2 = b.binop(BinOp::Add, [extracted, three]);
        let combined = b.combine_old_and_new(val, sum2, Size::new(0, 3));
        let extracted = b.extract(combined, 0, 32);

        let t = b.build();
        println!("{t:#?}");

        let ValNode::Extract {
            val,
            skip: 0,
            take: 32,
        } = t[extracted]
        else {
            panic!()
        };

        let ValNode::BinOp {
            op: BinOp::Add,
            args: [lhs, rhs],
        } = t[val]
        else {
            panic!()
        };

        let ValNode::Const(3) = t[rhs] else { panic!() };

        let ValNode::BinOp {
            op: BinOp::Mul,
            args: [lhs, rhs],
        } = t[lhs]
        else {
            panic!()
        };

        assert_eq!(
            t[lhs],
            ValNode::LoadPtrImm {
                ptr: Ptr::CpuState,
                offset: 0,
                size: DataSize::Dword
            }
        );
        assert_eq!(t[rhs], ValNode::Const(3));
    }

    #[test]
    pub fn should_optimize_blend_and() {
        let mut b = ValBuilder::new();
        let val = b.load_ptr(Ptr::CpuState, DataSize::Dword);
        let one = b.imm(1);
        let sum = b.binop(BinOp::Add, [val, one]);
        let mask = b.imm(0xffff_ffff);
        let sum = b.binop(BinOp::And, [sum, mask]);
        let blended = b.combine_old_and_new(val, sum, Size::new(0, 3));
        let sum = b.binop(BinOp::Add, [blended, one]);
        let mask = b.imm(0xffff_ffff);
        let sum = b.binop(BinOp::And, [sum, mask]);
        let blended = b.combine_old_and_new(blended, sum, Size::new(0, 3));

        let t = b.build();
        println!("{t:#?}");
        println!("blended = {blended:?}");

        let ValNode::Blend {
            rhs,
            mask: 0xffff,
            ..
        } = t[blended]
        else {
            panic!()
        };
        let ValNode::BinOp {
            op: BinOp::Add,
            args: [lhs, rhs],
        } = t[rhs]
        else {
            panic!()
        };

        assert_eq!(
            t[lhs],
            ValNode::LoadPtrImm {
                ptr: Ptr::CpuState,
                offset: 0,
                size: DataSize::Dword
            }
        );
        assert_eq!(t[rhs], ValNode::Const(2));
    }

    #[test]
    pub fn should_optimize_never_equal1() {
        let mut b = ValBuilder::new();
        let val = b.load_ptr(Ptr::CpuState, DataSize::Dword);
        let one = b.imm(1);
        let sum = b.binop(BinOp::Add, [val, one]);
        let mask = b.imm(7);
        let x = b.binop(BinOp::And, [sum, mask]);

        let sum = b.binop(BinOp::Add, [val, one]);
        let sum = b.binop(BinOp::Add, [sum, one]);
        let y = b.binop(BinOp::And, [sum, mask]);

        let equal = b.binop(BinOp::CmpEq, [x, y]);

        let t = b.build();
        println!("{t:#?}");

        assert_eq!(t[equal], ValNode::Const(0));
    }

    #[test]
    pub fn should_optimize_never_equal2() {
        let mut b = ValBuilder::new();
        let val = b.load_ptr(Ptr::CpuState, DataSize::Dword);
        let one = b.imm(1);
        let sum = b.binop(BinOp::Add, [val, one]);
        let mask = b.imm(7);
        let x = b.binop(BinOp::And, [sum, mask]);

        let one = b.imm(0x5);
        let sum = b.binop(BinOp::Add, [val, one]);
        let y = b.binop(BinOp::And, [sum, mask]);

        let equal = b.binop(BinOp::CmpEq, [x, y]);

        let t = b.build();
        println!("{t:#?}");

        assert_eq!(t[equal], ValNode::Const(0));
    }

    #[test]
    pub fn should_optimize_always_equal() {
        let mut b = ValBuilder::new();
        let val = b.load_ptr(Ptr::CpuState, DataSize::Dword);
        let one = b.imm(1);
        let sum = b.binop(BinOp::Add, [val, one]);
        let mask = b.imm(7);
        let x = b.binop(BinOp::And, [sum, mask]);

        let one = b.imm(0x9);
        let sum = b.binop(BinOp::Add, [val, one]);
        let y = b.binop(BinOp::And, [sum, mask]);

        let equal = b.binop(BinOp::CmpEq, [x, y]);

        let t = b.build();
        println!("{t:#?}");

        assert_eq!(t[equal], ValNode::Const(1));
    }
}
