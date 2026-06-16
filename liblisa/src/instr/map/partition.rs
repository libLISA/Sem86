use std::hash::Hasher;
use std::mem::swap;

use arrayvec::ArrayVec;
use fxhash::{FxHashMap, FxHasher64};
use hashbrown::HashMap;
use log::{debug, trace};
use rayon::prelude::*;

use super::components::StronglyConnectedComponents;
use super::node::NodeIndex;
use super::traits::{Graph, Index};
use crate::instr::map::traits::{Edge, Node};
use crate::utils::bitmap::GrowingBitmap;
use crate::utils::stopwatch::Stopwatch;

#[derive(Clone, Debug)]
pub struct Partition<I> {
    last_update: u32,
    nodes: Vec<I>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct PartitionId(u32);

impl PartitionId {
    pub const ZERO: PartitionId = PartitionId(0);
    pub const FAIL: PartitionId = PartitionId(u32::MAX);
    pub const UNREACHABLE: PartitionId = PartitionId(u32::MAX - 2);

    #[inline]
    pub fn from_usize(index: usize) -> Self {
        assert!(index & !0x7fff_ffff == 0);
        PartitionId(index as u32)
    }

    #[inline]
    pub fn from_val_index<I: Index>(index: I) -> Self {
        let index = index.index();
        assert!(index & !0x7fff_ffff == 0);
        PartitionId(index as u32 | 0x8000_0000)
    }

    #[inline]
    pub fn index(&self) -> usize {
        self.0 as usize
    }

    fn is_nonterminal(&self) -> bool {
        self.0 & 0x8000_0000 == 0
    }
}

pub struct Partitions<G: Graph> {
    canonical_nodes: Vec<Option<G::Edge>>,
    nodes_to_keep: GrowingBitmap,
    partition_of_node: Vec<PartitionId>,
}

fn partition_of<I: Index, T: Index, E: Edge<I, T>>(partition_of_node: &[PartitionId], next: E) -> PartitionId {
    if let Some(n) = next.next_node() {
        partition_of_node[n.index()]
    } else if let Some(v) = next.terminal() {
        PartitionId::from_val_index(v)
    } else {
        assert!(next.fails());
        PartitionId::FAIL
    }
}

impl<G: Graph> Partitions<G> {
    fn compute_initial_partitions(graph: &G) -> (Vec<Partition<G::Index>>, Vec<PartitionId>) {
        let mut partitions = Vec::new();
        let mut buckets = FxHashMap::default();
        let mut partition_of_node = vec![PartitionId::UNREACHABLE; graph.num_nodes()];

        let mut internal_hashes = vec![0; graph.num_nodes()];

        // TODO: Can we guarantee the ordering of the partitions to be reverse topologically ordered? That would allow us to reduce the number of run_partitioning operations we would have to do.
        StronglyConnectedComponents::iterate(graph, |group| {
            for &node_index in group {
                internal_hashes[node_index.index()] = 0x1234_5678;
            }

            for &node_index in group {
                trace!("Computing hash for node {node_index:?}");
                let mut hash = FxHasher64::default();
                let mut has_self_loop = false;
                let node = &graph.node(node_index);
                for (_, edge) in node.transitions() {
                    if let Some(n) = edge.next_node() {
                        if !has_self_loop && n == node_index {
                            has_self_loop = true;
                        }

                        hash.write_u32(internal_hashes[n.index()])
                    } else if let Some(v) = edge.terminal() {
                        hash.write_u32(0x8000_0000 | u32::try_from(v.index()).unwrap())
                    } else {
                        assert!(edge.fails());
                        hash.write_u32(0x5555_5555)
                    }
                }

                node.hash_uniqueness(&mut hash);

                let hash = hash.finish() as u32;
                trace!(" - hash = {hash:08X}");

                let new_id = partitions.len();
                let partition_id = *buckets.entry((hash, node.identity())).or_insert_with(|| {
                    assert_eq!(partitions.len(), new_id);
                    partitions.push(Partition {
                        nodes: Vec::new(),
                        last_update: 0,
                    });

                    PartitionId::from_usize(new_id)
                });
                partitions[partition_id.index()].nodes.push(node_index);
                partition_of_node[node_index.index()] = partition_id;

                if !has_self_loop && group.len() == 1 {
                    trace!("Updating internal hash for {node_index:?} to {hash:08X}");
                    internal_hashes[node_index.index()] = hash;
                }
            }
        });

        (partitions, partition_of_node)
    }

