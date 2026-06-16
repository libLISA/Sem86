#![allow(unused)]

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::fmt::{Debug, Display};

use liblisa::state::Size;
use liblisa::utils::bitmask_u128;

use crate::codegen::mir::union_find::UnionFind;
use crate::codegen::mir::val::VarId;
use crate::codegen::{DataSize, Ptr};
use crate::il::{BinOp, UnOp};

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClassId(u32);

impl ClassId {
    pub fn from_usize(n: usize) -> Self {
        Self(n.try_into().unwrap())
    }

    pub fn index(&self) -> usize {
        self.0 as usize
    }
}

impl Debug for ClassId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "~{}", self.0)
    }
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PatternId(u32);

impl PatternId {
    pub fn from_usize(n: usize) -> Self {
        Self(n.try_into().unwrap())
    }

    pub fn index(&self) -> usize {
        self.0 as usize
    }
}

impl Debug for PatternId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "@{}", self.0)
    }
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CaptureId(u32);

impl CaptureId {
    pub fn from_usize(n: usize) -> Self {
        Self(n.try_into().unwrap())
    }

    pub fn index(&self) -> usize {
        self.0 as usize
    }
}

impl Debug for CaptureId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "${}", self.0)
    }
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ReplacementId(u32);

impl ReplacementId {
    pub fn from_usize(n: usize) -> Self {
        Self(n.try_into().unwrap())
    }

    pub fn index(&self) -> usize {
        self.0 as usize
    }
}

impl Debug for ReplacementId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "^{}", self.0)
    }
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(u32);

impl NodeId {
    pub fn from_usize(n: usize) -> Self {
        Self(n.try_into().unwrap())
    }

    pub fn index(&self) -> usize {
        self.0 as usize
    }
}

impl Debug for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "${}", self.0)
    }
}

/// Represents a value.
/// These values are expected to be constant during the execution of the MIR.
/// This means that the code loading the value can be moved to anywhere in the program.
/// Variables are in SSA form, so they will not update once they have been initialized.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ValNode {
    Const(u128),
    Var(VarId),
    BinOp {
        args: [ClassId; 2],
        op: BinOp,
    },
    UnOp {
        arg: ClassId,
        op: UnOp,
    },
    Ite {
        cond: ClassId,
        if_zero: ClassId,
        if_nonzero: ClassId,
    },

    /// Loads a pointer from the CPU state structure.
    ///
    /// MIR has been designed such that changes are only committed when returning.
    /// This means that the value in the CPU state structure does not change during execution of the MIR.
    LoadPtr {
        ptr: Ptr,
        offset: ClassId,
        size: DataSize,
    },
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
        }
    }
}

impl Display for ValNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Debug::fmt(self, f)
    }
}

impl ValNode {
    pub fn children(&self) -> impl Iterator<Item = &ClassId> {
        match self {
            ValNode::Const(_) | ValNode::Var(_) => vec![],
            ValNode::BinOp {
                args, ..
            } => args.iter().collect(),
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
        }
        .into_iter()
    }

