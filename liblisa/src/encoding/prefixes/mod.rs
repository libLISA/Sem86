use core::panic;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::fmt::{Debug, Display};
use std::hash::Hash;
use std::ops::{Index, IndexMut};

use arrayvec::ArrayVec;
use bitcode::{Decode, Encode};
use itertools::Itertools;
use log::trace;
use serde::{Deserialize, Serialize};

use crate::Instruction;
use crate::instr::map::partition::Partitions;
use crate::instr::map::traits;
use crate::utils::bitmap::{Bitmap, GrowingBitmap};

#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Clone, Serialize, Deserialize, Encode, Decode, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PrefixSequence(ArrayVec<u8, 14>);

#[cfg(feature = "mem_dbg")]
impl mem_dbg::MemSize for PrefixSequence {
    fn mem_size(&self, _flags: mem_dbg::SizeFlags) -> usize {
        size_of::<Self>()
    }
}

#[cfg(feature = "mem_dbg")]
impl mem_dbg::CopyType for PrefixSequence {
    type Copy = mem_dbg::True;
}

impl Debug for PrefixSequence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Display::fmt(self, f)
    }
}

impl Display for PrefixSequence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_empty() {
            write!(f, "ϵ")
        } else {
            write!(f, "{:02X}", self.0.iter().format(""))
        }
    }
}

impl PrefixSequence {
    #[must_use]
    pub fn new(bytes: impl IntoIterator<Item = u8>) -> Self {
        Self(bytes.into_iter().collect())
    }

    #[must_use]
    pub fn one(val: u8) -> Self {
        Self::new([val])
    }

    #[must_use]
    pub fn chain(&self, other: &PrefixSequence) -> Self {
        match self.try_chain(other) {
            Some(seq) => seq,
            None => panic!("unable to chain prefixes, resulting sequence is too long: {self:?} . {other:?}"),
        }
    }

    #[must_use]
    pub fn try_chain(&self, other: &PrefixSequence) -> Option<Self> {
        if self.len() + other.len() < 15 {
            Some(Self::new(self.0.iter().chain(other.0.iter()).copied()))
        } else {
            None
        }
    }

    #[must_use]
    pub fn chain_one(&self, val: u8) -> Self {
        self.chain(&Self::one(val))
    }

    #[must_use]
    pub fn empty() -> Self {
        Self(ArrayVec::new())
    }

    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.0
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn removed(&self, pos: usize) -> PrefixSequence {
        PrefixSequence::new(
            self.bytes()
                .iter()
                .copied()
                .enumerate()
                .filter(|&(index, _)| index != pos)
                .map(|(_, byte)| byte),
        )
    }
}

#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "mem_dbg", derive(mem_dbg::MemSize))]
#[derive(Copy, Clone, Debug, Serialize, Deserialize, Encode, Decode, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(transparent)]
pub struct NodeId(u16);

impl NodeId {
    pub const ROOT: NodeId = NodeId(0);

    pub fn index(&self) -> usize {
        self.0 as usize
    }

    pub fn new(index: usize) -> Self {
        Self(index.try_into().unwrap())
    }
}

impl Index<NodeId> for Vec<Node> {
    type Output = Node;

    fn index(&self, index: NodeId) -> &Self::Output {
        &self[index.index()]
    }
}

impl IndexMut<NodeId> for Vec<Node> {
    fn index_mut(&mut self, index: NodeId) -> &mut Self::Output {
        &mut self[index.index()]
    }
}

#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "mem_dbg", derive(mem_dbg::MemSize))]
#[derive(Copy, Clone, Debug, Serialize, Deserialize, Encode, Decode, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Edge {
    NotEquivalent,
    Transition(NodeId),
}

#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "mem_dbg", derive(mem_dbg::MemSize))]
#[derive(Clone, Serialize, Deserialize, Encode, Decode, PartialEq, Eq, Hash)]
pub struct Node {
    substitute_sequence: SubstitutionSequence,
    edges: Next,
}

impl Node {
    pub fn substitute_sequence(&self) -> &SubstitutionSequence {
        &self.substitute_sequence
    }

