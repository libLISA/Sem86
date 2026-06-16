use std::collections::HashSet;
use std::ops::Index;

use liblisa::utils::bitmap::GrowingBitmap;

use crate::codegen::page::PageInstrWithEdges;

pub struct Roots {
    root_of: Vec<usize>,
}

impl Roots {
    pub fn of(edges: &[PageInstrWithEdges]) -> Roots {
        // Assign each node a root node.
        // The root of a node is the only root that can reach this node.
        // More roots are added until this is the case.

        let mut root_of = vec![None; edges.len()];
        for (index, edge) in edges.iter().enumerate() {
            if edge.make_split_point || edge.preds.is_empty() {
                root_of[index] = Some(index);
            }
        }

        Self::break_loops(edges, &mut root_of);

        loop {
            let mut any_changed = false;
            let roots = (0..edges.len())
                .filter(|&index| root_of[index] == Some(index))
                .collect::<Vec<_>>();
            let reachability = roots
                .iter()
                .map(|&index| {
                    (index, {
                        let mut reachable = HashSet::new();
                        let mut frontier = vec![index];
                        reachable.insert(index);
                        while let Some(index) = frontier.pop() {
                            for &next in edges[index].succs.iter() {
                                if reachable.insert(next) {
                                    frontier.push(next);
                                }
                            }
                        }

                        reachable
                    })
                })
                .collect::<Vec<_>>();
            let mut frontier = roots.clone();
            let mut seen = GrowingBitmap::new_all_zeros(edges.len());
            while let Some(index) = frontier.pop() {
                if seen.set(index) {
                    // If this node is reachable from multiple roots, make it a new root.
                    if root_of[index] != Some(index) && reachability.iter().filter(|(_, r)| r.contains(&index)).count() > 1 {
                        any_changed = true;
                        root_of[index] = Some(index);
                    } else {
                        frontier.extend(edges[index].succs.iter().copied());
                    }
                }
            }

            if !any_changed {
                // Propagate roots to all nodes
                let mut frontier = (0..edges.len())
                    .filter(|&index| root_of[index] == Some(index))
                    .map(|index| (index, index))
                    .collect::<Vec<_>>();

                while let Some((index, root)) = frontier.pop() {
                    root_of[index] = Some(root);
                    frontier.extend(
                        edges[index]
                            .succs
                            .iter()
                            .filter(|&&index| root_of[index].is_none())
                            .map(|&index| (index, root)),
                    );
                }

                break
            }
        }

        Roots {
            root_of: root_of.into_iter().map(|v| v.unwrap()).collect(),
        }
    }

    /// Makes a root at every node that has an incoming looping edge.
    fn break_loops(edges: &[PageInstrWithEdges], root_of: &mut [Option<usize>]) {
        fn dfs(
            edges: &[PageInstrWithEdges], root_of: &mut [Option<usize>], current: usize, seen: &mut GrowingBitmap,
            path: &mut Vec<usize>,
        ) {
            let instr = &edges[current];
            for &next in instr.succs.iter() {
                if seen[next] {
                    root_of[next] = Some(next);
                } else if root_of[next] != Some(next) {
                    path.push(next);
                    seen.set(next);
                    dfs(edges, root_of, next, seen, path);
                    seen.reset(next);
                    assert_eq!(path.pop(), Some(next));
                }
            }
        }

        let mut seen = GrowingBitmap::new();
        let mut path = Vec::new();
        for index in 0..edges.len() {
            if root_of[index] == Some(index) {
                seen.set(index);
                path.push(index);
                dfs(edges, root_of, index, &mut seen, &mut path);
                seen.reset(index);
                assert_eq!(path.pop(), Some(index));
            }
        }
    }

    #[allow(unused)]
    pub fn split(&self) -> impl Iterator<Item = (usize, Vec<usize>)> {
        self.root_of
            .iter()
            .enumerate()
            .filter(|&(index, &root)| index == root)
            .map(|(root, _)| {
                let items = self
                    .root_of
                    .iter()
                    .enumerate()
                    .filter(|&(_, &r)| r == root)
                    .map(|(index, _)| index)
                    .collect::<Vec<_>>();

                (root, items)
            })
    }
}

impl Index<usize> for Roots {
    type Output = usize;

    fn index(&self, index: usize) -> &Self::Output {
        &self.root_of[index]
    }
}
