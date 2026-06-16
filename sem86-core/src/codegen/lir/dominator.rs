use std::cmp::Ordering;

use crate::codegen::mir::bb::{BbGraph, BbId};

#[derive(Clone)]
struct Node {
    /// If 0, block isn't reachable.
    /// If 1, block is reachable.
    /// If >2, represents RPO index + 2.
    n: u32,
    idom: Option<BbId>,
    predecessors: Vec<BbId>,
}

pub struct DominatorTree {
    nodes: Vec<Node>,
    postorder: Vec<BbId>,
}

impl DominatorTree {
    pub fn new(bb: &BbGraph) -> Self {
        let mut postorder = Vec::new();
        let mut nodes = vec![
            Node {
                n: 0,
                idom: None,
                predecessors: Vec::new(),
            };
            bb.len()
        ];

        let mut stack = vec![(true, BbId::ROOT)];
        while let Some((is_entry, id)) = stack.pop() {
            nodes[id.index()].n = 1;

            if is_entry {
                stack.push((false, id));

                for &next in bb[id].next_blocks() {
                    nodes[next.index()].predecessors.push(id);

                    if nodes[next.index()].n == 0 {
                        stack.push((true, next));
                    }
                }
            } else {
                postorder.push(id);
            }
        }

        let mut tree = DominatorTree {
            nodes,
            postorder,
        };

        tree.initialize(bb);

        tree
    }

    fn initialize(&mut self, _bb: &BbGraph) {
        self.nodes[BbId::ROOT.index()].n = 2;

        for (index, &id) in self.postorder.iter().rev().skip(1).enumerate() {
            self.nodes[id.index()].idom = Some(self.compute_idom(id));
            self.nodes[id.index()].n = 2 + index as u32;
        }

        let mut done = false;
        while !done {
            done = true;
            for &id in self.postorder.iter().rev().skip(1) {
                let idom = self.compute_idom(id);
                if self.nodes[id.index()].idom != Some(idom) {
                    self.nodes[id.index()].idom = Some(idom);
                    done = false;
                }
            }
        }
    }

    fn compute_idom(&self, id: BbId) -> BbId {
        self.nodes[id.index()]
            .predecessors
            .iter()
            .copied()
            .reduce(|a, b| self.lca(a, b))
            .unwrap()
    }

    pub fn dominates(&self, dominator: BbId, mut other: BbId) -> bool {
        let dominator_n = self.nodes[dominator.index()].n;
        loop {
            match dominator_n.cmp(&self.nodes[other.index()].n) {
                Ordering::Less => {
                    if let Some(idom) = self.nodes[other.index()].idom {
                        other = idom
                    } else {
                        return false
                    }
                },
                Ordering::Equal => return dominator == other,
                Ordering::Greater => return false,
            }
        }
    }

    /// LCA of two nodes
    pub fn lca(&self, mut a: BbId, mut b: BbId) -> BbId {
        loop {
            match self.nodes[a.index()].n.cmp(&self.nodes[b.index()].n) {
                Ordering::Less => b = self.nodes[b.index()].idom.unwrap(),
                Ordering::Equal => break,
                Ordering::Greater => a = self.nodes[a.index()].idom.unwrap(),
            }
        }

        a
    }

    pub fn lca_many(&self, nodes: impl IntoIterator<Item = BbId>) -> BbId {
        nodes.into_iter().reduce(|a, b| self.lca(a, b)).unwrap()
    }
}