    pub fn next(&self) -> &Next {
        &self.edges
    }
}

#[cfg_attr(feature = "mem_dbg", derive(mem_dbg::MemSize))]
#[derive(Clone, PartialEq, Eq, Hash, Encode, Decode)]
pub struct Next([Edge; 256]);

impl Next {
    pub fn edge(&self, byte: u8) -> Edge {
        self.0[byte as usize]
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &Edge> + use<'_> {
        self.0.iter()
    }

    fn iter_mut(&mut self) -> impl Iterator<Item = &mut Edge> + use<'_> {
        self.0.iter_mut()
    }
}

#[cfg(feature = "schemars")]
impl schemars::JsonSchema for Next {
    fn schema_name() -> String {
        "Next".to_string()
    }

    fn json_schema(_gen: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
        todo!()
    }
}

impl Index<usize> for Next {
    type Output = Edge;

    fn index(&self, index: usize) -> &Self::Output {
        &self.0[index]
    }
}

impl IndexMut<usize> for Next {
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        &mut self.0[index]
    }
}

impl Serialize for Next {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.0.as_slice().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Next {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let data = Vec::<Edge>::deserialize(deserializer)?;

        Ok(Next(
            data.try_into().expect("unable to convert vec into array of 256 elements"),
        ))
    }
}

struct DisplayBytes(Vec<u8>);

impl Debug for DisplayBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.iter().format_with(" ", |b, f| f(&format_args!("{b:02X}"))))
    }
}

impl Debug for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut targets = HashMap::new();

        for (byte, next) in self.edges.iter().enumerate() {
            if next != &Edge::NotEquivalent {
                targets.entry(next).or_insert_with(Vec::new).push(byte as u8);
            }
        }

        let mut f = f.debug_map();
        for (next, bytes) in targets.into_iter().sorted_by_key(|(k, _)| *k) {
            f.entry(&DisplayBytes(bytes), &next);
        }

        f.finish()
    }
}

/// A graph that represents all equivalent prefixes.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "mem_dbg", derive(mem_dbg::MemSize))]
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Encode, Decode)]
pub struct EquivalentPrefixes {
    nodes: Vec<Node>,
    num_bytes_to_replace: usize,
}

impl Debug for EquivalentPrefixes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map()
            .entries(
                self.nodes
                    .iter()
                    .enumerate()
                    .map(|(index, node)| ((NodeId::new(index), &node.substitute_sequence), node)),
            )
            .finish()
    }
}

#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "mem_dbg", derive(mem_dbg::MemSize))]
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode)]
pub enum SubstitutionSequence {
    EquivalentTo(PrefixSequence),
    NotEquivalent,
    TooLong,
}

#[derive(Serialize, Deserialize)]
enum SubstitutionSequenceSerializationHelper {
    TooLong,
    EquivalentTo(PrefixSequence),
}

impl Serialize for SubstitutionSequence {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let helper = match self {
            SubstitutionSequence::EquivalentTo(prefix_sequence) => {
                Some(SubstitutionSequenceSerializationHelper::EquivalentTo(prefix_sequence.clone()))
            },
            SubstitutionSequence::NotEquivalent => None,
            SubstitutionSequence::TooLong => Some(SubstitutionSequenceSerializationHelper::TooLong),
        };

        helper.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SubstitutionSequence {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(
            match Option::<SubstitutionSequenceSerializationHelper>::deserialize(deserializer)? {
                Some(SubstitutionSequenceSerializationHelper::TooLong) => SubstitutionSequence::TooLong,
                Some(SubstitutionSequenceSerializationHelper::EquivalentTo(prefix_sequence)) => {
                    SubstitutionSequence::EquivalentTo(prefix_sequence)
                },
                None => SubstitutionSequence::NotEquivalent,
            },
        )
    }
}

impl SubstitutionSequence {
    pub fn is_prefix_sequence(&self) -> bool {
        matches!(self, Self::EquivalentTo(_))
    }

    pub fn as_prefix_sequence(&self) -> Option<&PrefixSequence> {
        if let Self::EquivalentTo(seq) = self { Some(seq) } else { None }
    }

