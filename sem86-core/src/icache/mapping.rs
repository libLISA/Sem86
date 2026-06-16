use std::collections::HashMap;
use std::collections::hash_map::Entry;

use liblisa::utils::bitmap::{Bitmap, BitmapSlice, FixedBitmapU64};
use log::{debug, trace, warn};
use sem86_arch::addr::{LinAddr, LinPageIndex, PhysAddr, PhysFrameIndex};
use sem86_arch::exceptions::{Exception, PageFaultCode};
use sem86_arch::mem::{Mem32, PageWalkResult};

use crate::icache::PhysIndexedArray;

#[derive(Clone, Debug, Default)]
struct Frame {
    /// A single physical frame can be mapped at multiple linear addresses.
    /// When we are notified that this frame is no longer mapped at a linear address,
    /// we need to break all links to this frame.
    mappings: Vec<PageMapping>,

    /// Set to true when we know that all mappings in `mappings` are current as of the last CR3 reload.
    page_mappings_current: bool,
}

#[derive(Copy, Clone, Debug)]
struct PageMapping {
    // TODO: We need to track whether mappings are user-accessible or not.
    page: LinPageIndex,

    /// The physical frame index of the page that is mapped to linear page index `(page + 1)`.
    successor: Option<PhysFrameIndex>,

    /// The physical frame index of the page that is mapped to linear page index `(page - 1)`.
    /// The dual of `successor`.
    predecessor: Option<PhysFrameIndex>,
}

pub trait MappingObserver {
    fn predecessor_changed(
        &mut self, mapping: &MappingTracker, phys_frame_index: PhysFrameIndex, lin_page_index: LinPageIndex,
        old_predecessor: Option<PhysFrameIndex>, new_predecessor: Option<PhysFrameIndex>,
    );
    fn successor_changed(
        &mut self, mapping: &MappingTracker, phys_frame_index: PhysFrameIndex, lin_page_index: LinPageIndex,
        old_successor: Option<PhysFrameIndex>, new_successor: Option<PhysFrameIndex>,
    );

    fn mapping_added(&mut self, phys_frame_index: PhysFrameIndex, new_lin_page: LinPageIndex);

    fn mapping_removed(&mut self, phys_frame_index: PhysFrameIndex, old_lin_page: LinPageIndex);

    fn page_mappings_current_changed(&mut self, mapping: &MappingTracker, phys_frame_index: PhysFrameIndex, checked: bool);
}

const NUM_FRAMES: usize = 1 << 20;
const NUM_PAGE_MAPPING_CHECKED_BITS: usize = 64 * 8;
const PAGE_MAPPING_CHECKED_CHUNK_SIZE: usize = NUM_FRAMES / NUM_PAGE_MAPPING_CHECKED_BITS;

/// Tracks how pages are mapped to physical frames.
/// Maintains an eventually-consistent cached representation of page mappings.
/// A call to `resolve_phys_frame_index` (as well as the `rewalk_pages` helper method)
/// will query memory and update some mappings.
///
/// This type tracks *linear predecessors and successors*.
/// When instructions cross page bounds, the two frames mapped to those consecutive pages
/// are likely not consecutive physical frames.
/// We call two physical frames that are mapped to consecutive linear memory linear predecessors/successors.
///
/// Whenever a mapping is updated, as well as when predecessors and successors change,
/// these changes are provided to the `observer` argument passed to a function.
/// If a function does not take an `observer` argument, the internal mapping state will not change.
pub struct MappingTracker {
    info: PhysIndexedArray<Frame>,

    /// Stores a mapping of page indices to physical frame indices.
    lin_address_map: HashMap<LinPageIndex, PhysFrameIndex>,

    frame_chunks_with_page_mappings_current: FixedBitmapU64<{ NUM_PAGE_MAPPING_CHECKED_BITS / 64 }>,
}

impl Default for MappingTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl MappingTracker {
    pub fn new() -> Self {
        Self {
            info: PhysIndexedArray::new(),
            lin_address_map: HashMap::new(),
            frame_chunks_with_page_mappings_current: FixedBitmapU64::new_all_zeros(NUM_PAGE_MAPPING_CHECKED_BITS),
        }
    }