    fn compute_partition_key<I: Index, T: Index, E: Edge<I, T>, N: Node<I, T, E>>(
        node: &N, partition_of_node: &[PartitionId], indices: impl Iterator<Item = u8>,
    ) -> [PartitionId; 256] {
        let mut key = [PartitionId::ZERO; 256];
        for byte in indices {
            key[byte as usize] = partition_of(partition_of_node, node.get(byte));
        }

        key
    }

    fn split_partitions(
        graph: &G, partition_of_node: &[PartitionId], current_partition: &Partition<G::Index>,
        all_partitions: &[Partition<G::Index>], tick: u32, check_nonterminals: bool,
    ) -> Vec<Partition<G::Index>> {
        trace!("partition_of_node = {partition_of_node:?}");
        let mut result = Vec::new();
        let primary_key = Self::compute_partition_key(graph.node(current_partition.nodes[0]), partition_of_node, 0..=0xff);

        // Improve performance by skipping inputs that couldn't have been updated
        let indices_that_need_update = primary_key
            .iter()
            .enumerate()
            .filter(|(_, partition_id)| {
                check_nonterminals
                    || (partition_id.is_nonterminal()
                        && all_partitions[partition_id.index()].last_update >= current_partition.last_update)
            })
            .map(|(index, _)| index as u8)
            .collect::<ArrayVec<_, 256>>();

        if indices_that_need_update.is_empty() {
            return vec![current_partition.clone()];
        }

        let mut pending = vec![(
            current_partition
                .nodes
                .iter()
                .map(|&node| (node, graph.node(node)))
                .collect::<Vec<_>>(),
            primary_key,
            0u8,
        )];

        let mut new_pending = Vec::new();
        while !pending.is_empty() {
            for (mut partition, primary_key, start) in pending.drain(..) {
                let mut new_partitions = HashMap::new();
                partition.retain_mut(|&mut (node_id, node)| {
                    let key = Self::compute_partition_key(node, partition_of_node, indices_that_need_update.iter().copied());
                    if let Some(&differing_byte) = indices_that_need_update.iter()
                        .find(|&&index| index >= start && primary_key[index as usize] != key[index as usize]) {
                        trace!("Next partition for byte {differing_byte} in node {node_id:?} = {:?} (primary: {:?}) -- splitting off into separate partition", key[differing_byte as usize], primary_key[differing_byte as usize]);
                        new_partitions.entry((differing_byte, key[differing_byte as usize]))
                            .or_insert_with(|| (key, Vec::new()))
                            .1.push((node_id, node));
                        false
                    } else {
                        true
                    }
                });

                new_pending.extend(
                    new_partitions
                        .into_iter()
                        .map(|((byte, _), (key, nodes))| (nodes, key, byte.saturating_add(1))),
                );

                result.push(Partition {
                    nodes: partition.into_iter().map(|(node, _)| node).collect(),
                    last_update: tick,
                });
            }

            swap(&mut pending, &mut new_pending);
        }

        assert!(new_pending.is_empty() && pending.is_empty());
        result
    }