    pub fn as_mut_prefix_sequence(&mut self) -> Option<&mut PrefixSequence> {
        if let Self::EquivalentTo(seq) = self { Some(seq) } else { None }
    }
}

#[derive(Clone, Debug, Default)]
pub struct DotConfig {
    pub bgcolor: Option<String>,
}

impl EquivalentPrefixes {
    pub fn new_matching_empty_sequence(num_bytes_to_replace: usize) -> Self {
        Self {
            nodes: vec![Node {
                substitute_sequence: SubstitutionSequence::EquivalentTo(PrefixSequence::empty()),
                edges: Next([Edge::NotEquivalent; 256]),
            }],
            num_bytes_to_replace,
        }
    }

    pub fn new_matching_none(num_bytes_to_replace: usize) -> Self {
        Self::new_matching_none_with_prefixes(num_bytes_to_replace, 0..=0xff)
    }

    pub fn new_matching_none_with_prefixes(num_bytes_to_replace: usize, prefixes: impl IntoIterator<Item = u8>) -> Self {
        let mut next = [Edge::NotEquivalent; 256];
        for prefix in prefixes.into_iter() {
            next[prefix as usize] = Edge::Transition(NodeId::ROOT);
        }

        Self {
            nodes: vec![Node {
                substitute_sequence: SubstitutionSequence::NotEquivalent,
                edges: Next(next),
            }],
            num_bytes_to_replace,
        }
    }

    pub fn from_edges(
        num_bytes: usize, nodes: impl IntoIterator<Item = SubstitutionSequence>,
        edges: impl IntoIterator<Item = (usize, u8, usize)>,
    ) -> Self {
        let mut graph = Self::new_matching_empty_sequence(num_bytes);
        graph.nodes = nodes
            .into_iter()
            .map(|substitute_sequence| Node {
                substitute_sequence,
                edges: Next([Edge::NotEquivalent; 256]),
            })
            .collect::<Vec<_>>();

        for (from, b, to) in edges.into_iter() {
            assert!(from < graph.nodes.len());
            assert!(to < graph.nodes.len());
            graph.nodes[from].edges[b as usize] = Edge::Transition(NodeId::new(to));
        }

        graph.optimize();

        graph
    }

    /// Replaces substitution sequences with their canonical (shortest-path) equivalents.
    pub fn canonicalize_substitution_sequences(&mut self) {
        // Map to store the shortest known path to each substitution sequence
        let mut canonical_map: HashMap<PrefixSequence, PrefixSequence> = HashMap::new();
        let mut visited = GrowingBitmap::new_all_zeros(self.nodes.len());

        // BFS queue: (node_id, path_so_far)
        let mut queue = VecDeque::new();
        queue.push_back((NodeId::ROOT, PrefixSequence::new([])));

        while let Some((node_id, path)) = queue.pop_front() {
            if !visited.set(node_id.index()) {
                continue
            }

            // If the path we used to get here is shorter than the current canonical sequence, update it.
            let node = &self.nodes[node_id];
            if let SubstitutionSequence::EquivalentTo(seq) = node.substitute_sequence() {
                let entry = canonical_map.entry(seq.clone()).or_insert_with(|| path.clone());
                if path.len() < entry.len() {
                    *entry = path.clone();
                }
            }

            for (byte, edge) in node.next().iter().enumerate() {
                if let Edge::Transition(next_id) = edge {
                    queue.push_back((*next_id, path.chain_one(byte as u8)));
                }
            }
        }

        // Update all nodes with their canonical substitution sequence
        for node in &mut self.nodes {
            if let SubstitutionSequence::EquivalentTo(seq) = &mut node.substitute_sequence
                && let Some(canonical) = canonical_map.get(seq)
            {
                *seq = canonical.clone();
            }
        }

        self.optimize();
    }

    pub(crate) fn transition(&self, node: NodeId, b: u8) -> Edge {
        self.nodes[node.index()].edges[b as usize]
    }

    pub fn num_bytes_to_replace(&self) -> usize {
        self.num_bytes_to_replace
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.len() == 1 && self.nodes[0].edges.iter().all(|e| e == &Edge::NotEquivalent)
    }