    pub fn mark_all_mappings_as_stale(&mut self, observer: &mut impl MappingObserver) {
        trace!("Marking all page mappings as stale");
        let mut x = FixedBitmapU64::new_all_zeros(NUM_PAGE_MAPPING_CHECKED_BITS);
        std::mem::swap(&mut x, &mut self.frame_chunks_with_page_mappings_current);
        for chunk in x.iter_one_indices() {
            let chunk = chunk * PAGE_MAPPING_CHECKED_CHUNK_SIZE..(chunk + 1) * PAGE_MAPPING_CHECKED_CHUNK_SIZE;
            for index in chunk {
                let phys_frame_index = PhysFrameIndex::new(index as u32);

                if self.info[phys_frame_index].page_mappings_current {
                    self.info[phys_frame_index].page_mappings_current = false;
                    observer.page_mappings_current_changed(self, phys_frame_index, false);
                }
            }
        }

        #[cfg(debug_assertions)]
        for (index, info) in self.info.iter().enumerate() {
            debug_assert!(
                !info.page_mappings_current,
                "mark_all_mappings_as_stale should have marked {} as stale",
                PhysFrameIndex::new(index as u32)
            );
        }

        self.check_consistency();
    }

    fn page_walk(lin_addr: LinAddr, is_userspace: bool, memory: &Mem32) -> Result<PhysAddr, Exception> {
        Ok(if memory.paging_enabled() {
            PhysAddr::new(match memory.page_walk(lin_addr.as_u32(), true) {
                PageWalkResult::Unmapped(_) => {
                    warn!("page fault at {lin_addr} while resolving address in icache");

                    return Err(Exception::PageFault {
                        code: PageFaultCode::from_normal_access(false, false, is_userspace, false),
                        address: lin_addr.as_u32(),
                    })
                },
                PageWalkResult::PhysAddr {
                    addr, ..
                } => addr as u32,
            })
        } else if memory.a20_line() {
            PhysAddr::new(lin_addr.as_u32())
        } else {
            PhysAddr::new(lin_addr.as_u32() & 0xfffff)
        })
    }

    #[inline(always)]
    pub fn resolve_phys_frame_index(
        &mut self, observer: &mut impl MappingObserver, lin_addr: LinAddr, is_userspace: bool, memory: &Mem32,
    ) -> Result<PhysAddr, Exception> {
        let result = match Self::page_walk(lin_addr, is_userspace, memory) {
            Ok(phys_addr) => {
                self.update_frame_page_mapping(observer, phys_addr.into(), lin_addr.into());

                Ok(phys_addr)
            },
            Err(e) => {
                self.remove_frame_page_mapping(observer, lin_addr.into());

                Err(e)
            },
        };

        self.check_consistency();

        result
    }

    #[inline(always)]
    pub fn page_mapping_is_current(&self, phys_frame_index: PhysFrameIndex) -> bool {
        self.info[phys_frame_index].page_mappings_current
    }

    pub fn rewalk_pages(
        &mut self, observer: &mut impl MappingObserver, phys_frame_index: PhysFrameIndex, is_userspace: bool, memory: &Mem32,
    ) {
        let info = &mut self.info[phys_frame_index];
        info.page_mappings_current = true;
        self.frame_chunks_with_page_mappings_current
            .set(phys_frame_index.index() / PAGE_MAPPING_CHECKED_CHUNK_SIZE);

        for n in (0..self.info[phys_frame_index].mappings.len()).rev() {
            let lin_addr = self.info[phys_frame_index].mappings[n].page;
            self.resolve_phys_frame_index(observer, lin_addr.start_addr(), is_userspace, memory)
                .ok();
        }

        observer.page_mappings_current_changed(self, phys_frame_index, true);

        self.check_consistency();
    }