    pub fn children_mut(&mut self) -> impl Iterator<Item = &mut ClassId> {
        match self {
            ValNode::Const(_) | ValNode::Var(_) => vec![],
            ValNode::BinOp {
                args, ..
            } => args.iter_mut().collect(),
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
        }
        .into_iter()
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ValPattern {
    Any,
    Const(u128),
    Capture(CaptureId),
    BinOp {
        args: [PatternId; 2],
        op: BinOp,
    },
    UnOp {
        arg: PatternId,
        op: UnOp,
    },
    Ite {
        cond: PatternId,
        if_zero: PatternId,
        if_nonzero: PatternId,
    },

    /// Loads a pointer from the CPU state structure.
    ///
    /// MIR has been designed such that changes are only committed when returning.
    /// This means that the value in the CPU state structure does not change during execution of the MIR.
    LoadPtr {
        ptr: Ptr,
        offset: PatternId,
        size: DataSize,
    },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Replacement {
    Const(u128),
    Capture(CaptureId),
    BinOp {
        args: [ReplacementId; 2],
        op: BinOp,
    },
    UnOp {
        arg: ReplacementId,
        op: UnOp,
    },
    Ite {
        cond: ReplacementId,
        if_zero: ReplacementId,
        if_nonzero: ReplacementId,
    },

    /// Loads a pointer from the CPU state structure.
    ///
    /// MIR has been designed such that changes are only committed when returning.
    /// This means that the value in the CPU state structure does not change during execution of the MIR.
    LoadPtr {
        ptr: Ptr,
        offset: ReplacementId,
        size: DataSize,
    },
}

#[derive(Clone, Debug)]
struct AnnotatedPattern {
    pattern: ValPattern,
    replacement: ReplacementId,
}

#[derive(Clone, Debug)]
pub struct Patterns {
    patterns: Vec<AnnotatedPattern>, // TODO
    replacements: Vec<Replacement>,
}

#[derive(Clone, Debug)]
struct EClass {
    nodes: Vec<ValNode>,
    parents: Vec<ClassId>,
    matched_patterns: Vec<PatternMatch>,
}

#[derive(Clone, Debug)]
struct PatternMatch {
    pattern_id: PatternId,
    captures: HashMap<CaptureId, ClassId>,
}

impl EClass {
    fn union_with(&mut self, removed: EClass) {
        self.nodes.extend(removed.nodes);
        self.parents.extend(removed.parents);
    }
}

#[derive(Clone, Debug)]
pub struct ValGraphBuilder<'a> {
    classes: HashMap<ClassId, EClass>,
    node_to_class: HashMap<ValNode, ClassId>,
    worklist: Vec<ClassId>,
    union_find: UnionFind,
    patterns: &'a Patterns,
    next_class_id: usize,
}

impl<'a> ValGraphBuilder<'a> {
    pub fn new(patterns: &'a Patterns) -> Self {
        Self {
            classes: HashMap::new(),
            node_to_class: HashMap::new(),
            worklist: Vec::new(),
            union_find: UnionFind::new(0),
            patterns,
            next_class_id: 0,
        }
    }

    fn canonical_class_id(&mut self, id: ClassId) -> ClassId {
        ClassId::from_usize(self.union_find.find(id.index()))
    }

    fn union_classes(&mut self, lhs: ClassId, rhs: ClassId) {
        let lhs = self.canonical_class_id(lhs);
        let rhs = self.canonical_class_id(rhs);

        if lhs != rhs {
            let new_root_id = ClassId::from_usize(self.union_find.union(lhs.index(), rhs.index()).unwrap());
            let (target, removed) = if new_root_id == lhs {
                let v = self.classes.remove(&rhs).unwrap();
                (self.classes.get_mut(&lhs).unwrap(), v)
            } else if new_root_id == rhs {
                let v = self.classes.remove(&lhs).unwrap();
                (self.classes.get_mut(&rhs).unwrap(), v)
            } else {
                unreachable!()
            };

            self.worklist.extend(removed.parents.iter().copied());
            target.union_with(removed);
        }
    }

    fn get_or_insert(&mut self, mut val: ValNode) -> ClassId {
        Self::canonicalize_node(&mut self.union_find, &mut val);

        match self.node_to_class.entry(val) {
            Entry::Occupied(existing_class) => *existing_class.get(),
            Entry::Vacant(e) => {
                let new_class_id = {
                    let id = ClassId::from_usize(self.next_class_id);
                    self.next_class_id += 1;
                    self.union_find.resize(self.next_class_id);
                    id
                };
                e.insert(new_class_id);

                for child in val.children() {
                    let child = self.classes.get_mut(child).unwrap();
                    if !child.parents.contains(&new_class_id) {
                        child.parents.push(new_class_id);
                    }
                }

                let matched_patterns = self.find_matches(val);
                self.classes.insert(
                    new_class_id,
                    EClass {
                        nodes: vec![val],
                        parents: Vec::new(),
                        matched_patterns,
                    },
                );

                new_class_id
            },
        }
    }

    fn process_worklist(&mut self) {
        let mut unions_to_perform = HashSet::new();
        let mut seen = HashSet::new();
        while !self.worklist.is_empty() {
            while let Some(class) = self.worklist.pop() {
                let new_nodes = self.classes[&class]
                    .nodes
                    .iter()
                    .map(|&val| {
                        let mut val = val;
                        Self::canonicalize_node(&mut self.union_find, &mut val);
                        val
                    })
                    .filter(|val| seen.insert(*val))
                    .collect::<Vec<_>>();
                self.classes.get_mut(&class).unwrap().nodes = new_nodes.clone();

                // TODO: Determine if there are any new pattern matches

                for val in new_nodes.iter() {
                    match self.node_to_class.entry(*val) {
                        Entry::Occupied(mut e) => {
                            let old_class = *e.get();
                            if old_class != class {
                                *e.get_mut() = class;
                                self.union_classes(old_class, class);
                            }
                        },
                        Entry::Vacant(e) => {
                            e.insert(class);
                        },
                    }
                }
            }

            for (lhs, rhs) in unions_to_perform.drain() {
                self.union_classes(lhs, rhs);
            }
        }
    }

    fn rebuild_classes(&mut self) {}

    fn rebuild(&mut self) {
        self.process_worklist();
        self.rebuild_classes();
    }

    fn find_matches(&self, _val: ValNode) -> Vec<PatternMatch> {
        // TODO
        Vec::new()
    }

    fn canonicalize_node(union_find: &mut UnionFind, val: &mut ValNode) {
        for child in val.children_mut() {
            *child = ClassId::from_usize(union_find.find(child.index()));
        }
    }
}

// Public utility methods. These methods rely on the operations defined above.
impl ValGraphBuilder<'_> {
    pub fn imm(&mut self, val: u128) -> ClassId {
        self.get_or_insert(ValNode::Const(val))
    }

    pub fn unop(&mut self, op: UnOp, arg: ClassId) -> ClassId {
        // if let ValNode::Const(c) = self.nodes[arg.index()].node {
        //     self.get_or_insert(ValNode::Const(
        //         op.execute(c)
        //     ))
        // } else {
        self.get_or_insert(ValNode::UnOp {
            arg,
            op,
        })
        // }
    }

    pub fn binop(&mut self, op: BinOp, args: [ClassId; 2]) -> ClassId {
        // if let [ValNode::Const(lhs), ValNode::Const(rhs)] = args.map(|arg| self.nodes[arg.index()].node) {
        //     self.get_or_insert(ValNode::Const(
        //         op.execute(lhs, rhs)
        //     ))
        // } else {
        self.get_or_insert(ValNode::BinOp {
            args,
            op,
        })
        // }
    }

    pub fn ite(&mut self, cond: ClassId, if_zero: ClassId, if_nonzero: ClassId) -> ClassId {
        // if let ValNode::Const(c) = self.nodes[cond.index()].node {
        //     if c == 0 {
        //         if_zero
        //     } else {
        //         if_nonzero
        //     }
        // } else {
        self.get_or_insert(ValNode::Ite {
            cond,
            if_zero,
            if_nonzero,
        })
        // }
    }

    pub fn load_ptr_offset(&mut self, ptr: Ptr, size: DataSize, offset: ClassId) -> ClassId {
        self.get_or_insert(ValNode::LoadPtr {
            ptr,
            size,
            offset,
        })
    }

    pub fn load_ptr(&mut self, ptr: Ptr, size: DataSize) -> ClassId {
        let offset = self.imm(0);
        self.load_ptr_offset(ptr, size, offset)
    }

    pub fn combine_old_and_new(&mut self, old: ClassId, new: ClassId, size: Size) -> ClassId {
        let mask = bitmask_u128(size.num_bytes() as u32 * 8) << (size.start_byte() * 8);
        let inverted_mask = self.imm(!mask);
        let mask = self.imm(mask);
        let old = self.binop(BinOp::And, [old, inverted_mask]);
        let new = if size.start_byte() != 0 {
            let shift = self.imm(size.start_byte() as u128 * 8);
            self.binop(BinOp::Shl, [new, shift])
        } else {
            new
        };
        let new = self.binop(BinOp::And, [new, mask]);
        self.binop(BinOp::Or, [old, new])
    }

    pub fn use_var(&mut self, var: VarId) -> ClassId {
        self.get_or_insert(ValNode::Var(var))
    }

    // pub fn build(self) -> ValTree {
    //     ValTree {
    //         tree: self.nodes
    //     }
    // }
}

#[cfg(test)]
mod tests {
    use crate::codegen::mir::egraph::{Patterns, ValGraphBuilder};
    use crate::il::BinOp;

    #[test]
    pub fn test_union() {
        let patterns = Patterns {
            patterns: Vec::new(),
            replacements: Vec::new(),
        };
        let mut builder = ValGraphBuilder::new(&patterns);
        let a = builder.imm(1);
        let b = builder.imm(2);
        let c = builder.imm(3);

        let sum = builder.binop(BinOp::Add, [a, b]);

        let mul_sum = builder.binop(BinOp::Mul, [sum, b]);
        let mul_c = builder.binop(BinOp::Mul, [c, b]);

        println!("Graph: {builder:#?}");
        println!(
            "mul_sum={:?}, mul_c={:?}",
            builder.canonical_class_id(mul_sum),
            builder.canonical_class_id(mul_c)
        );
        assert_ne!(builder.canonical_class_id(mul_sum), builder.canonical_class_id(mul_c));

        builder.union_classes(sum, c);
        builder.process_worklist();

        println!("Graph: {builder:#?}");
        println!(
            "mul_sum={:?}, mul_c={:?}",
            builder.canonical_class_id(mul_sum),
            builder.canonical_class_id(mul_c)
        );
        assert_eq!(builder.canonical_class_id(mul_sum), builder.canonical_class_id(mul_c));
    }
}
