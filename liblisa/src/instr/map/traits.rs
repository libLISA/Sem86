use std::fmt::Debug;
use std::hash::{Hash, Hasher};

use super::InstructionMap;
use super::node::{NodeId, NodeIndex, UnpackedNodeId, ValIndex};

pub(crate) trait Graph {
    type Index: Index + Hash + PartialEq + Eq + PartialOrd + Ord;
    type TerminalIndex: Index;
    type Edge: Edge<Self::Index, Self::TerminalIndex>;
    type Node: Node<Self::Index, Self::TerminalIndex, Self::Edge>;

    const ROOT: Self::Index;

    fn num_nodes(&self) -> usize;
    fn node(&self, index: Self::Index) -> &Self::Node;
}

pub(crate) trait Index: Copy + Clone + Debug + PartialEq + Eq {
    fn index(&self) -> usize;
    fn from_usize(val: usize) -> Self;
}

pub(crate) trait Edge<I: Index, T: Index>: Copy + Clone + Debug + PartialEq + Eq {
    const FAIL: Self;

    fn fails(&self) -> bool;
    fn next_node(&self) -> Option<I>;
    fn terminal(&self) -> Option<T>;

    fn from_node_index(index: I) -> Self;
    fn from_terminal_index(index: T) -> Self;
}

pub(crate) trait Node<I: Index, T: Index, E: Edge<I, T>> {
    fn transitions(&self) -> impl Iterator<Item = (u8, E)>;
    fn get(&self, byte: u8) -> E;
    fn hash_uniqueness(&self, hasher: &mut impl Hasher);
    fn identity(&self) -> impl Hash + Eq;
}

impl<T> Graph for InstructionMap<T> {
    type Index = NodeIndex;
    type TerminalIndex = ValIndex;
    type Edge = NodeId;
    type Node = super::node::Node;

    const ROOT: Self::Index = NodeIndex::ROOT;

    fn num_nodes(&self) -> usize {
        self.nodes.len()
    }

    fn node(&self, index: Self::Index) -> &Self::Node {
        &self.nodes[index]
    }
}

impl Index for NodeIndex {
    fn index(&self) -> usize {
        self.index()
    }

    fn from_usize(val: usize) -> Self {
        Self::new(val)
    }
}

impl Index for ValIndex {
    fn index(&self) -> usize {
        self.index()
    }

    fn from_usize(val: usize) -> Self {
        Self::new(val)
    }
}

impl Edge<NodeIndex, ValIndex> for NodeId {
    const FAIL: Self = NodeId::FAIL;

    fn fails(&self) -> bool {
        *self == Self::FAIL
    }

    fn next_node(&self) -> Option<NodeIndex> {
        if let Some(UnpackedNodeId::NodeIndex(next)) = self.unpack() {
            Some(next)
        } else {
            None
        }
    }

    fn terminal(&self) -> Option<ValIndex> {
        if let Some(UnpackedNodeId::ValIndex(next)) = self.unpack() {
            Some(next)
        } else {
            None
        }
    }

    fn from_node_index(index: NodeIndex) -> Self {
        index.into()
    }

    fn from_terminal_index(index: ValIndex) -> Self {
        index.into()
    }
}

impl Node<NodeIndex, ValIndex, NodeId> for super::node::Node {
    fn transitions(&self) -> impl Iterator<Item = (u8, NodeId)> {
        self.next.iter()
    }

    fn get(&self, byte: u8) -> NodeId {
        self.next.get(byte)
    }

    fn hash_uniqueness(&self, _hasher: &mut impl Hasher) {
        // There are no differences between nodes besides the transitions, so we do not need to add anything here.
    }

    fn identity(&self) -> impl Hash + Eq {}
}

impl Index for () {
    fn index(&self) -> usize {
        0
    }

    fn from_usize(val: usize) -> Self {
        assert_eq!(val, 0);
    }
}
