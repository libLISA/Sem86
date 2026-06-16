use hashbrown::HashMap;

#[derive(Debug)]
pub struct Grouping {
    items: Vec<u32>,
}

impl Grouping {
    pub fn new(len: usize) -> Self {
        Self {
            items: (0..len).map(|n| n as u32).collect(),
        }
    }

    fn lookup(&self, mut n: u32) -> u32 {
        while self.items[n as usize] != n {
            n = self.items[n as usize];
        }

        n
    }

    pub fn same_group(&mut self, a: u32, b: u32) -> bool {
        self.lookup(a) == self.lookup(b)
    }

    pub fn same_group_many(&self, items: impl IntoIterator<Item = u32>) -> bool {
        items
            .into_iter()
            .map(|n| (self.lookup(n), true))
            .reduce(|(n1, b1), (n2, b2)| (n1, b1 && b2 && n1 == n2))
            .unwrap()
            .1
    }

    pub fn mark_same_group(&mut self, a: u32, b: u32) {
        if a > b {
            self.mark_same_group(b, a);
        } else {
            let v = self.lookup(a);
            self.items[a as usize] = v;
            self.items[b as usize] = v;
        }
    }

    pub fn mark_same_group_many(&mut self, items: &[u32]) {
        let min = items.iter().map(|&n| self.lookup(n)).min().unwrap();

        for &n in items {
            self.items[n as usize] = min;
        }
    }

    pub fn groups(&self) -> Vec<Vec<u32>> {
        let mut m = HashMap::new();
        for n in 0..self.items.len() {
            m.entry(self.lookup(n as u32)).or_insert_with(Vec::new).push(n as u32)
        }

        m.into_values().collect()
    }
}