    pub(crate) fn substitution_prefix(&self, node: NodeId) -> &SubstitutionSequence {
        &self.nodes[node.index()].substitute_sequence
    }

    pub fn compute_base_instr(&self, instr: Instruction) -> Result<Instruction, BaseInstrError> {
        let mut pos = 0;
        let mut node = NodeId::ROOT;
        loop {
            if let Some(b) = instr.bytes().get(pos) {
                if let Edge::Transition(next) = self.nodes[node].edges[*b as usize] {
                    node = next;
                    pos += 1;
                } else {
                    break
                }
            } else {
                return Err(BaseInstrError::NeedMoreBytes(1))
            }
        }

        if let SubstitutionSequence::EquivalentTo(seq) = &self.nodes[node].substitute_sequence {
            Ok(Instruction::new(
                &seq.bytes()
                    .iter()
                    .chain(instr.bytes()[pos..].iter())
                    .copied()
                    .collect::<ArrayVec<u8, 15>>(),
            ))
        } else {
            Err(BaseInstrError::NoMatch)
        }
    }

    pub fn node_for(&self, seq: &PrefixSequence) -> NodeId {
        self.compute_partial_substitution_prefix_internal(seq).2
    }

    pub fn compute_partial_substitution_prefix(&self, seq: &PrefixSequence) -> &SubstitutionSequence {
        self.compute_partial_substitution_prefix_internal(seq).0
    }

    pub fn canonicalize_instr(&self, instr: Instruction) -> Option<Instruction> {
        // We know that instructions are no longer than 15 bytes, so at most 14 bytes can be a prefix.
        let possible_prefix_bytes = &instr.bytes()[..instr.byte_len().min(14)];
        let seq = PrefixSequence::new(possible_prefix_bytes.iter().copied());
        let (seq, bytes_to_replace, _) = self.compute_partial_substitution_prefix_internal(&seq);
        let seq = seq.as_prefix_sequence()?;

        let mut bytes = [0; 16];
        bytes[..seq.len()].copy_from_slice(seq.bytes());
        bytes[seq.len()..instr.byte_len() + seq.len() - bytes_to_replace].copy_from_slice(&instr.bytes()[bytes_to_replace..]);

        Some(Instruction::new(&bytes[..instr.byte_len() + seq.len() - bytes_to_replace]))
    }

    fn compute_partial_substitution_prefix_internal(&self, seq: &PrefixSequence) -> (&SubstitutionSequence, usize, NodeId) {
        let mut pos = 0;
        let mut node_id = NodeId::ROOT;
        loop {
            let node = &self.nodes[node_id];
            if let Some(b) = seq.bytes().get(pos) {
                if let Edge::Transition(next) = node.edges[*b as usize] {
                    node_id = next;
                    pos += 1;
                } else {
                    return (&node.substitute_sequence, pos, node_id)
                }
            } else {
                return (&node.substitute_sequence, pos, node_id)
            }
        }
    }

    fn optimize(&mut self) -> (Vec<usize>, Partitions<Self>) {
        let partitions: Partitions<EquivalentPrefixes> = Partitions::of(self);
        if let Some(Edge::NotEquivalent) = partitions.canonical_node_of(NodeId::ROOT) {
            panic!("Optimized EquivalentPrefixes graph matches nothing")
        }

        assert_eq!(
            partitions.canonical_node_of(NodeId::ROOT),
            Some(Edge::Transition(NodeId::ROOT)),
            "root node must always stay root node"
        );

        if !partitions.no_optimizations_possible() {
            let mut n = 0;
            self.nodes.retain_mut(|node| {
                let keep = partitions.should_keep(NodeId::new(n));
                n += 1;

                if keep {
                    for edge in node.edges.iter_mut() {
                        if let Edge::Transition(next) = edge {
                            *edge = partitions.canonical_node_of(*next).unwrap();
                        }
                    }
                }

                keep
            });
        }

        let mut index = 0;
        let mut new_node_ids = vec![usize::MAX; self.nodes.len()];
        let mut explored = GrowingBitmap::new();
        let mut frontier = vec![NodeId::ROOT];

        while let Some(node) = frontier.pop() {
            if explored.set(node.index()) {
                new_node_ids[node.index()] = index;
                index += 1;

                let node = &self.nodes[node];
                for &edge in node.next().iter().rev() {
                    if let Edge::Transition(next) = edge {
                        frontier.push(next);
                    }
                }
            }
        }

        let mut new_nodes = self.nodes.clone();
        for (node_id, &new_index) in new_node_ids.iter().enumerate() {
            let mut source = self.nodes[node_id].clone();
            for edge in source.edges.iter_mut() {
                if let Edge::Transition(next) = edge {
                    *next = NodeId::new(new_node_ids[next.index()]);
                }
            }

            new_nodes[new_index] = source;
        }

        self.nodes = new_nodes;
        (new_node_ids, partitions)
    }

    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    fn transitions(&self, b: u8, node: Option<NodeId>) -> Option<NodeId> {
        node.and_then(|node| match self.nodes[node.index()].next().edge(b) {
            Edge::Transition(node) => Some(node),
            Edge::NotEquivalent => None,
        })
    }