    fn run_partitioning(graph: &G, partitions: &mut Vec<Partition<G::Index>>, partition_of_node: &mut [PartitionId])
    where
        G: Send + Sync,
        G::Index: Send + Sync,
    {
        let mut tick = 1;
        let mut more_to_check = true;
        let mut first = true;
        while more_to_check {
            more_to_check = false;
            debug!("Partitions: {partitions:?}");
            assert!(partitions.len() < u32::MAX as usize);

            let f = |(index, current_partition): (usize, &Partition<G::Index>)| {
                let partition_to_check = PartitionId::from_usize(index);

                if current_partition.nodes.len() > 1 {
                    let result = Self::split_partitions(graph, partition_of_node, current_partition, partitions, tick, first);
                    if result.len() > 1 {
                        Some((partition_to_check, result))
                    } else {
                        None
                    }
                } else {
                    None
                }
            };

            // Only paralellize if there is a lot of work to do.
            let extra_partitions = if partitions.len() > 100 && graph.num_nodes() > 100_000 {
                partitions
                    .par_iter()
                    .with_min_len(1)
                    .with_max_len(8)
                    .enumerate()
                    .map(f)
                    .flatten()
                    .collect::<Vec<_>>()
            } else {
                partitions.iter().enumerate().flat_map(f).collect::<Vec<_>>()
            };

            for (partition_to_check, mut result) in extra_partitions {
                partitions[partition_to_check.index()] = Partition {
                    nodes: result.pop().unwrap().nodes,
                    last_update: tick,
                };

                for partition in result {
                    let new_partition_id = PartitionId::from_usize(partitions.len());
                    for node_id in partition.nodes.iter() {
                        partition_of_node[node_id.index()] = new_partition_id;
                    }

                    partitions.push(partition);
                }

                more_to_check = true;
            }

            first = false;
            tick += 1;
        }
    }

    pub fn of(graph: &G) -> Self
    where
        G: Send + Sync,
        G::Index: Send + Sync,
    {
        let s = Stopwatch::now();

        // We immediately classify nodes that *only* transition to final states into separate partitions.
        // This is different from Hopcroft's algorithm, which initially only partitions into final and non-final states.
        // The result is the same. This is only an optimization that allows us to scan for all-done and all-fail nodes and collapse them immediately.

        let (mut partitions, mut partition_of_node) = Self::compute_initial_partitions(graph);
        let initial_partitioning_time = s.elapsed().as_millis();

        let s = Stopwatch::now();
        Self::run_partitioning(graph, &mut partitions, &mut partition_of_node[..]);
        let partitioning_time = s.elapsed().as_millis();

        let s = Stopwatch::now();
        debug!(
            "Partitioned {} nodes into {} partitions: {:?}",
            graph.num_nodes(),
            partitions.len(),
            partition_of_node
        );
        let mut next_node_id = 0;
        let mut nodes_to_keep = GrowingBitmap::new_all_zeros(graph.num_nodes());
        let mut canonical_nodes = vec![None; partitions.len()];
        for (node_id, &partition_id) in partition_of_node.iter().enumerate() {
            if partition_id != PartitionId::UNREACHABLE && canonical_nodes[partition_id.index()].is_none() {
                let node_index = NodeIndex::new(node_id);
                canonical_nodes[partition_id.index()] = Some({
                    nodes_to_keep.set(node_index.index());
                    let remapped_node_id = G::Edge::from_node_index(G::Index::from_usize(next_node_id));
                    next_node_id += 1;
                    debug!("Mapping partition {partition_id:?} to node id {remapped_node_id:?}");
                    remapped_node_id
                });
            }
        }

        debug!(
            "Partitioning timings: Inital list in {initial_partitioning_time}ms, partitioned in {partitioning_time}ms, canonical nodes in {}ms",
            s.elapsed().as_millis()
        );
        debug!("Canonical nodes: {canonical_nodes:?}");

        Partitions {
            canonical_nodes,
            nodes_to_keep,
            partition_of_node,
        }
    }

    pub fn canonical_node_of(&self, node: G::Index) -> Option<G::Edge> {
        let partition = self.partition_of_node[node.index()];
        if partition.is_nonterminal() {
            self.canonical_nodes[partition.index()]
        } else if partition == PartitionId::FAIL {
            Some(G::Edge::FAIL)
        } else if partition == PartitionId::UNREACHABLE {
            None
        } else {
            Some(G::Edge::from_terminal_index(G::TerminalIndex::from_usize(
                partition.0 as usize & 0x7fff_ffff,
            )))
        }
    }

    pub fn should_keep(&self, node: G::Index) -> bool {
        self.nodes_to_keep.get(node.index())
    }

    pub fn no_optimizations_possible(&self) -> bool {
        self.partition_of_node.iter().enumerate().all(|(node_id, &partition)| {
            partition != PartitionId::UNREACHABLE
                && self.canonical_nodes[partition.index()] == Some(G::Edge::from_node_index(G::Index::from_usize(node_id)))
        })
    }
}
