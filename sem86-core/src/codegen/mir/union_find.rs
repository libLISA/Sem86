#[derive(Clone, Debug)]
pub struct UnionFind {
    parent: Vec<usize>,
    rank: Vec<usize>,
}

impl UnionFind {
    pub fn new(n: usize) -> Self {
        UnionFind {
            parent: (0..n).collect(),
            rank: vec![0; n],
        }
    }

    pub fn resize(&mut self, new_len: usize) {
        if new_len > self.parent.len() {
            self.parent.extend(self.parent.len()..new_len);
            self.rank.resize(new_len, 0);
        }
    }

    pub fn find(&mut self, mut x: usize) -> usize {
        let mut root = x;
        while self.parent[root] != root {
            root = self.parent[root];
        }

        while self.parent[x] != root {
            let parent = self.parent[x];
            self.parent[x] = root;
            x = parent;
        }

        root
    }

    pub fn union(&mut self, x: usize, y: usize) -> Option<usize> {
        let root_x = self.find(x);
        let root_y = self.find(y);
        if root_x != root_y {
            let (root_x, root_y) = if self.rank[root_x] < self.rank[root_y] {
                (root_y, root_x)
            } else {
                (root_x, root_y)
            };

            self.parent[root_y] = root_x;
            if self.rank[root_x] == self.rank[root_y] {
                self.rank[root_x] += 1;
            }

            Some(root_x)
        } else {
            None
        }
    }

    pub fn in_same_group(&mut self, x: usize, y: usize) -> bool {
        self.find(x) == self.find(y)
    }
}