    fn map_substitution_sequences_to_ids(&self) -> HashMap<SubstitutionSequence, u32> {
        let mut map = HashMap::new();
        for seq in self.nodes.iter() {
            let n = map.len() as u32;
            map.entry(seq.substitute_sequence.clone()).or_insert(n);
        }

        map
    }

    /// Computes the union of two equivalent prefixes graph.
    /// In order to do this, substitution sequences are canonicalized.
    pub fn union(&self, rhs: &EquivalentPrefixes) -> EquivalentPrefixes {
        let mut lhs = self.clone();
        lhs.canonicalize_substitution_sequences();
        let mut rhs = rhs.clone();
        rhs.canonicalize_substitution_sequences();

        trace!("Unioning with {rhs:#?}");

        #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        struct T {
            lhs: Option<NodeId>,
            rhs: Option<NodeId>,
        }

        let mut nodes = vec![Node {
            substitute_sequence: match lhs.nodes[0].substitute_sequence.clone() {
                SubstitutionSequence::NotEquivalent | SubstitutionSequence::TooLong => rhs.nodes[0].substitute_sequence.clone(),
                other => other,
            },
            edges: Next([Edge::NotEquivalent; 256]),
        }];

        let mut id_map = HashMap::new();
        let t = T {
            lhs: Some(NodeId::ROOT),
            rhs: Some(NodeId::ROOT),
        };
        id_map.insert(t.clone(), NodeId::ROOT);
        let mut frontier = vec![(t, NodeId::ROOT)];

        let lhs_ids = lhs.map_substitution_sequences_to_ids();
        let rhs_ids = rhs.map_substitution_sequences_to_ids();

        let mut node_identities = Vec::new();
        node_identities.push(Some((
            lhs_ids[lhs.nodes[0].substitute_sequence()],
            rhs_ids[rhs.nodes[0].substitute_sequence()],
        )));

        while let Some((t, id)) = frontier.pop() {
            let mut b = 0;
            let next = Next([(); 256].map(|_| {
                let lhs_id = lhs.transitions(b, t.lhs);
                let rhs_id = rhs.transitions(b, t.rhs);

                trace!("For {t:?} transitioning over 0x{b:02X} ({id:?}): {lhs_id:?} : {rhs_id:?}");

                b = b.wrapping_add(1);

                if lhs_id.is_none() && rhs_id.is_none() {
                    Edge::NotEquivalent
                } else {
                    Edge::Transition(
                        *id_map
                            .entry(T {
                                lhs: lhs_id,
                                rhs: rhs_id,
                            })
                            .or_insert_with_key(|key| {
                                let id = NodeId::new(nodes.len());
                                let lhs_seq = lhs_id
                                    .map(|n| &lhs.nodes[n].substitute_sequence)
                                    .unwrap_or(&SubstitutionSequence::NotEquivalent);
                                let rhs_seq = rhs_id
                                    .map(|n| &rhs.nodes[n].substitute_sequence)
                                    .unwrap_or(&SubstitutionSequence::NotEquivalent);

                                nodes.push(Node {
                                    substitute_sequence: SubstitutionSequence::TooLong,
                                    edges: Next([Edge::NotEquivalent; 256]),
                                });
                                node_identities.push(
                                    if let (SubstitutionSequence::NotEquivalent, SubstitutionSequence::NotEquivalent) =
                                        (lhs_seq, rhs_seq)
                                    {
                                        None
                                    } else {
                                        Some((lhs_ids[&lhs_seq], rhs_ids[&rhs_seq]))
                                    },
                                );
                                frontier.push((key.clone(), id));

                                trace!("{key:?} becomes new node {id:?}");

                                id
                            }),
                    )
                }
            }));

            nodes[id].edges = next;
        }

