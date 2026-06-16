use std::collections::HashMap;
use std::fmt::Debug;
use std::ops::{Index, IndexMut};

use bitcode::{Decode, Encode};
use itertools::Itertools;
use serde::{Deserialize, Serialize};

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Encode, Decode)]
#[serde(transparent)]
pub struct NodeIndex(u32);

impl Debug for NodeIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "${}", self.0)
    }
}

impl NodeIndex {
    pub const ROOT: NodeIndex = NodeIndex(0);
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Encode, Decode)]
#[serde(transparent)]
pub struct ValIndex(u32);

impl Debug for ValIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "val{}", self.0)
    }
}

impl From<ValIndex> for NodeId {
    fn from(value: ValIndex) -> Self {
        // Do not need to check whether the top bit is set, since ValIndex::new guarantees it is unset.
        NodeId(value.0 | 0x8000_0000)
    }
}

impl From<NodeIndex> for NodeId {
    fn from(value: NodeIndex) -> Self {
        // Do not need to check whether the top bit is set, since NodeIndex::new guarantees it is unset.
        NodeId(value.0)
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Encode, Decode)]
pub enum UnpackedNodeId {
    NodeIndex(NodeIndex),
    ValIndex(ValIndex),
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Encode, Decode)]
#[serde(transparent)]
pub struct NodeId(u32);

impl NodeId {
    pub const FAIL: NodeId = NodeId(u32::MAX);
    // TODO: Replace DONE by index into Vec of Ts
    pub const ROOT: NodeId = NodeId(0);

    #[inline(always)]
    pub fn unpack(&self) -> Option<UnpackedNodeId> {
        if *self == Self::FAIL {
            None
        } else if self.0 & 0x8000_0000 == 0 {
            Some(UnpackedNodeId::NodeIndex(NodeIndex(self.0)))
        } else {
            Some(UnpackedNodeId::ValIndex(ValIndex(self.0 & 0x7fff_ffff)))
        }
    }
}

impl NodeIndex {
    pub fn new(node_id: usize) -> Self {
        assert!(node_id & !0x7fff_ffff == 0);
        Self(node_id as u32)
    }

    pub fn index(&self) -> usize {
        self.0 as usize
    }
}

impl ValIndex {
    pub fn new(val_id: usize) -> Self {
        assert!(val_id & !0x7fff_ffff == 0);
        Self(val_id as u32)
    }

    pub fn index(&self) -> usize {
        self.0 as usize
    }
}

impl Index<NodeIndex> for Vec<Node> {
    type Output = Node;

    fn index(&self, index: NodeIndex) -> &Self::Output {
        &self[index.index()]
    }
}

impl IndexMut<NodeIndex> for Vec<Node> {
    fn index_mut(&mut self, index: NodeIndex) -> &mut Self::Output {
        &mut self[index.index()]
    }
}

impl Debug for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.unpack() {
            Some(UnpackedNodeId::NodeIndex(n)) => write!(f, "{n:?}"),
            Some(UnpackedNodeId::ValIndex(n)) => write!(f, "{n:?}"),
            None => write!(f, "-"),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Encode, Decode)]
pub enum Next {
    Array([NodeId; 256]),
}

impl Serialize for Next {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let Next::Array(a) = self;
        a.as_slice().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Next {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let data = Vec::<NodeId>::deserialize(deserializer)?;

        Ok(Next::Array(
            data.try_into().expect("unable to convert vec into array of 256 elements"),
        ))
    }
}

impl Next {
    pub const ALL_FAIL: Next = Next::Array([NodeId::FAIL; 256]);

    pub fn new(mut f: impl FnMut(u8) -> NodeId) -> Next {
        let mut b = 0u8;
        Self::Array([0; 256].map(|_| {
            let id = f(b);
            b = b.wrapping_add(1);

            id
        }))
    }

    #[inline(always)]
    pub fn get(&self, byte: u8) -> NodeId {
        match self {
            Self::Array(next) => next[byte as usize],
        }
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = (u8, NodeId)> + '_ {
        match self {
            Self::Array(next) => next.iter().copied().enumerate().map(|(n, id)| (n as u8, id)),
        }
    }

    pub fn invert(&mut self) {
        match self {
            Self::Array(next) => {
                for node_id in next.iter_mut() {
                    match node_id.unpack() {
                        Some(UnpackedNodeId::ValIndex(_)) => *node_id = NodeId::FAIL,
                        None => *node_id = ValIndex::new(0).into(),
                        _ => (),
                    }
                }
            },
        }
    }

    pub fn remap_node_indices(&mut self, mut map: impl FnMut(NodeIndex) -> NodeId) {
        match self {
            Self::Array(next) => {
                for id in next.iter_mut() {
                    if let Some(UnpackedNodeId::NodeIndex(n)) = id.unpack() {
                        *id = map(n);
                    }
                }
            },
        }
    }

    pub fn remap_value_indices(&mut self, mut map: impl FnMut(ValIndex) -> NodeId) {
        match self {
            Self::Array(next) => {
                for id in next.iter_mut() {
                    if let Some(UnpackedNodeId::ValIndex(v)) = id.unpack() {
                        *id = map(v);
                    }
                }
            },
        }
    }

    pub fn any_not_fail(&self) -> bool {
        self.iter().any(|(_, id)| id != NodeId::FAIL)
    }

    pub fn any_fail(&self) -> bool {
        self.iter().any(|(_, id)| id == NodeId::FAIL)
    }

    pub fn any_done(&self) -> bool {
        self.iter()
            .any(|(_, id)| matches!(id.unpack(), Some(UnpackedNodeId::ValIndex(_))))
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct Node {
    pub next: Next,
}

impl Node {
    #[inline]
    pub fn resolve_next(&self, byte: u8) -> NodeId {
        self.next.get(byte)
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

        for (byte, next) in self.next.iter() {
            if next != NodeId::FAIL {
                targets.entry(next).or_insert_with(Vec::new).push(byte);
            }
        }

        let mut f = f.debug_map();
        for (next, bytes) in targets.into_iter().sorted_by_key(|(k, _)| *k) {
            f.entry(&DisplayBytes(bytes), &next);
        }

        f.finish()
    }
}