    fn update_frame_page_mapping(
        &mut self, observer: &mut impl MappingObserver, phys_frame_index: PhysFrameIndex, page: LinPageIndex,
    ) {
        match self.lin_address_map.entry(page) {
            Entry::Occupied(mut e) => {
                if *e.get() != phys_frame_index {
                    let old_phys_index = *e.get();
                    debug!("Updated linear address map: page {page} -> phys frame {phys_frame_index}");
                    e.insert(phys_frame_index);

                    // All mappings where the successor was mapped at `(page - 1)` now have `phys_frame_index` as predecessor.
                    let n = self.info[old_phys_index]
                        .mappings
                        .iter()
                        .position(|m| m.page == page)
                        .expect("frame should have mapping set if it is in `lin_address_map`");

                    let m = self.info[old_phys_index].mappings.remove(n);
                    if let Some(successor) = m.successor {
                        self.info[successor]
                            .mappings
                            .iter_mut()
                            .find(|m| m.page == (page + 1))
                            .expect("predecessors and successors should be consistent")
                            .predecessor = Some(phys_frame_index);
                    }

                    if let Some(predecessor) = m.predecessor {
                        self.info[predecessor]
                            .mappings
                            .iter_mut()
                            .find(|m| m.page == (page - 1))
                            .expect("predecessors and successors should be consistent")
                            .successor = Some(phys_frame_index);
                    }

                    debug_assert!(!self.info[phys_frame_index].mappings.iter().any(|m| m.page == page));
                    self.info[phys_frame_index].mappings.push(PageMapping {
                        page,
                        successor: m.successor,
                        predecessor: m.predecessor,
                    });

                    // The old physical frame is no longer located at `page`, so remove them.
                    if let Some(successor) = m.successor {
                        observer.successor_changed(self, old_phys_index, page, Some(successor), None);
                    }
                    if let Some(predecessor) = m.predecessor {
                        observer.predecessor_changed(self, old_phys_index, page, Some(predecessor), None);
                    }

                    // For the successor and predecessor, their predecessor and successor (respectively) changed to the new physical frame.
                    if let Some(successor) = m.successor {
                        observer.predecessor_changed(self, successor, page + 1, Some(old_phys_index), Some(phys_frame_index));
                    }
                    if let Some(predecessor) = m.predecessor {
                        observer.successor_changed(self, predecessor, page - 1, Some(old_phys_index), Some(phys_frame_index));
                    }

                    observer.mapping_removed(old_phys_index, page);
                    observer.mapping_added(phys_frame_index, page);
                } else {
                    // Nothing changed.
                }
            },
            Entry::Vacant(e) => {
                debug!("Now tracking: page {page} -> phys frame {phys_frame_index}");
                e.insert(phys_frame_index);

                let successor = self.lin_address_map.get(&(page + 1)).copied();
                let predecessor = self.lin_address_map.get(&(page - 1)).copied();
                self.info[phys_frame_index].mappings.push(PageMapping {
                    page,
                    successor,
                    predecessor,
                });

                if let Some(successor) = successor
                    && let Some(existing) = self.info[successor].mappings.iter_mut().find(|m| m.page == page + 1)
                {
                    assert!(existing.predecessor.is_none());
                    existing.predecessor = Some(phys_frame_index);
                }

                if let Some(predecessor) = predecessor
                    && let Some(existing) = self.info[predecessor].mappings.iter_mut().find(|m| m.page == page - 1)
                {
                    assert!(existing.successor.is_none());
                    existing.successor = Some(phys_frame_index);
                }

                let frame = &mut self.info[phys_frame_index];
                if !frame.page_mappings_current && frame.mappings.len() == 1 {
                    trace!("Page mapping made current by resolving {page}, which is the only mapping to {phys_frame_index}");
                    frame.page_mappings_current = true;
                    self.frame_chunks_with_page_mappings_current
                        .set(phys_frame_index.index() / PAGE_MAPPING_CHECKED_CHUNK_SIZE);
                    observer.page_mappings_current_changed(self, phys_frame_index, true);
                }

                if let Some(successor) = successor {
                    observer.predecessor_changed(self, successor, page + 1, None, Some(phys_frame_index));
                }
                if let Some(predecessor) = predecessor {
                    observer.successor_changed(self, predecessor, page - 1, None, Some(phys_frame_index));
                }

                observer.mapping_added(phys_frame_index, page);
            },
        }
    }