        let mut result = EquivalentPrefixes {
            nodes,
            num_bytes_to_replace: 0,
        };

        trace!("LHS IDs: {lhs_ids:?}");
        trace!("RHS IDs: {rhs_ids:?}");

        let mut identity_map = HashMap::new();
        let mut seen = GrowingBitmap::new_all_zeros(result.nodes.len());
        let mut frontier = VecDeque::new();
        frontier.push_back((NodeId::ROOT, PrefixSequence::empty()));
        while let Some((id, path)) = frontier.pop_front() {
            let node = &result.nodes[id.index()];

            let identity = node_identities[id.index()];
            if let Some(identity) = identity
                && let Entry::Vacant(e) = identity_map.entry(identity)
            {
                trace!("{identity:?} = {path:?}");
                e.insert(path.clone());
            }

            for (prefix, next) in node.edges.iter().enumerate() {
                if let Edge::Transition(next) = next
                    && seen.set(next.index())
                {
                    frontier.push_back((*next, path.chain_one(prefix as u8)));
                }
            }
        }

        for (node, identity) in result.nodes.iter_mut().zip(node_identities.iter()) {
            if let Some(identity) = identity
                && let Some(seq) = identity_map.get(identity)
            {
                node.substitute_sequence = SubstitutionSequence::EquivalentTo(seq.clone());
            } else {
                node.substitute_sequence = SubstitutionSequence::NotEquivalent;
            }
        }

        result.optimize();

        result
    }
}

impl traits::Graph for EquivalentPrefixes {
    type Index = NodeId;
    type TerminalIndex = ();
    type Edge = Edge;
    type Node = Node;

    const ROOT: Self::Index = NodeId::ROOT;

    fn num_nodes(&self) -> usize {
        self.nodes.len()
    }

    fn node(&self, index: Self::Index) -> &Self::Node {
        &self.nodes[index]
    }
}

impl traits::Index for NodeId {
    fn index(&self) -> usize {
        self.index()
    }

    fn from_usize(val: usize) -> Self {
        Self::new(val)
    }
}

impl traits::Edge<NodeId, ()> for Edge {
    const FAIL: Self = Edge::NotEquivalent;

    fn fails(&self) -> bool {
        *self == Edge::NotEquivalent
    }

    fn next_node(&self) -> Option<NodeId> {
        if let Edge::Transition(next) = self {
            Some(*next)
        } else {
            None
        }
    }

    fn terminal(&self) -> Option<()> {
        None
    }

    fn from_node_index(index: NodeId) -> Self {
        Edge::Transition(index)
    }

    fn from_terminal_index(_index: ()) -> Self {
        unreachable!()
    }
}

impl traits::Node<NodeId, (), Edge> for Node {
    fn transitions(&self) -> impl Iterator<Item = (u8, Edge)> {
        self.edges.iter().enumerate().map(|(index, &edge)| (index as u8, edge))
    }

    fn get(&self, byte: u8) -> Edge {
        self.edges[byte as usize]
    }

    fn hash_uniqueness(&self, hasher: &mut impl std::hash::Hasher) {
        self.substitute_sequence.hash(hasher);
    }

    fn identity(&self) -> impl Hash + Eq {
        &self.substitute_sequence
    }
}

#[derive(Clone, Debug)]
pub enum BaseInstrError {
    NeedMoreBytes(usize),
    TooManyBytes(usize),
    NoMatch,
}
