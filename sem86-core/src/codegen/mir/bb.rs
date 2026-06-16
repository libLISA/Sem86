use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::fmt::{Debug, Display};
use std::iter::once;
use std::mem::take;
use std::ops::Index;
use std::time::Instant;

use itertools::Itertools;
use liblisa::arch::Register;
use liblisa::utils::bitmap::GrowingBitmap;
use log::{debug, info, trace};
use sem86_arch::exceptions::Exception;
use serde::{Deserialize, Serialize};

use super::val::ValId;
use crate::arch::intel386::{HandlerId, Reg, State};
use crate::codegen::components::StronglyConnectedComponents;
use crate::codegen::lir::dominator::DominatorTree;
use crate::codegen::mir::val::{ValBuilder, ValNode, VarId};
use crate::codegen::{DataSize, Ptr};
use crate::il::UnOp;

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct BbId(u32);

impl BbId {
    pub const ROOT: BbId = BbId(0);

    pub fn from_usize(n: usize) -> Self {
        Self(n.try_into().unwrap())
    }

    pub fn index(&self) -> usize {
        self.0 as usize
    }
}

impl Debug for BbId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "!{}", self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BbSeq {
    Store { values: Vec<(VarId, ValId)> },
    ReadMemoryUnchecked { value: VarId, addr: ValId, num_bytes: u8 },
    WriteMemoryUnchecked { value: ValId, addr: ValId, num_bytes: u8 },
    Log { message: String },
    SetException { exception: Exception, code: ValId },
    SetHandler { id: HandlerId, args: [ValId; 2] },
    Commit { values: Vec<(CommitDest, ValId)> },
}