    fn remove_frame_page_mapping(&mut self, observer: &mut impl MappingObserver, page: LinPageIndex) {
        let Some(old_phys_index) = self.lin_address_map.remove(&page) else {
            trace!("No mapping for untracked page, ignoring: {page}");

            // We were not tracking this page, so we don't need to take any action.
            return
        };

        // All mappings where the successor was mapped at `(page - 1)` now have `phys_frame_index` as predecessor.
        let n = self.info[old_phys_index]
            .mappings
            .iter()
            .position(|m| m.page == page)
            .expect("frame should have mapping set if it is in `lin_address_map`");

        let m = self.info[old_phys_index].mappings.remove(n);
        trace!("Removed mapping: {m:X?}");

        if let Some(successor) = m.successor {
            let mapping = self.info[successor]
                .mappings
                .iter_mut()
                .find(|m| m.page == (page + 1))
                .unwrap();
            assert_eq!(mapping.predecessor, Some(old_phys_index));
            mapping.predecessor = None;
        }

        if let Some(predecessor) = m.predecessor {
            let mapping = self.info[predecessor]
                .mappings
                .iter_mut()
                .find(|m| m.page == (page - 1))
                .unwrap();
            assert_eq!(mapping.successor, Some(old_phys_index));
            mapping.successor = None;
        }

        if let Some(successor) = m.successor {
            observer.successor_changed(self, old_phys_index, page, Some(successor), None);
            observer.predecessor_changed(self, successor, page + 1, Some(old_phys_index), None);
        }

        if let Some(predecessor) = m.predecessor {
            observer.predecessor_changed(self, old_phys_index, page, Some(predecessor), None);
            observer.successor_changed(self, predecessor, page - 1, Some(old_phys_index), None);
        }

        observer.mapping_removed(old_phys_index, page);
    }

    pub fn frame_is_mapped_as(&self, phys_frame_index: PhysFrameIndex, expected_page: LinPageIndex) -> bool {
        self.info[phys_frame_index].mappings.iter().any(|m| m.page == expected_page)
    }

    pub fn lookup_cached_phys_addr(&self, page: LinPageIndex) -> PhysFrameIndex {
        match self.lin_address_map.get(&page) {
            Some(&phys_frame_index) => phys_frame_index,
            None => panic!(
                "page {page} has not been cached in MappingTracker: {:X?}",
                self.lin_address_map
            ),
        }
    }

    pub fn try_lookup_cached_phys_addr(&self, page: LinPageIndex) -> Option<PhysFrameIndex> {
        self.lin_address_map.get(&page).copied()
    }

    pub fn lin_predecessors(&self, phys_frame_index: PhysFrameIndex) -> impl Iterator<Item = PhysFrameIndex> {
        self.info[phys_frame_index].mappings.iter().flat_map(|m| m.predecessor)
    }

    pub fn lin_successors(&self, phys_frame_index: PhysFrameIndex) -> impl Iterator<Item = PhysFrameIndex> {
        self.info[phys_frame_index].mappings.iter().flat_map(|m| m.successor)
    }

    pub fn current_frame_mappings(&self, phys_frame_index: PhysFrameIndex) -> impl Iterator<Item = LinPageIndex> {
        self.info[phys_frame_index].mappings.iter().map(|m| m.page)
    }

    pub fn check_consistency(&self) {
        #[cfg(debug_assertions)]
        if false {
            for (index, info) in self.info.iter().enumerate() {
                let phys_frame_index = PhysFrameIndex::new(index as u32);
                for m in info.mappings.iter() {
                    assert_eq!(self.lin_address_map[&m.page], phys_frame_index);

                    if let Some(predecessor) = m.predecessor {
                        assert!(
                            self.info[predecessor]
                                .mappings
                                .iter()
                                .any(|other| other.page == m.page - 1 && other.successor == Some(phys_frame_index))
                        );
                    }

                    if let Some(successor) = m.successor {
                        assert!(
                            self.info[successor]
                                .mappings
                                .iter()
                                .any(|other| other.page == m.page + 1 && other.predecessor == Some(phys_frame_index))
                        );
                    }
                }
            }
        }
    }
}
