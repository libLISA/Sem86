use std::cmp::Ordering;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Debug;
use std::hash::Hash;
use std::marker::PhantomData;

use arrayvec::ArrayVec;
use bitcode::{Decode, Encode};
use components::StronglyConnectedComponents;
use log::{debug, trace};
use node::*;
use partition::Partitions;
use serde::{Deserialize, Serialize};

use crate::Instruction;
use crate::arch::Arch;
use crate::encoding::Encoding;
use crate::encoding::bitpattern::{Bit, MappingOrBitOrder, PartMapping};
use crate::encoding::prefixes::{self, EquivalentPrefixes, PrefixSequence, SubstitutionSequence};
use crate::utils::bitmap::{BitmapSlice, GrowingBitmap};
use crate::utils::stopwatch::Stopwatch;

pub type InstructionSet = InstructionMap<()>;

pub(crate) mod components;
mod node;
pub(crate) mod partition;
pub(crate) mod traits;

impl<'a> arbitrary::Arbitrary<'a> for InstructionMap<()> {
    fn arbitrary(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        let num_nodes = u.int_in_range(1..=100)?;
        let mut nodes = Vec::new();
        for _ in 0..num_nodes {
            let mut next = [NodeId::FAIL; 256];
            for next in next.iter_mut() {
                let n = u.int_in_range(0..=num_nodes + 1)?;
                *next = if n < num_nodes {
                    NodeIndex::new(n).into()
                } else if n == num_nodes {
                    ValIndex::new(0).into()
                } else {
                    NodeId::FAIL
                }
            }

            nodes.push(Node {
                next: Next::new(|b| next[b as usize]),
            });
        }

        Ok(Self {
            nodes,
            values: vec![()],
        })
    }
}

impl<'a, T: PartialEq + Eq + Hash + Clone + Send + Sync> FromIterator<&'a InstructionMap<T>> for InstructionMap<T> {
    fn from_iter<I: IntoIterator<Item = &'a InstructionMap<T>>>(iter: I) -> Self {
        let mut g = InstructionMap::new();
        for item in iter {
            g.union_with(item);
        }

        g
    }
}

impl<T: PartialEq + Eq + Hash + Clone + Send + Sync> FromIterator<InstructionMap<T>> for InstructionMap<T> {
    fn from_iter<I: IntoIterator<Item = InstructionMap<T>>>(iter: I) -> Self {
        let mut g = InstructionMap::new();
        for item in iter {
            g.union_with(&item);
        }

        g
    }
}

impl<T> AsRef<InstructionMap<T>> for InstructionMap<T> {
    fn as_ref(&self) -> &InstructionMap<T> {
        self
    }
}

#[derive(Clone, Debug)]
pub enum Transition<N, R> {
    Next(N),
    Result(R),
    Fail,
}

pub trait GraphBuilder<Ctx: Copy>: Clone + Debug + PartialEq + Eq + Hash {
    type Output: Clone + Debug + PartialEq + Eq + Hash + Send + Sync;

    /// Returns a transition that indicates whether this edge should terminate, and if so, whether it should match and return a value or fail.
    fn next(&self, ctx: Ctx, byte: u8) -> Transition<Self, Self::Output>;
}

