use std::fmt::Debug;
use std::hash::Hash;

pub trait Graph {
    type Index: Index;
    type Node: Node<Self::Index>;

    const ROOT: Self::Index;

    fn num_nodes(&self) -> usize;
    fn node(&self, index: Self::Index) -> &Self::Node;
}

pub trait Index: Copy + Clone + Debug + PartialEq + Eq + Hash + PartialEq + Eq + PartialOrd + Ord {
    fn index(&self) -> usize;
    fn from_usize(val: usize) -> Self;
}

pub trait Node<I: Index> {
    fn transitions(&self) -> impl Iterator<Item = I>;
}

impl Index for () {
    fn index(&self) -> usize {
        0
    }

    fn from_usize(val: usize) -> Self {
        assert_eq!(val, 0);
    }
}

impl Index for usize {
    fn index(&self) -> usize {
        *self
    }

    fn from_usize(val: usize) -> Self {
        val
    }
}
