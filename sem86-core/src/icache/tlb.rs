use sem86_arch::addr::LinAddr;

use crate::icache::entry::CacheEntryId;
use crate::util::version::Version;

#[derive(Copy, Clone, Debug)]
struct Entry<'tag> {
    pc: LinAddr,
    page_version: Version,
    id: CacheEntryId<'tag>,
}

pub struct Tlb<'tag, const N: usize>
where
    [(); 1 << N]:,
{
    entries: [Entry<'tag>; 1 << N],
}

impl<'tag, const N: usize> Default for Tlb<'tag, N>
where
    [(); 1 << N]:,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<'tag, const N: usize> Tlb<'tag, N>
where
    [(); 1 << N]:,
{
    pub fn new() -> Self {
        Self {
            entries: [Entry {
                pc: LinAddr::new(0),
                page_version: Version::ZERO,

                // SAFETY: We will never return this becaues the page version is zero
                id: unsafe { CacheEntryId::new_unchecked(0) },
            }; 1 << N],
        }
    }

    #[inline(always)]
    fn hash(pc: LinAddr) -> usize {
        let pc = pc.as_u32() >> 1;
        let pc = pc ^ (pc >> 16);
        let pc = pc.wrapping_mul(0x9E3779B1);

        (pc >> (32 - N)) as usize
    }

    pub fn clear(&mut self) {
        for e in self.entries.iter_mut() {
            e.page_version = Version::ZERO;
        }
    }

    #[inline(always)]
    pub fn lookup(&self, pc: LinAddr, page_version: Version) -> Option<CacheEntryId<'tag>> {
        let entry = self.entries[Self::hash(pc)];
        if entry.pc == pc && entry.page_version >= page_version {
            Some(entry.id)
        } else {
            None
        }
    }

    pub fn insert(&mut self, pc: LinAddr, page_version: Version, id: CacheEntryId<'tag>) {
        self.entries[Self::hash(pc)] = Entry {
            pc,
            page_version,
            id,
        };
    }
}