pub trait GraphBuilderFrom<Ctx: Copy, Val>: GraphBuilder<Ctx> {
    fn from(ctx: Ctx, val: Val) -> Transition<Self, Self::Output>;
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ChainBuilder<A, B> {
    First(A),
    Second(B),
}

impl<A, B> ChainBuilder<A, B> {
    pub fn new(builder: A) -> Self {
        Self::First(builder)
    }
}

impl<CtxA: Copy, CtxB: Copy, A: GraphBuilder<CtxA>, B: GraphBuilder<CtxB>> GraphBuilder<&(CtxA, CtxB)> for ChainBuilder<A, B>
where
    B: GraphBuilderFrom<CtxB, A::Output>,
{
    type Output = B::Output;

    fn next(&self, ctx: &(CtxA, CtxB), byte: u8) -> Transition<Self, Self::Output> {
        match self {
            ChainBuilder::First(b) => match b.next(ctx.0, byte) {
                Transition::Next(next) => Transition::Next(ChainBuilder::First(next)),
                Transition::Result(val) => match B::from(ctx.1, val) {
                    Transition::Next(next) => match next.next(ctx.1, byte) {
                        Transition::Next(next) => Transition::Next(ChainBuilder::Second(next)),
                        Transition::Result(val) => Transition::Result(val),
                        Transition::Fail => Transition::Fail,
                    },
                    Transition::Result(val) => Transition::Result(val),
                    Transition::Fail => Transition::Fail,
                },
                Transition::Fail => Transition::Fail,
            },
            ChainBuilder::Second(b) => match b.next(ctx.1, byte) {
                Transition::Next(next) => Transition::Next(ChainBuilder::Second(next)),
                Transition::Result(val) => Transition::Result(val),
                Transition::Fail => Transition::Fail,
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct PrefixGraphBuilder<T> {
    node: prefixes::NodeId,
    output: T,
}

impl<T: Clone + Debug + PartialEq + Eq + Hash + Send + Sync> GraphBuilder<&EquivalentPrefixes> for PrefixGraphBuilder<T> {
    type Output = (Option<PrefixSequence>, T);

    fn next(&self, equivalent_prefixes: &EquivalentPrefixes, b: u8) -> Transition<Self, Self::Output> {
        if let SubstitutionSequence::TooLong = equivalent_prefixes.substitution_prefix(self.node) {
            return Transition::Result((None, self.output.clone()))
        }

        match equivalent_prefixes.transition(self.node, b) {
            prefixes::Edge::NotEquivalent => match equivalent_prefixes.substitution_prefix(self.node) {
                SubstitutionSequence::EquivalentTo(substitute_prefix) => {
                    Transition::Result((Some(substitute_prefix.clone()), self.output.clone()))
                },
                SubstitutionSequence::TooLong | SubstitutionSequence::NotEquivalent => Transition::Fail,
            },
            prefixes::Edge::Transition(node) => Transition::Next(Self {
                node,
                output: self.output.clone(),
            }),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct EncodingGraphBuilder<T> {
    index: usize,
    part_values: Vec<GrowingBitmap>,
    output: T,
}

impl<T> EncodingGraphBuilder<T> {
    fn advance_encoding_byte<A: Arch, S, M>(
        index: usize, part_values: &mut [GrowingBitmap], encoding: &Encoding<A, S, M>, b: u8,
    ) -> bool {
        let remaining_bits = &encoding.bits[..encoding.bits.len() - (index + 1) * 8];
        let bits = &encoding.bits[encoding.bits.len() - (index + 1) * 8..encoding.bits.len() - index * 8];
        for (index, bit) in bits.iter().enumerate() {
            match bit.into() {
                Bit::Fixed(val) => {
                    if val != (b >> index) & 1 {
                        // Early return if the bits don't match
                        return false
                    }
                },
                Bit::Part(part_index) => {
                    let map = &mut part_values[part_index as usize];
                    if !map.is_empty() {
                        let bit_in_part = bits.iter().take(index).chain(remaining_bits).filter(|b| *b == bit).count();
                        let current_val = (b >> index) & 1;
                        let mut any = false;

                        trace!("bit {index} in byte: part #{part_index} (bit {bit_in_part} in part): {map:?}");

                        for n in 0..map.len() {
                            if (n >> bit_in_part) as u8 & 1 == current_val {
                                any |= map.get(n);
                            } else {
                                map.reset(n);
                            }
                        }

                        trace!("resulting part #{part_index}: {map:?}");

                        if !any && !map.is_empty() {
                            return false
                        }
                    }
                },
                _ => (),
            }
        }

        true
    }

    fn extract_initial_part_values<A: Arch, S, M>(encoding: &Encoding<A, S, M>) -> Vec<GrowingBitmap> {
        encoding
            .parts
            .iter()
            .map(|p| match &p.mapping {
                PartMapping::Imm {
                    mapping, ..
                } => {
                    if let Some(MappingOrBitOrder::Mapping(mapping)) = mapping {
                        if mapping.iter().any(|v| !v.is_valid()) {
                            mapping.iter().map(|v| v.is_valid()).collect::<GrowingBitmap>()
                        } else {
                            GrowingBitmap::new()
                        }
                    } else {
                        GrowingBitmap::new()
                    }
                },
                PartMapping::MemoryComputation {
                    mapping,
                } => {
                    if mapping.iter().any(|v| v.is_none()) {
                        mapping.iter().map(|v| v.is_some()).collect::<GrowingBitmap>()
                    } else {
                        GrowingBitmap::new()
                    }
                },
                PartMapping::Register {
                    mapping,
                } => {
                    if mapping.iter().any(|v| v.is_none()) {
                        mapping.iter().map(|v| v.is_some()).collect::<GrowingBitmap>()
                    } else {
                        GrowingBitmap::new()
                    }
                },
            })
            .collect::<Vec<_>>()
    }
}

impl<A: Arch, S, M, T: Clone + Debug + PartialEq + Eq + Hash + Send + Sync>
    GraphBuilderFrom<&Encoding<A, S, M>, (Option<PrefixSequence>, T)> for EncodingGraphBuilder<T>
{
    fn from(
        encoding: &Encoding<A, S, M>, (substitute_prefix, output): (Option<PrefixSequence>, T),
    ) -> Transition<Self, Self::Output> {
        if let Some(substitute_prefix) = substitute_prefix {
            <Self as GraphBuilderFrom<_, _>>::from(encoding, (substitute_prefix, output))
        } else {
            let num_encoding_bytes = encoding.bits.len() / 8;
            let len = encoding.equivalent_prefixes.num_bytes_to_replace();
            if len == num_encoding_bytes {
                Transition::Result(output)
            } else {
                Transition::Next(EncodingGraphBuilder {
                    index: len,
                    part_values: Self::extract_initial_part_values(encoding),
                    output,
                })
            }
        }
    }
}

impl<A: Arch, S, M, T: Clone + Debug + PartialEq + Eq + Hash + Send + Sync>
    GraphBuilderFrom<&Encoding<A, S, M>, (PrefixSequence, T)> for EncodingGraphBuilder<T>
{
    fn from(encoding: &Encoding<A, S, M>, (substitute_prefix, output): (PrefixSequence, T)) -> Transition<Self, Self::Output> {
        let num_encoding_bytes = encoding.bits.len() / 8;
        let mut part_values = Self::extract_initial_part_values(encoding);

        for (index, &b) in substitute_prefix.bytes().iter().enumerate() {
            if !Self::advance_encoding_byte(index, &mut part_values, encoding, b) {
                return Transition::Fail
            }
        }

        if substitute_prefix.len() == num_encoding_bytes {
            Transition::Result(output)
        } else {
            Transition::Next(EncodingGraphBuilder {
                index: substitute_prefix.len(),
                part_values,
                output,
            })
        }
    }
}

impl<A: Arch, S, M, T: Clone + Debug + PartialEq + Eq + Hash + Send + Sync> GraphBuilder<&Encoding<A, S, M>>
    for EncodingGraphBuilder<T>
{
    type Output = T;

    fn next(&self, encoding: &Encoding<A, S, M>, b: u8) -> Transition<Self, Self::Output> {
        let num_encoding_bytes = encoding.bits.len() / 8;
        let mut part_values = self.part_values.clone();
        if Self::advance_encoding_byte(self.index, &mut part_values, encoding, b) {
            if self.index + 1 == num_encoding_bytes {
                Transition::Result(self.output.clone())
            } else {
                Transition::Next(EncodingGraphBuilder {
                    index: self.index + 1,
                    part_values,
                    output: self.output.clone(),
                })
            }
        } else {
            Transition::Fail
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
struct GraphTerminator<T>(PhantomData<T>);

impl<Ctx: Copy, T: Clone + Debug + PartialEq + Eq + Hash + Send + Sync> GraphBuilderFrom<Ctx, (Option<T>, ())>
    for GraphTerminator<T>
{
    fn from(_ctx: Ctx, (val, _): (Option<T>, ())) -> Transition<Self, Self::Output> {
        match val {
            Some(val) => Transition::Result(val),
            None => Transition::Fail,
        }
    }
}

impl<Ctx: Copy, T: Clone + Debug + PartialEq + Eq + Hash + Send + Sync> GraphBuilder<Ctx> for GraphTerminator<T> {
    type Output = T;

    fn next(&self, _ctx: Ctx, _byte: u8) -> Transition<Self, Self::Output> {
        unreachable!()
    }
}

#[derive(Clone, Serialize, Deserialize, Encode, Decode)]
pub struct InstructionMap<T> {
    // TODO: Nodes should not be empty when we match all, because we still need to map to a value.
    nodes: Vec<Node>,
    values: Vec<T>,
}

impl<T> Debug for InstructionMap<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map()
            .entries(
                self.nodes
                    .iter()
                    .enumerate()
                    .map(|(index, node)| (NodeIndex::new(index), node)),
            )
            .finish()
    }
}

impl<T: PartialEq + Eq + Hash + Clone + Send + Sync> Default for InstructionMap<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoReachable {
    None,
    Single(NodeId),
    Many,
}

impl CoReachable {
    fn add(&mut self, next: CoReachable) {
        use CoReachable::*;
        match (*self, next) {
            (Single(_), None) | (Many, _) | (None, None) => (),
            (Single(node_id), Single(next_id)) if node_id == next_id => (),
            (Single(_), Single(_) | Many) => *self = Many,
            (None, next) => *self = next,
        }
    }

    fn is_top(&self) -> bool {
        matches!(self, CoReachable::Many)
    }
}

impl From<EquivalentPrefixes> for InstructionMap<PrefixSequence> {
    fn from(value: EquivalentPrefixes) -> Self {
        Self::build(
            &(&value, ()),
            ChainBuilder::<PrefixGraphBuilder<()>, GraphTerminator<PrefixSequence>>::new(PrefixGraphBuilder {
                node: prefixes::NodeId::ROOT,
                output: (),
            }),
        )
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LookupResult<T> {
    Found(T),
    NeedMoreBytes,
    NotPresent,
}

impl<T: PartialEq + Eq + Hash + Clone + Send + Sync> InstructionMap<T> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            nodes: vec![Node {
                next: Next::ALL_FAIL,
            }],
            values: Vec::new(),
        }
    }

    #[must_use]
    pub fn matching_any(val: T) -> Self {
        Self {
            nodes: vec![Node {
                next: Next::new(|_| ValIndex::new(0).into()),
            }],
            values: vec![val],
        }
    }

    #[must_use]
    pub fn is_all_fail(&self) -> bool {
        self.nodes == Self::new().nodes
    }

    #[must_use]
    pub fn build<Ctx: Copy>(context: Ctx, root: impl GraphBuilder<Ctx, Output = T>) -> Self
    where
        T: Debug,
    {
        let mut graph = InstructionMap::new();
        let mut value_indices = HashMap::new();

        let mut id_map = HashMap::new();
        let pos = root;

        id_map.insert(pos.clone(), NodeIndex::ROOT);
        let mut frontier = vec![(pos, NodeIndex::ROOT)];

        while let Some((pos, id)) = frontier.pop() {
            graph.nodes[id].next = Next::new(|b| {
                let transition = pos.next(context, b);
                let id = match transition {
                    Transition::Result(val) => ValIndex::new(*value_indices.entry(val.clone()).or_insert_with(|| {
                        let index = graph.values.len();
                        graph.values.push(val);
                        index
                    }))
                    .into(),
                    Transition::Next(next_position) => {
                        let new_node = *id_map.entry(next_position).or_insert_with_key(|key| {
                            let id = NodeIndex::new(graph.nodes.len());
                            graph.nodes.push(Node {
                                next: Next::ALL_FAIL,
                            });
                            frontier.push((key.clone(), id));

                            id
                        });

                        new_node.into()
                    },
                    Transition::Fail => NodeId::FAIL,
                };

                if id != NodeId::FAIL {
                    trace!("0x{b:02X} => {id:?}");
                }

                id
            });
        }

        debug!("Created graph: {graph:#?}");
        graph.optimize();

        graph
    }

    pub fn create_from_instruction(instr: Instruction, val: T) -> Self {
        Self {
            nodes: instr
                .bytes()
                .iter()
                .enumerate()
                .map(|(index, &byte)| {
                    let next = Next::new(|b| {
                        if b == byte {
                            if index == instr.byte_len() - 1 {
                                ValIndex::new(0).into()
                            } else {
                                NodeIndex::new(index + 1).into()
                            }
                        } else {
                            NodeId::FAIL
                        }
                    });

                    Node {
                        next,
                    }
                })
                .collect::<Vec<_>>(),
            values: vec![val],
        }
    }

    #[must_use]
    pub fn create_from_encoding<A: Arch, S, M>(encoding: &Encoding<A, S, M>, val: T) -> Self
    where
        T: Debug,
    {
        let p = &encoding.equivalent_prefixes;

        Self::build(
            &(p, encoding),
            ChainBuilder::<PrefixGraphBuilder<T>, EncodingGraphBuilder<T>>::new(PrefixGraphBuilder {
                node: prefixes::NodeId::ROOT,
                output: val,
            }),
        )
    }

    #[must_use]
    pub fn num_nodes(&self) -> usize {
        self.nodes.len()
    }

    #[must_use]
    pub fn is_all_done(&self) -> bool {
        self.num_nodes() == 1
            && self.nodes[0]
                .next
                .iter()
                .all(|(_, node)| matches!(node.unpack(), Some(UnpackedNodeId::ValIndex(_))))
    }

    fn compute_co_reachability(&mut self) -> (Vec<CoReachable>, bool) {
        let mut co_reachability = vec![CoReachable::None; self.nodes.len()];
        let mut any_remappable = false;

        StronglyConnectedComponents::iterate(self, |group| {
            let mut reachability = CoReachable::None;

            // We know all successors of this strongly connected group have already been iterated.
            // This means that we can compute the reachability for the group in one go.
            // All non-looping leaf nodes will have their reachability set correctly.
            // All nodes in this group currently have their reachability set to None.
            'outer: for node_index in group {
                let node = &self.nodes[node_index.index()];
                for (_, next_node) in node.next.iter() {
                    match next_node.unpack() {
                        Some(UnpackedNodeId::NodeIndex(next)) => {
                            reachability.add(co_reachability[next.index()]);
                        },
                        _ => reachability.add(CoReachable::Single(next_node)),
                    }

                    if reachability.is_top() {
                        break 'outer
                    }
                }
            }

            any_remappable |= matches!(reachability, CoReachable::Single(_));

            for node_index in group {
                co_reachability[node_index.index()] = reachability;
            }
        });

        (co_reachability, any_remappable)
    }

    pub fn map(&mut self, map: impl FnMut(T) -> T) {
        self.values = self.values.drain(..).map(map).collect::<Vec<_>>();

        self.optimize();
    }

    pub fn map_typechange<Q: PartialEq + Eq + Hash + Clone + Send + Sync>(
        mut self, map: impl FnMut(T) -> Q,
    ) -> InstructionMap<Q> {
        let mut result = InstructionMap {
            nodes: self.nodes,
            values: self.values.drain(..).map(map).collect::<Vec<_>>(),
        };
        result.optimize();
        result
    }

    pub fn optimize(&mut self) {
        if self.nodes.is_empty() {
            return
        }

        let start = Stopwatch::now();
        debug!("Optimizing {} values...", self.values.len());
        let mut val_map = HashMap::with_capacity(self.values.len());
        let mut new_values = Vec::new();
        let original_value_len = self.values.len();
        let value_remap = self
            .values
            .drain(..)
            .map(|val| {
                *val_map.entry(val.clone()).or_insert_with(|| {
                    let index = new_values.len();
                    new_values.push(val);
                    ValIndex::new(index).into()
                })
            })
            .collect::<Vec<_>>();

        // If the number of values did not change, then `new_values == old(self.values)`.
        // When this is the case, we do not need to remap.
        if new_values.len() != original_value_len {
            for node in self.nodes.iter_mut() {
                node.next.remap_value_indices(|v| value_remap[v.index()]);
            }
        }

        self.values = new_values;
        debug!("Unique values remaining: {}", self.values.len());
        let value_remap_time = start.elapsed().as_millis();

        // TODO: Make this faster.
        // TODO: Can we skip some nodes that were not changed?

        debug!("Optimizing ({} nodes): {self:#?}", self.nodes.len());
        debug!("Pruning all-done nodes");

        // We are going to perform a variation of Hopcroft's DFA minimization algorithm.
        debug!("Performing reachability analysis...");

        let s = Stopwatch::now();
        // TODO: Compute this together with partitioning.
        let (co_reachable, any_remappable) = self.compute_co_reachability();
        let co_reachable_node_id_time = s.elapsed().as_millis();
        trace!("Co-reachability: {co_reachable:?}");

        let s = Stopwatch::now();

        if any_remappable {
            for node in self.nodes.iter_mut() {
                node.next.remap_node_indices(|node| {
                    if let CoReachable::Single(next_node) = co_reachable[node.index()] {
                        match next_node.unpack() {
                            Some(UnpackedNodeId::NodeIndex(_)) => node.into(),
                            Some(UnpackedNodeId::ValIndex(val)) => val.into(),
                            None => NodeId::FAIL,
                        }
                    } else {
                        node.into()
                    }
                });
            }
        }

        let remapping_fail_unreachable_time = s.elapsed().as_millis();
        let s = Stopwatch::now();

        // Check if graph matches everything
        if let CoReachable::Single(node) = co_reachable[0]
            && let Some(UnpackedNodeId::ValIndex(val)) = node.unpack()
            && co_reachable.iter().all(|&c| c == CoReachable::Single(node))
        {
            self.nodes.clear();
            self.nodes.push(Node {
                next: Next::new(|_| val.into()),
            });
            return
        }

        debug!("Computing initial node partitioning...");

        let partitions = Partitions::of(self);
        match partitions.canonical_node_of(NodeIndex::ROOT).map(|n| n.unpack()) {
            Some(Some(UnpackedNodeId::ValIndex(val))) => {
                debug!("Optimized graph maps all instructions to {val:?} -- reducing to 1 node");
                self.nodes.clear();
                self.nodes.push(Node {
                    next: Next::new(|_| val.into()),
                });
                return
            },
            Some(None) => {
                debug!("Optimized graph matches no instructions -- reinitializing to default state");
                *self = InstructionMap::new();
                return
            },
            _ => (),
        }

        assert_eq!(
            partitions.canonical_node_of(NodeIndex::ROOT),
            Some(NodeIndex::new(0).into()),
            "root node must always stay root node"
        );
        let partitioning_time = s.elapsed().as_millis();
        if partitions.no_optimizations_possible() {
            debug!("No optimizations possible");

            debug!(
                "InstructionSet optimization timings: Remapped values in {value_remap_time}ms, identified fail-reachable nodes in {co_reachable_node_id_time}ms, remapped them in {remapping_fail_unreachable_time}ms, partitioned all nodes in {partitioning_time}ms, aborted early, total time taken: {}ms",
                start.elapsed().as_millis()
            );
            return
        }

        let s = Stopwatch::now();
        let remap_needed = (0..self.nodes.len())
            .map(NodeIndex::new)
            .any(|n| partitions.canonical_node_of(n) != Some(n.into()));

        let mut n = 0;
        self.nodes.retain_mut(|node| {
            let keep = partitions.should_keep(NodeIndex::new(n));
            n += 1;

            if keep && remap_needed {
                node.next
                    .remap_node_indices(|node_id| partitions.canonical_node_of(node_id).unwrap());
            }

            keep
        });

        let remapping_time = s.elapsed().as_millis();
        debug!("Optimized: {self:#?}");

        debug!(
            "InstructionSet optimization timings: Remapped values in {value_remap_time}ms, identified fail-reachable nodes in {co_reachable_node_id_time}ms, remapped them in {remapping_fail_unreachable_time}ms, partitioned all nodes in {partitioning_time}ms, remapped in {remapping_time}ms, total time taken: {}ms",
            start.elapsed().as_millis()
        );
    }

    /// Returns true if the provided instruction `instr` is filtered by the graph.
    pub fn get(&self, instr: Instruction) -> LookupResult<&T> {
        self.get_with_node_id(instr).1
    }

    /// Returns true if the provided instruction `instr` is filtered by the graph.
    pub fn get_with_node_id(&self, instr: Instruction) -> (NodeIndex, LookupResult<&T>) {
        let mut node = NodeIndex::ROOT;
        for &byte in instr.bytes() {
            match self.nodes[node].next.get(byte).unpack() {
                Some(UnpackedNodeId::ValIndex(index)) => return (node, LookupResult::Found(&self.values[index.index()])),
                Some(UnpackedNodeId::NodeIndex(next)) => node = next,
                None => return (node, LookupResult::NotPresent),
            }
        }

        (node, LookupResult::NeedMoreBytes)
    }

    /// Looks up an instruction, reading the instruction byte-for-byte.
    #[inline(always)]
    pub fn get_iteratively<E>(&self, mut next_byte: impl FnMut() -> Result<u8, E>) -> Result<Option<(Instruction, &T)>, E> {
        let mut node = NodeIndex::ROOT;
        let mut instr = ArrayVec::<u8, 16>::new();
        loop {
            let byte = next_byte()?;
            instr.push(byte);
            match self.nodes[node].next.get(byte).unpack() {
                Some(UnpackedNodeId::ValIndex(index)) => return Ok(Some((Instruction::new(&instr), &self.values[index.index()]))),
                Some(UnpackedNodeId::NodeIndex(next)) => node = next,
                None => return Ok(None),
            }
        }
    }

    // TODO: Rename contains
    /// Returns true if the provided instruction `instr` is filtered by the graph.
    pub fn matches(&self, instr: Instruction) -> bool {
        matches!(self.get(instr), LookupResult::Found(_))
    }

    pub fn is_empty(&self) -> bool {
        // This assumes that all nodes are reachable, which holds:
        // The methods that may modify the graph are union(), create_from_filter_below/above and intersect().
        // The invariant is guaranteed by optimize(), which is called after a union() and create_from_filter_below/above.
        // The invariant is guaranteed by intersect(), because it produces the intersection of two graphs for which the invariant holds.
        !self.nodes.iter().any(|node| node.next.any_done())
    }

    fn transitions(&self, b: u8, node: Option<NodeId>) -> Option<NodeId> {
        node.and_then(|node| match node.unpack() {
            Some(UnpackedNodeId::ValIndex(v)) => Some(v.into()),
            Some(UnpackedNodeId::NodeIndex(node)) => match self.nodes[node].next.get(b).unpack() {
                Some(UnpackedNodeId::ValIndex(v)) => Some(v.into()),
                Some(UnpackedNodeId::NodeIndex(node)) => Some(node.into()),
                None => None,
            },
            None => None,
        })
    }

    pub fn union_with(&mut self, other: &InstructionMap<T>) {
        self.union_with_report_overlapping(other, |_, _| ())
    }

    pub fn union_with_report_overlapping(&mut self, other: &InstructionMap<T>, mut overlap: impl FnMut(&T, &T)) {
        trace!("Unioning with {other:#?}");
        let start = Stopwatch::now();
        // If either graphs matches none, return the other graph.
        if self.is_all_fail() {
            *self = other.clone();
            return
        } else if other.is_all_fail() {
            return
        }

        #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        struct T {
            lhs: Option<NodeId>,
            rhs: Option<NodeId>,
        }

        // Compute new indices for the RHS values, deduplicating when possible.
        let mut val_map = self
            .values
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, val)| (val, ValIndex::new(index)))
            .collect::<HashMap<_, _>>();
        let rhs_value_remap = other
            .values
            .iter()
            .map(|val| {
                *val_map.entry(val.clone()).or_insert_with(|| {
                    let index = self.values.len();
                    self.values.push(val.clone());
                    ValIndex::new(index)
                })
            })
            .collect::<Vec<_>>();

        let old_root = NodeIndex::new(self.nodes.len());
        self.nodes.push(self.nodes[0].clone());
        for node in self.nodes.iter_mut() {
            node.next
                .remap_node_indices(|index| if index == NodeIndex::ROOT { old_root } else { index }.into());
        }

        trace!("Remapped old root to {old_root:?}: {self:#?}");

        let mut id_map = HashMap::new();
        let t = T {
            lhs: Some(NodeId::ROOT),
            rhs: Some(NodeId::ROOT),
        };
        id_map.insert(t.clone(), NodeIndex::ROOT);
        let mut frontier = vec![(t, NodeIndex::ROOT)];

        while let Some((t, id)) = frontier.pop() {
            let next = Next::new(|b| {
                let lhs = self.transitions(b, t.lhs);
                let rhs = other.transitions(b, t.rhs);

                trace!("For {t:?} transitioning over 0x{b:02X} ({id:?}): {lhs:?} : {rhs:?}");

                if let Some(Some(UnpackedNodeId::ValIndex(rhs_index))) = rhs.map(|v| v.unpack()) {
                    if let Some(Some(UnpackedNodeId::ValIndex(lhs_index))) = lhs.map(|v| v.unpack()) {
                        overlap(&self.values[lhs_index.index()], &other.values[rhs_index.index()])
                    }

                    rhs_value_remap[rhs_index.index()].into()
                } else if let Some(Some(UnpackedNodeId::ValIndex(index))) = lhs.map(|v| v.unpack()) {
                    index.into()
                } else if lhs.is_none() && rhs.is_none() {
                    NodeId::FAIL
                } else if rhs.is_none() {
                    lhs.unwrap()
                } else {
                    (*id_map
                        .entry(T {
                            lhs,
                            rhs,
                        })
                        .or_insert_with_key(|key| {
                            let id = NodeIndex::new(self.nodes.len());
                            self.nodes.push(Node {
                                next: Next::ALL_FAIL,
                            });
                            frontier.push((key.clone(), id));

                            trace!("{key:?} becomes new node {id:?}");

                            id
                        }))
                    .into()
                }
            });

            self.nodes[id].next = next;
        }

        debug!("Union took {}ms", start.elapsed().as_millis());

        self.optimize();
    }

    // TODO: Remove in favor of union_with
    pub fn union(&self, other: &InstructionMap<T>) -> InstructionMap<T> {
        let mut result = self.clone();
        result.union_with(other);
        result
    }

    pub fn values(&self) -> impl Iterator<Item = &T> {
        self.values.iter()
    }

    /// Shinks the capacity of the interal datastructures as much as possible.
    pub fn shrink_to_fit(&mut self) {
        self.nodes.shrink_to_fit()
    }
}