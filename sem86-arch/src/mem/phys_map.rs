use std::ops::Range;
use std::sync::Arc;

use rangemap::RangeMap;

use super::{MmioId, Shm};
use crate::mem::MemoryWatcher;

#[derive(Clone, Debug)]
pub enum Entry {
    Mmio {
        id: MmioId,
    },
    Shm {
        shm: Arc<Shm>,
        phys_addr_to_shm_delta: i64,
        watcher: Option<Arc<dyn MemoryWatcher>>,
        writable: bool,
    },
}

impl PartialEq for Entry {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Entry::Mmio {
                    id: a,
                },
                Entry::Mmio {
                    id: b,
                },
            ) => a == b,
            (
                Entry::Shm {
                    shm: shm_a,
                    phys_addr_to_shm_delta: patsd_a,
                    watcher: watcher_a,
                    writable: writable_a,
                },
                Entry::Shm {
                    shm: shm_b,
                    phys_addr_to_shm_delta: patsd_b,
                    watcher: watcher_b,
                    writable: writable_b,
                },
            ) => {
                shm_a == shm_b
                    && patsd_a == patsd_b
                    && writable_a == writable_b
                    && watcher_a
                        .as_ref()
                        .zip(watcher_b.as_ref())
                        .map_or(watcher_a.is_none() && watcher_b.is_none(), |(a, b)| Arc::ptr_eq(a, b))
            },
            _ => false,
        }
    }
}

impl Eq for Entry {}

#[derive(Clone, Debug)]
pub enum EntryRef<'r> {
    Mmio {
        id: MmioId,
    },
    Shm {
        shm: &'r Arc<Shm>,
        phys_addr_to_shm_delta: i64,
        watcher: Option<&'r Arc<dyn MemoryWatcher>>,
        writable: bool,
    },
    Default,
}

pub struct PhysMap {
    entries: RangeMap<u64, Entry>,
}

impl PhysMap {
    pub fn new() -> Self {
        Self {
            entries: RangeMap::new(),
        }
    }

    pub fn map_shm(
        &mut self, range: Range<u64>, shm: Arc<Shm>, watcher: Option<Arc<dyn MemoryWatcher>>, offset: u64, writable: bool,
    ) {
        let entry = Entry::Shm {
            shm,
            phys_addr_to_shm_delta: offset.wrapping_sub(range.start) as i64,
            watcher,
            writable,
        };
        self.map_internal(range, entry);
    }

    pub fn map_mmio(&mut self, range: Range<u64>, id: MmioId) {
        self.map_internal(
            range,
            Entry::Mmio {
                id,
            },
        );
    }

    pub fn remove_map(&mut self, range: Range<u64>) {
        self.entries.remove(range);
    }

    fn map_internal(&mut self, range: Range<u64>, content: Entry) {
        self.entries.insert(range, content);
    }

    pub fn lookup(&self, addr: u64) -> (EntryRef<'_>, Range<u64>) {
        if let Some((range, entry)) = self.entries.get_key_value(&addr) {
            let r = match entry {
                Entry::Mmio {
                    id,
                } => EntryRef::Mmio {
                    id: *id,
                },
                Entry::Shm {
                    shm,
                    phys_addr_to_shm_delta,
                    watcher,
                    writable,
                } => EntryRef::Shm {
                    shm,
                    phys_addr_to_shm_delta: *phys_addr_to_shm_delta,
                    watcher: watcher.as_ref(),
                    writable: *writable,
                },
            };

            (r, range.clone())
        } else {
            let page_start = addr & !0xfff;
            (EntryRef::Default, page_start..page_start + 0x1000)
        }
    }
}
