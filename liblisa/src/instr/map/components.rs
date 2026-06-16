use super::traits::{Graph, Index, Node};
use crate::instr::map::traits::Edge;

#[derive(Copy, Clone, Debug)]
struct Info {
    on_stack: bool,
    index: Option<u32>,
    lowlink: u32,
}

pub struct StronglyConnectedComponents<'graph, G: Graph> {
    info: Vec<Info>,
    stack: Vec<G::Index>,
    index: u32,
    graph: &'graph G,
}

impl<G: Graph> StronglyConnectedComponents<'_, G> {
    /// Non-recursive implementation of [Tarjan's strongly connected components algorithm.](https://en.wikipedia.org/wiki/Tarjan%27s_strongly_connected_components_algorithm)
    fn strong_connect_norec(mut self, v: G::Index, callback: &mut impl FnMut(&[G::Index])) {
        let mut frontier = vec![(v, self.graph.node(v).transitions())];

        let entry = &mut self.info[v.index()];
        entry.index = Some(self.index);
        entry.lowlink = self.index;
        entry.on_stack = true;
        self.index += 1;
        self.stack.push(v);

        'enter: while let Some((v, iter)) = frontier.last_mut() {
            let v_index = v.index();
            assert!(v_index < self.info.len());
            loop {
                if let Some((_, edge)) = iter.next() {
                    // Main loop
                    if let Some(node_w) = edge.next_node() {
                        let w = &mut self.info[node_w.index()];
                        // trace!("w = {w:?}");
                        match w.index {
                            None => {
                                w.index = Some(self.index);
                                w.lowlink = self.index;
                                w.on_stack = true;
                                self.index += 1;
                                self.stack.push(node_w);

                                frontier.push((node_w, self.graph.node(node_w).transitions()));
                                continue 'enter
                            },
                            Some(w_index) if w.on_stack => {
                                self.info[v_index].lowlink = self.info[v_index].lowlink.min(w_index);
                            },
                            _ => (),
                        }
                    }
                } else {
                    // Tail
                    let entry = &mut self.info[v_index];
                    if entry.lowlink == entry.index.unwrap() {
                        let num = self.stack.iter().rev().take_while(|&node| node != v).count() + 1;

                        callback(&self.stack[self.stack.len() - num..]);
                        for w in self.stack.drain(self.stack.len() - num..) {
                            self.info[w.index()].on_stack = false;
                        }
                    }

                    let (node_w, _) = frontier.pop().unwrap();
                    if let Some((v, _)) = frontier.last() {
                        self.info[v.index()].lowlink = self.info[v.index()].lowlink.min(self.info[node_w.index()].lowlink);
                    }

                    continue 'enter
                }
            }
        }
    }

    /// Iterates over all strongly connected components in the graph in reverse topological order.
    pub fn iterate(graph: &G, mut callback: impl FnMut(&[G::Index])) {
        let b = StronglyConnectedComponents {
            info: vec![
                Info {
                    on_stack: false,
                    index: None,
                    lowlink: 0,
                };
                graph.num_nodes()
            ],
            stack: Vec::new(),
            index: 0,
            graph,
        };
        b.strong_connect_norec(G::ROOT, &mut callback);
    }
}