impl BbSeq {
    pub fn referenced_values(&self) -> impl Iterator<Item = &ValId> {
        match self {
            BbSeq::Store {
                values,
            } => values.iter().map(|(_, val)| val).collect::<Vec<_>>(),
            BbSeq::ReadMemoryUnchecked {
                addr, ..
            } => vec![addr],
            BbSeq::WriteMemoryUnchecked {
                value,
                addr,
                ..
            } => vec![value, addr],
            BbSeq::Log {
                ..
            } => vec![],
            BbSeq::SetException {
                code, ..
            } => vec![code],
            BbSeq::SetHandler {
                args, ..
            } => args.iter().collect::<Vec<_>>(),
            BbSeq::Commit {
                values,
            } => values
                .iter()
                .flat_map(|(dest, v)| {
                    [
                        Some(v),
                        match dest {
                            CommitDest::Reg(_) => None,
                            CommitDest::Fixed {
                                ..
                            } => None,
                            CommitDest::Dynamic {
                                offset, ..
                            } => Some(offset),
                        },
                    ]
                })
                .flatten()
                .collect::<Vec<_>>(),
        }
        .into_iter()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum BbFallible {
    PortOut {
        port: ValId,
        data: ValId,
        len: u8,
    },
    PortIn {
        port: ValId,
        data: VarId,
        len: u8,
    },
    ReadMemory {
        value: VarId,
        addr: ValId,
        num_bytes: u8,
    },
    WriteMemory {
        value: ValId,
        addr: ValId,
        num_bytes: u8,
    },
    ReadDescriptor {
        selector: ValId,
        force: bool,
        mark_accessed: bool,
        ok: VarId,
        base: VarId,
        limit: VarId,
        ar: VarId,
    },
}

impl BbFallible {
    pub fn referenced_values(&self) -> impl Iterator<Item = &ValId> {
        match self {
            BbFallible::PortOut {
                port,
                data,
                ..
            } => vec![port, data],
            BbFallible::PortIn {
                port, ..
            } => vec![port],
            BbFallible::ReadMemory {
                addr, ..
            } => vec![addr],
            BbFallible::WriteMemory {
                value,
                addr,
                ..
            } => vec![value, addr],
            BbFallible::ReadDescriptor {
                selector, ..
            } => vec![selector],
        }
        .into_iter()
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CommitDest {
    Reg(Reg),
    Fixed { size: DataSize, offset: u16 },
    Dynamic { size: DataSize, offset: ValId },
}

impl CommitDest {
    pub fn referenced_values(&self) -> impl Iterator<Item = &ValId> {
        match self {
            CommitDest::Reg(_)
            | CommitDest::Fixed {
                ..
            } => None,
            CommitDest::Dynamic {
                offset, ..
            } => Some(offset),
        }
        .into_iter()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Bb {
    Jump(BbId),
    Br {
        cond: ValId,
        if_zero: BbId,
        if_nonzero: BbId,
    },
    Seq {
        entry: BbSeq,
        next: BbId,
    },
    Fallible {
        op: BbFallible,
        if_ok: BbId,
        if_exception: BbId,
    },
    CommitAndExit {
        metadata: Option<u64>,
        values: Vec<(CommitDest, ValId)>,
        last_jump_condition: Option<ValId>,
        k: Option<ValId>,
        success: bool,
    },
}

impl Bb {
    pub fn next_blocks_mut(&mut self) -> impl Iterator<Item = &mut BbId> {
        match self {
            Bb::Jump(next) => vec![next].into_iter(),
            Bb::Br {
                if_zero,
                if_nonzero,
                ..
            } => vec![if_zero, if_nonzero].into_iter(),
            Bb::Seq {
                next, ..
            } => vec![next].into_iter(),
            Bb::Fallible {
                if_ok,
                if_exception,
                ..
            } => vec![if_ok, if_exception].into_iter(),
            Bb::CommitAndExit {
                ..
            } => vec![].into_iter(),
        }
    }

    pub fn next_blocks(&self) -> impl Iterator<Item = &BbId> {
        match self {
            Bb::Jump(next) => vec![next].into_iter(),
            Bb::Br {
                if_zero,
                if_nonzero,
                ..
            } => vec![if_zero, if_nonzero].into_iter(),
            Bb::Seq {
                next, ..
            } => vec![next].into_iter(),
            Bb::Fallible {
                if_ok,
                if_exception,
                ..
            } => vec![if_ok, if_exception].into_iter(),
            Bb::CommitAndExit {
                ..
            } => vec![].into_iter(),
        }
    }

    pub fn referenced_values(&self) -> impl Iterator<Item = &ValId> {
        match self {
            Bb::Jump(_) => vec![],
            Bb::Br {
                cond, ..
            } => vec![cond],
            Bb::Seq {
                entry, ..
            } => entry.referenced_values().collect::<Vec<_>>(),
            Bb::Fallible {
                op, ..
            } => op.referenced_values().collect::<Vec<_>>(),
            Bb::CommitAndExit {
                values,
                last_jump_condition,
                k,
                ..
            } => values
                .iter()
                .flat_map(|(dest, val)| once(val).chain(dest.referenced_values()))
                .chain(last_jump_condition.iter())
                .chain(k.iter())
                .collect::<Vec<_>>(),
        }
        .into_iter()
    }
}

impl Display for Bb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Debug::fmt(self, f)
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct BbGraph {
    graph: Vec<Bb>,
}

impl Debug for BbGraph {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut identical_node_map = HashMap::new();
        for (index, item) in self.graph.iter().enumerate() {
            identical_node_map
                .entry(item)
                .or_insert_with(Vec::new)
                .push(BbId::from_usize(index));
        }

        let mut f = f.debug_map();
        let mut seen = GrowingBitmap::new_all_zeros(self.graph.len());
        let mut frontier = (0..self.graph.len()).rev().map(BbId::from_usize).collect::<Vec<_>>();
        while let Some(id) = frontier.pop() {
            if seen.set(id.index()) {
                let bb = &self.graph[id.index()];
                let ids = &identical_node_map[bb];
                if ids.len() > 1 {
                    for id in ids.iter() {
                        seen.set(id.index());
                    }

                    f.entry(&ids, bb);
                } else {
                    f.entry(&id, bb);
                }

                match bb {
                    Bb::Seq {
                        next, ..
                    } => frontier.push(*next),
                    Bb::Jump(next) => frontier.push(*next),
                    Bb::Br {
                        if_zero,
                        if_nonzero,
                        ..
                    } => frontier.extend([*if_nonzero, *if_zero]),
                    Bb::Fallible {
                        if_ok,
                        if_exception,
                        ..
                    } => frontier.extend([*if_ok, *if_exception]),
                    Bb::CommitAndExit {
                        ..
                    } => (),
                }
            }
        }

        f.finish()
    }
}

impl Index<BbId> for BbGraph {
    type Output = Bb;

    fn index(&self, index: BbId) -> &Self::Output {
        &self.graph[index.index()]
    }
}

impl BbGraph {
    pub fn merge_identical_nodes(&mut self) {
        // TODO: Remove branches with constant conditions
        // TODO: Remove branches with conditions that have already been checked
        // TODO
        // let mut mapping = HashMap::new();
        // let mut remap = Vec::with_capacity(self.graph.len());
        // self.graph.retain(|item| {
        //     remap.push(*mapping.entry(item).or_insert_with(|| mapping.len()));
        // });

        // for item in self.graph.iter_mut() {
        //     for id in item.next_blocks() {
        //         *id = BbId::from_usize(remap[id.index()]);
        //     }
        // }
    }

    pub fn iter(&self) -> impl Iterator<Item = (BbId, &Bb)> {
        self.graph.iter().enumerate().map(|(index, bb)| (BbId::from_usize(index), bb))
    }

    pub fn len(&self) -> usize {
        self.graph.len()
    }

    pub fn optimize(&mut self, value_tree: &mut ValBuilder) {
        #[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        enum Class {
            IsZero,
            IsNonZero,
            Any,
        }

        impl Class {
            fn union(&mut self, other: &Class) {
                *self = match (*self, *other) {
                    (a, b) if a == b => a,
                    _ => Class::Any,
                }
            }
        }

        #[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        struct Assumption {
            val: ValId,
            class: Class,
        }

        #[derive(Clone, Debug, Default)]
        struct Assumptions {
            assumptions: Option<HashMap<ValId, Class>>,
        }

        impl Assumptions {
            pub fn union_with(&mut self, other: &Assumptions, extra: Option<Assumption>) {
                match self.assumptions.as_mut() {
                    Some(existing) => {
                        for (val, class) in other.assumptions.iter().flatten() {
                            match existing.entry(*val) {
                                Entry::Occupied(e) => e.into_mut().union(class),
                                Entry::Vacant(e) => {
                                    e.insert(*class);
                                },
                            }
                        }
                    },
                    None => {
                        let mut v = other.assumptions.as_ref().cloned().unwrap_or_else(HashMap::new);
                        if let Some(extra) = extra {
                            v.insert(extra.val, extra.class);
                        }

                        self.assumptions = Some(v);
                    },
                }
            }

            fn get(&self, val: ValId) -> Option<Class> {
                self.assumptions
                    .as_ref()
                    .and_then(|assumptions| assumptions.get(&val).copied())
            }
        }

        let start = Instant::now();
        debug!("Computing path conditions...");
        let mut reverse_topological_order = Vec::new();
        StronglyConnectedComponents::iterate(self, |nodes| {
            assert_eq!(nodes.len(), 1, "loops in control flow are not supported");
            reverse_topological_order.extend_from_slice(nodes);
        });

        let mut assumptions = vec![Assumptions::default(); self.graph.len()];
        for &id in reverse_topological_order.iter().rev() {
            match self[id] {
                Bb::Seq {
                    next, ..
                }
                | Bb::Jump(next) => {
                    let [next, current] = assumptions.get_disjoint_mut([next.index(), id.index()]).unwrap();
                    next.union_with(current, None);
                },
                Bb::Br {
                    cond,
                    if_zero,
                    if_nonzero,
                } => {
                    let [if_zero, if_nonzero, current] = assumptions
                        .get_disjoint_mut([if_zero.index(), if_nonzero.index(), id.index()])
                        .unwrap();
                    if_zero.union_with(
                        current,
                        Some(Assumption {
                            val: cond,
                            class: Class::IsZero,
                        }),
                    );
                    if_nonzero.union_with(
                        current,
                        Some(Assumption {
                            val: cond,
                            class: Class::IsNonZero,
                        }),
                    );
                },
                Bb::Fallible {
                    if_ok,
                    if_exception,
                    ..
                } => {
                    let [if_ok, if_exception, current] = assumptions
                        .get_disjoint_mut([if_ok.index(), if_exception.index(), id.index()])
                        .unwrap();
                    if_ok.union_with(current, None);
                    if_exception.union_with(current, None);
                },
                Bb::CommitAndExit {
                    ..
                } => (),
            }
        }

        info!("Computing path conditions took {}ms", start.elapsed().as_millis());

        let start = Instant::now();
        debug!("Updating branches and ITEs with path conditions...");
        for (bb, assumptions) in self.graph.iter_mut().zip(assumptions.iter()) {
            match bb {
                Bb::Br {
                    cond,
                    if_zero,
                    if_nonzero,
                } => match assumptions.get(*cond) {
                    Some(Class::IsZero) => *bb = Bb::Jump(*if_zero),
                    Some(Class::IsNonZero) => *bb = Bb::Jump(*if_nonzero),
                    _ => (),
                },
                Bb::CommitAndExit {
                    values,
                    last_jump_condition,
                    ..
                } => {
                    let zero_val = value_tree.imm(0);
                    let one_val = value_tree.imm(1);
                    for value in values.iter_mut().map(|(_, value)| value).chain(last_jump_condition.as_mut()) {
                        *value = value_tree.remap(*value, |_, id, val| match val {
                            ValNode::Ite {
                                cond,
                                if_zero,
                                if_nonzero,
                            } => match assumptions.get(*cond) {
                                Some(Class::IsZero) => Some(*if_zero),
                                Some(Class::IsNonZero) => Some(*if_nonzero),
                                _ => None,
                            },
                            ValNode::UnOp {
                                arg,
                                op: UnOp::IsZero,
                            } => match assumptions.get(*arg) {
                                Some(Class::IsZero) => Some(one_val),
                                Some(Class::IsNonZero) => Some(zero_val),
                                _ => None,
                            },
                            _ if assumptions.get(id) == Some(Class::IsZero) => Some(zero_val),
                            _ => None,
                        })
                    }

                    if let Some(cond) = last_jump_condition
                        && let Some(c) = assumptions.get(*cond)
                    {
                        match c {
                            Class::IsZero => *cond = zero_val,
                            Class::IsNonZero => *cond = one_val,
                            Class::Any => (),
                        }
                    }
                },
                _ => (),
            }
        }
        info!(
            "Updating branches and ITEs with path conditions took {}ms",
            start.elapsed().as_millis()
        );

        let start = Instant::now();
        debug!("Analyzing value usage...");
        let mut val_value_usage = vec![Vec::<ValId>::new(); value_tree.len()];
        StronglyConnectedComponents::iterate(value_tree, |nodes| {
            assert_eq!(nodes.len(), 1, "TODO: support loops");
            let node = nodes[0];
            val_value_usage[node.index()] = value_tree[node]
                .referenced_nodes()
                .flat_map(|n| val_value_usage[n.index()].iter())
                .copied()
                .chain(once(node))
                .sorted()
                .dedup()
                .collect();
        });

        let dt = DominatorTree::new(self);
        let mut vals_to_commit = vec![HashMap::<CommitDest, Vec<Option<ValId>>>::new(); self.len()];
        let mut return_points = vec![Vec::<BbId>::new(); self.len()];
        let mut vars_stored = vec![Vec::<VarId>::new(); self.len()];
        let mut reverse_topological_order = Vec::new();
        StronglyConnectedComponents::iterate(self, |nodes| {
            assert_eq!(nodes.len(), 1, "loops in control flow are not supported");
            reverse_topological_order.extend_from_slice(nodes);

            let node = nodes[0];

            return_points[node.index()] = self[node]
                .next_blocks()
                .flat_map(|next| return_points[next.index()].iter().copied())
                .chain(match &self[node] {
                    Bb::CommitAndExit {
                        ..
                    } => Some(node),
                    _ => None,
                })
                .unique()
                .collect();

            vars_stored[node.index()] = self[node]
                .next_blocks()
                .flat_map(|next| vars_stored[next.index()].iter())
                .chain(
                    if let Bb::Seq {
                        entry: BbSeq::Store {
                            values,
                        },
                        ..
                    } = &self[node]
                    {
                        Some(values.iter().map(|(var, _)| var))
                    } else {
                        None
                    }
                    .into_iter()
                    .flatten(),
                )
                .copied()
                .unique()
                .collect();

            vals_to_commit[node.index()] = match &self[node] {
                Bb::Seq {
                    next: id, ..
                }
                | Bb::Jump(id) => vals_to_commit[id.index()].clone(),
                Bb::Br {
                    if_zero: a,
                    if_nonzero: b,
                    ..
                }
                | Bb::Fallible {
                    if_ok: a,
                    if_exception: b,
                    ..
                } => {
                    let mut result = HashMap::new();
                    let a = &vals_to_commit[a.index()];
                    let b = &vals_to_commit[b.index()];
                    for k in a.keys().chain(b.keys()).unique() {
                        let a_entries = a.get(k);
                        let b_entries = b.get(k);
                        let vec = result.entry(*k).or_insert_with(Vec::new);
                        if let Some(v) = a_entries {
                            vec.extend_from_slice(v);
                        } else {
                            vec.push(None);
                        }

                        if let Some(v) = b_entries {
                            vec.extend_from_slice(v);
                        } else {
                            vec.push(None);
                        }

                        vec.sort();
                        vec.dedup();
                    }

                    result
                },
                Bb::CommitAndExit {
                    values, ..
                } => values.iter().map(|&(k, v)| (k, vec![Some(v)])).collect(),
            };
        });

        info!("Value usage analysis took {}ms", start.elapsed().as_millis());

        let start = Instant::now();
        debug!("Introducing early commits...");
        // If there are multiple paths to exit, but all will store the same values in the CPU state,
        // we should store them immediately here instead.
        // This reduces register pressure on the codegen backend, as there are less temporary values.
        let mut already_committed_per_bb = vec![HashSet::<CommitDest>::new(); self.len()];
        trace!("Control flow: {self:#?}");
        for &id in reverse_topological_order.iter().rev() {
            let bb = &mut self.graph[id.index()];
            let mut already_committed = take(&mut already_committed_per_bb[id.index()]);
            let original_next_blocks = bb.next_blocks().copied().collect::<Vec<_>>();

            trace!("{id:?} = {bb:?}, next blocks: {original_next_blocks:?}");

            if let Bb::CommitAndExit {
                values, ..
            } = bb
            {
                trace!("Removing already committed {already_committed:?} from {values:?}");
                values.retain(|(dest, _)| !already_committed.contains(dest));
            } else if let Bb::Seq {
                entry: BbSeq::SetException {
                    ..
                },
                ..
            } = bb
            {
                // It is not worth moving stores to before the SetException
            } else {
                let ready_to_commit = vals_to_commit[id.index()]
                    .iter()
                    .filter(|(_, v)| v.len() == 1)
                    .map(|(&k, v)| (k, v[0].unwrap()))
                    .collect::<Vec<_>>();
                trace!(
                    "Vals that will be committed in {id:?}'s successors: {:?}",
                    vals_to_commit[id.index()]
                );
                trace!("Ready to commit: {ready_to_commit:?}");

                trace!(
                    "Checking if {id:?} dominates all return points: {:?}",
                    return_points[id.index()]
                );
                let bb_dominates_all_returns = return_points[id.index()].iter().all(|&r| dt.dominates(id, r));
                if !bb_dominates_all_returns {
                    trace!("Path does not dominate all uses, skipping");
                } else {
                    let mut early_commits = Vec::new();
                    for (dest, val) in ready_to_commit {
                        if already_committed.contains(&dest) {
                            continue
                        }

                        // Avoid early-committing dynamic dest, because it is difficult to track all possible state involved.
                        if let CommitDest::Dynamic {
                            ..
                        } = dest
                        {
                            continue
                        }

                        let (dest_offset, dest_size) = match dest {
                            CommitDest::Reg(reg) => (State::byte_offset_of(reg) as u16, reg.byte_size() as u16),
                            CommitDest::Fixed {
                                size,
                                offset,
                            } => (offset, size.num_bytes() as u16),
                            CommitDest::Dynamic {
                                ..
                            } => continue,
                        };
                        trace!("Considering committing {dest:?} (state byte offset: 0x{dest_offset:X}) <- {val:?}");

                        // We must make sure there are no reads from the commit dest that we are about to overwrite here.
                        let val_used = {
                            let mut bb_frontier = vec![id];
                            let mut bbs_checked = HashSet::new();
                            let mut val_frontier = Vec::new();
                            let mut vals_checked = HashSet::new();
                            let mut val_used = false;

                            // We search through all nodes reachable from this node.
                            // For each node, we search through all values that are used for anything other than to write to `dest`.
                            'outer: while let Some(bb) = bb_frontier.pop() {
                                if bbs_checked.insert(bb) {
                                    bb_frontier.extend(self[bb].next_blocks().copied());
                                    if let Bb::CommitAndExit {
                                        values,
                                        k,
                                        last_jump_condition,
                                        ..
                                    } = &self[bb]
                                    {
                                        val_frontier.extend(
                                            values
                                                .iter()
                                                .filter(|(d, _)| *d != dest)
                                                .map(|(_, val)| *val)
                                                .chain(k.iter().copied())
                                                .chain(last_jump_condition.iter().copied())
                                                .filter(|&val| vals_checked.insert(val)),
                                        );
                                    } else {
                                        val_frontier.extend(
                                            self[bb].referenced_values().copied().filter(|&val| vals_checked.insert(val)),
                                        );
                                    }
                                }

                                while let Some(val) = val_frontier.pop() {
                                    match value_tree[val] {
                                        // If any loads with dynamic offsets are performed, we have to bail.
                                        ValNode::LoadPtr {
                                            ptr: Ptr::CpuState, ..
                                        } => {
                                            val_used = true;
                                            break 'outer
                                        },
                                        // Any load of a value that overlaps with the commit destination prevents early commits.
                                        ValNode::LoadPtrImm {
                                            ptr: Ptr::CpuState,
                                            offset,
                                            size,
                                        } => {
                                            val_used = offset < dest_offset + dest_size
                                                && offset + size.num_bytes() as u16 > dest_offset;
                                            if val_used {
                                                break 'outer
                                            }
                                        },
                                        _ => (),
                                    }

                                    val_frontier.extend(
                                        value_tree[val]
                                            .referenced_nodes()
                                            .copied()
                                            .filter(|&val| vals_checked.insert(val)),
                                    );
                                }
                            }

                            val_used
                        };

                        trace!("Destination is used in future computations: {val_used}");
                        if val_used {
                            // TODO: Store the original value in a variable and substitute that whenever we would have loaded from the CPU state in memory.
                            continue
                        }

                        let vars = {
                            let mut vars = Vec::new();
                            value_tree.walk(val, |_, val| {
                                if let &ValNode::Var(var) = val {
                                    vars.push(var);
                                }
                            });

                            vars
                        };
                        trace!("Variables used by value {val:?}: {vars:?}");
                        let all_vars_final = vars.iter().all(|var| !vars_stored[id.index()].contains(var));

                        trace!("All variables final: {all_vars_final}");
                        if !all_vars_final {
                            continue
                        }

                        early_commits.push((dest, val));
                        already_committed.insert(dest);
                    }

                    if !early_commits.is_empty() {
                        early_commits.sort_by_key(|&(dest, _)| match dest {
                            CommitDest::Reg(reg) => State::byte_offset_of(reg),
                            CommitDest::Fixed {
                                offset, ..
                            } => offset.into(),
                            CommitDest::Dynamic {
                                ..
                            } => usize::MAX,
                        });
                        let new_block = BbId::from_usize(self.graph.len());
                        let early_commit = Bb::Seq {
                            entry: BbSeq::Commit {
                                values: early_commits,
                            },
                            next: new_block,
                        };

                        let mut tmp = early_commit;
                        std::mem::swap(&mut self.graph[id.index()], &mut tmp);
                        self.graph.push(tmp);
                    }
                }
            }

            for next in original_next_blocks {
                if !already_committed_per_bb[next.index()].is_empty() {
                    for d in already_committed.iter() {
                        assert!(
                            already_committed_per_bb[next.index()].contains(d),
                            "when multiple paths converge, the same values should have been already committed, but {d:?} is missing:\n{:?}\n{already_committed:?}",
                            already_committed_per_bb[next.index()]
                        );
                    }
                }
                already_committed_per_bb[next.index()].extend(&already_committed);
                assert!(
                    already_committed_per_bb[next.index()].len() < 4096,
                    "{:?}",
                    already_committed_per_bb[next.index()]
                );
            }
        }

        info!("Early commit introduction took {}ms", start.elapsed().as_millis());
    }
}

impl crate::codegen::graph_traits::Graph for BbGraph {
    type Index = BbId;
    type Node = Bb;
    const ROOT: Self::Index = BbId(0);

    fn num_nodes(&self) -> usize {
        self.graph.len()
    }

    fn node(&self, index: Self::Index) -> &Self::Node {
        &self.graph[index.index()]
    }
}

impl crate::codegen::graph_traits::Index for BbId {
    fn index(&self) -> usize {
        self.index()
    }

    fn from_usize(val: usize) -> Self {
        Self::from_usize(val)
    }
}

impl crate::codegen::graph_traits::Node<BbId> for Bb {
    fn transitions(&self) -> impl Iterator<Item = BbId> {
        self.next_blocks().copied()
    }
}

pub struct BbBuilder {
    bb_graph: Vec<Option<Bb>>,
    current_block: BbId,
}

impl Default for BbBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl BbBuilder {
    pub fn new() -> Self {
        Self {
            bb_graph: vec![None],
            current_block: BbId::from_usize(0),
        }
    }

    pub fn create_block(&mut self) -> BbId {
        let id = BbId::from_usize(self.bb_graph.len());
        self.bb_graph.push(None);
        id
    }

    pub fn block_is_terminated(&self) -> bool {
        self.bb_graph[self.current_block.index()].is_some()
    }

    pub fn switch_to(&mut self, next: BbId) {
        assert!(self.block_is_terminated(), "block not terminated: {:?}", self.current_block);
        self.current_block = next;
    }

    pub fn write_and_switch_to(&mut self, bb: Bb, next: BbId) {
        trace!("Writing {bb:?} to {:?}", self.current_block);
        assert!(
            self.bb_graph[self.current_block.index()].is_none(),
            "Cannot write into existing block"
        );
        self.bb_graph[self.current_block.index()] = Some(bb);
        self.switch_to(next);
    }

    pub fn write_terminal(&mut self, bb: Bb) {
        self.write_and_switch_to(bb, self.current_block);
    }

    pub fn write_seq(&mut self, entry: BbSeq) {
        let next = self.create_block();
        self.write_and_switch_to(
            Bb::Seq {
                entry,
                next,
            },
            next,
        );
    }

    pub fn build(self, value_tree: &mut ValBuilder) -> BbGraph {
        debug!("Building control flow graph of {} nodes", self.bb_graph.len());
        let mut graph = BbGraph {
            graph: self
                .bb_graph
                .into_iter()
                .enumerate()
                .map(|(index, v)| {
                    if let Some(mut v) = v {
                        if let Bb::Br {
                            cond,
                            if_zero,
                            if_nonzero,
                        } = v
                        {
                            // Optimize conditional jump on constant
                            if let ValNode::Const(c) = value_tree[cond] {
                                v = Bb::Jump(if c == 0 { if_zero } else { if_nonzero })
                            }
                        }

                        v
                    } else {
                        panic!(
                            "Not all basic blocks are filled: block {:?} is empty",
                            BbId::from_usize(index)
                        )
                    }
                })
                .collect(),
        };

        debug!("Optimizing...");
        graph.optimize(value_tree);
        graph
    }
}

#[cfg(test)]
mod tests {
    use test_log::test;

    use crate::arch::intel386::{GpReg, Reg, State};
    use crate::codegen::mir::Mir;
    use crate::codegen::mir::bb::{Bb, BbBuilder, BbFallible, BbId, BbSeq, CommitDest};
    use crate::codegen::mir::val::{ValBuilder, VarId};
    use crate::codegen::{DataSize, Ptr};
    use crate::il::BinOp;

    #[test]
    fn early_commit_optimization() {
        let mut val = ValBuilder::new();
        let mut cf = BbBuilder::new();

        let v = val.load_ptr_imm(Ptr::CpuState, DataSize::Dword, 0);

        cf.write_seq(BbSeq::Log {
            message: String::from("test 1"),
        });
        cf.write_seq(BbSeq::Log {
            message: String::from("test 2"),
        });
        cf.write_terminal(Bb::CommitAndExit {
            metadata: None,
            success: true,
            k: None,
            values: vec![(CommitDest::Reg(Reg::Gp(GpReg::Bx)), v)],
            last_jump_condition: None,
        });

        let cf = cf.build(&mut val);
        let val = val.build();

        let mir = Mir {
            value_tree: val,
            control_flow: cf,
        };

        println!("{mir}");

        assert!(matches!(&mir.control_flow[BbId::from_usize(2)], Bb::CommitAndExit { values, .. } if values.is_empty()));
    }

    #[test]
    fn dependent_store_optimization() {
        let mut val = ValBuilder::new();
        let mut cf = BbBuilder::new();

        // Load two initial values
        let v1 = val.load_ptr_imm(Ptr::CpuState, DataSize::Dword, 0);
        let v2 = val.load_ptr_imm(Ptr::CpuState, DataSize::Dword, 4);

        // Perform a computation: let's say v3 depends on v1 and v2
        let v3 = val.binop(BinOp::Add, [v1, v2]);

        // Log something
        cf.write_seq(BbSeq::Log {
            message: String::from("before dependent store"),
        });

        // Store v1 somewhere
        cf.write_seq(BbSeq::Store {
            values: vec![(VarId::from_usize(0), v1)],
        });

        // Store v3 (dependent on v1 and v2)
        let var1 = VarId::from_usize(1);
        cf.write_seq(BbSeq::Store {
            values: vec![(var1, v3)],
        });

        let var3 = val.use_var(var1);

        // Commit and exit
        cf.write_terminal(Bb::CommitAndExit {
            metadata: None,
            success: true,
            k: None,
            values: vec![
                (CommitDest::Reg(Reg::Gp(GpReg::Ax)), v1),
                (CommitDest::Reg(Reg::Gp(GpReg::Bx)), var3),
            ],
            last_jump_condition: None,
        });

        let cf = cf.build(&mut val);
        let val = val.build();

        let mir = Mir {
            value_tree: val,
            control_flow: cf,
        };

        println!("{mir}");

        // Ensure that variable assignment for BX inhibits early commit
        assert!(matches!(
            &mir.control_flow[BbId::from_usize(3)],
            Bb::CommitAndExit { values, .. } if values.iter().any(|&(dest, _)| dest == CommitDest::Reg(Reg::Gp(GpReg::Bx)))
        ));
    }

    #[test]
    fn multiple_loads_block_early_commit() {
        let mut val = ValBuilder::new();
        let mut cf = BbBuilder::new();

        // Load the same memory location twice
        let load1 = val.load_ptr_imm(
            Ptr::CpuState,
            DataSize::Dword,
            State::byte_offset_of(Reg::Gp(GpReg::Ax)) as u16,
        );
        let load2 = val.load_ptr_imm(
            Ptr::CpuState,
            DataSize::Dword,
            State::byte_offset_of(Reg::Gp(GpReg::Ax)) as u16,
        ); // same address

        // Store both loads to ensure they are not optimized away
        let var1 = VarId::from_usize(0);
        cf.write_seq(BbSeq::Store {
            values: vec![(var1, load1)],
        });

        let var2 = VarId::from_usize(1);
        cf.write_seq(BbSeq::Store {
            values: vec![(var2, load2)],
        });

        let val1 = val.use_var(var1);
        let val2 = val.use_var(var2);

        cf.write_seq(BbSeq::Log {
            message: String::from("test"),
        });

        // Commit and exit after both loads
        cf.write_terminal(Bb::CommitAndExit {
            metadata: None,
            success: true,
            k: None,
            values: vec![
                (CommitDest::Reg(Reg::Gp(GpReg::Ax)), val1),
                (CommitDest::Reg(Reg::Gp(GpReg::Bx)), val2),
            ],
            last_jump_condition: None,
        });

        let cf = cf.build(&mut val);
        let val = val.build();

        let mir = Mir {
            value_tree: val,
            control_flow: cf,
        };

        println!("{mir}");

        assert!(matches!(
            &mir.control_flow[BbId::from_usize(2)],
            Bb::Seq { entry: BbSeq::Commit { values }, .. }
            if values.iter().any(|&(dest, _)| dest == CommitDest::Reg(Reg::Gp(GpReg::Ax))) &&
                values.iter().any(|&(dest, _)| dest == CommitDest::Reg(Reg::Gp(GpReg::Bx)))
        ));
    }

    #[test]
    fn fallible_operation_blocks_early_commit() {
        let mut val = ValBuilder::new();
        let mut cf = BbBuilder::new();

        // Load an initial value
        let v1 = val.load_ptr_imm(Ptr::CpuState, DataSize::Dword, 0);

        // Prepare a fallible operation: a memory read
        let var = VarId::from_usize(1);
        let fallible_read = BbFallible::ReadMemory {
            value: var,
            addr: v1,
            num_bytes: 4,
        };

        // Write a fallible block with two possible exits
        let ok_block = cf.create_block();
        let exc_block = cf.create_block();

        cf.write_and_switch_to(
            Bb::Fallible {
                op: fallible_read,
                if_ok: ok_block,
                if_exception: exc_block,
            },
            ok_block,
        );

        cf.write_terminal(Bb::CommitAndExit {
            metadata: None,
            success: true,
            k: None,
            values: vec![
                (CommitDest::Reg(Reg::Gp(GpReg::Ax)), v1),
                (CommitDest::Reg(Reg::Gp(GpReg::Bx)), val.use_var(var)),
            ],
            last_jump_condition: None,
        });

        cf.switch_to(exc_block);
        let fallback = val.load_ptr_imm(Ptr::CpuState, DataSize::Dword, 4);
        cf.write_terminal(Bb::CommitAndExit {
            metadata: None,
            success: false,
            k: None,
            values: vec![
                (CommitDest::Reg(Reg::Gp(GpReg::Ax)), v1),
                (CommitDest::Reg(Reg::Gp(GpReg::Bx)), fallback),
            ],
            last_jump_condition: None,
        });

        let cf = cf.build(&mut val);
        let val = val.build();

        let mir = Mir {
            value_tree: val,
            control_flow: cf,
        };

        println!("{mir}");

        assert!(matches!(
            &mir.control_flow[BbId::from_usize(0)],
            Bb::Seq { entry: BbSeq::Commit { values }, .. }
            if values.len() == 1 && values.iter().any(|&(dest, _)| dest == CommitDest::Reg(Reg::Gp(GpReg::Ax)))
        ));
        assert!(matches!(
            &mir.control_flow[BbId::from_usize(1)],
            Bb::CommitAndExit { values, .. }
            if !values.iter().any(|&(dest, _)| dest == CommitDest::Reg(Reg::Gp(GpReg::Ax))) &&
                values.iter().any(|&(dest, _)| dest == CommitDest::Reg(Reg::Gp(GpReg::Bx)))
        ));
        assert!(matches!(
            &mir.control_flow[BbId::from_usize(2)],
            Bb::CommitAndExit { values, .. }
            if !values.iter().any(|&(dest, _)| dest == CommitDest::Reg(Reg::Gp(GpReg::Ax))) &&
                values.iter().any(|&(dest, _)| dest == CommitDest::Reg(Reg::Gp(GpReg::Bx)))
        ));
    }

    #[test]
    fn dynamic_commit_destination_blocks_early_commit() {
        let mut val = ValBuilder::new();
        let mut cf = BbBuilder::new();

        let base_val = val.load_ptr_imm(Ptr::CpuState, DataSize::Dword, 0);
        let offset = val.load_ptr_imm(Ptr::CpuState, DataSize::Dword, 4);
        let dyn_offset = val.binop(BinOp::Add, [base_val, offset]);
        let stored_val = val.load_ptr_imm(Ptr::CpuState, DataSize::Dword, 8);

        let var_stored = VarId::from_usize(0);
        cf.write_seq(BbSeq::Store {
            values: vec![(var_stored, stored_val)],
        });

        let val_stored = val.use_var(var_stored);

        cf.write_terminal(Bb::CommitAndExit {
            metadata: None,
            success: true,
            k: None,
            values: vec![(
                CommitDest::Dynamic {
                    size: DataSize::Dword,
                    offset: dyn_offset,
                },
                val_stored,
            )],
            last_jump_condition: None,
        });

        let cf = cf.build(&mut val);
        let val = val.build();

        let mir = Mir {
            value_tree: val,
            control_flow: cf,
        };

        println!("{mir}");

        // Assert that no early commit blocks exist
        for (i, (_, bb)) in mir.control_flow.iter().enumerate() {
            match bb {
                Bb::CommitAndExit {
                    ..
                } => {
                    // Only the last block should be a commit
                    assert_eq!(i, mir.control_flow.len() - 1, "Commit emitted too early at block {i}");
                },
                Bb::Seq {
                    entry: BbSeq::Commit {
                        ..
                    },
                    ..
                } => panic!("No early commit should be done when there are dynamic destinations"),
                _ => {},
            }
        }
    }
}
