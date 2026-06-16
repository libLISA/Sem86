use std::fmt::Debug;
use std::num::NonZero;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arrayvec::ArrayVec;
use generativity::Guard;
use indexmap::{IndexMap, IndexSet};
use itertools::Itertools;
use liblisa::Instruction;
use log::{debug, trace};
use rand::{Rng, SeedableRng};
use rand_xoshiro::Xoshiro256Plus;
use sem86_arch::addr::{LinAddr, LinPageIndex, PhysAddr, PhysFrameIndex};
use sem86_arch::mem::{MarkDirtyAdvice, Mem32};
use thin_vec::ThinVec;

use super::debug::FrameSnapshot;
use crate::codegen::backends::Backend;
use crate::codegen::backends::inkwell::InkwellBackend;
use crate::codegen::mm::bump::BumpCodeAlloc;
use crate::codegen::page::{PageCode, PageInstr, PageJit, PageJitRequest, PageJitRequestData};
use crate::codegen::see::SingleEncodingExecution;
use crate::decoder::{Decoder, EncodingLookup, PackedInstrSem};
use crate::icache::entry::{CacheEntries, CacheEntry, CacheEntryId, EntryPoint, Links};
use crate::icache::mapping::{MappingObserver, MappingTracker};
use crate::icache::tlb::Tlb;
use crate::icache::{LookupProgress, PhysIndexedArray};
use crate::util::version::Versioner;

const CLEANING_DELAY: NonZero<u16> = NonZero::new(30_000).unwrap();
const CLEANING_RECHECK_THRESHOLD: u16 = CLEANING_DELAY.get() / 2;

pub(super) struct DirtyFrame {
    dirty_since: Instant,
    last_cleaning_check: Instant,
    phys_frame_index: PhysFrameIndex,
}

impl DirtyFrame {
    pub fn new(phys_frame_index: PhysFrameIndex) -> Self {
        Self {
            dirty_since: Instant::now(),
            last_cleaning_check: Instant::now(),
            phys_frame_index,
        }
    }

    pub fn mark_checked(&mut self) {
        self.last_cleaning_check = Instant::now();
    }
}

type B = InkwellBackend<'static>;

const NUM_FRAMES: usize = 1 << 20;
pub struct CheckingFlagsCache([bool; NUM_FRAMES]);

impl Default for CheckingFlagsCache {
    fn default() -> Self {
        Self::new()
    }
}

impl CheckingFlagsCache {
    pub fn new() -> Self {
        Self([false; NUM_FRAMES])
    }

    #[inline(always)]
    pub fn get(&self, index: PhysFrameIndex) -> bool {
        self.0[index.index()]
    }

    pub fn get_mut(&mut self, index: PhysFrameIndex) -> CheckingFlagsCacheRef<'_> {
        CheckingFlagsCacheRef {
            cache: self,
            index: index.index(),
        }
    }
}

pub struct CheckingFlagsCacheRef<'r> {
    cache: &'r mut CheckingFlagsCache,
    index: usize,
}

impl CheckingFlagsCacheRef<'_> {
    fn set(&mut self, val: bool) {
        self.cache.0[self.index] = val;
    }
}

pub(super) struct InnerCache<'tag> {
    // TODO: This currently uses ~64MiB. We should reduce this size.
    pub(super) entries: CacheEntries<'tag, <B as Backend>::UninstantiatedFn>,

    // TODO: Somewhat expensive indirection on each access.
    pub(super) phys_cache: PhysIndexedArray<FrameEntry<'tag>>,

    pub backend: SingleEncodingExecution<InkwellBackend<'static>>,
    pub decoder: Decoder<PackedInstrSem>,
    pub(super) page_compiler: PageJit,
    pub(super) num_frames_dirty: usize,
    pub(super) dirty_code_frames: IndexMap<PhysFrameIndex, DirtyFrame>,
    pub(super) num_frames_clean: usize,
    pub(super) num_dirty_frame_checks: usize,
    pub(super) tlb: Tlb<'tag, 12>,
    checking_flags_cache: CheckingFlagsCache,
    rng: Xoshiro256Plus,
    pub(super) page_versioner: Versioner,
    pub(super) page_alloc: BumpCodeAlloc,
    pub(super) jits_pending: IndexSet<PhysFrameIndex>,
    pub(super) page_jit_enabled: bool,
}

impl<'tag> MappingObserver for InnerCache<'tag> {
    #[inline(always)]
    fn predecessor_changed(
        &mut self, mapping: &MappingTracker, phys_frame_index: PhysFrameIndex, lin_page_index: LinPageIndex,
        old_predecessor: Option<PhysFrameIndex>, new_predecessor: Option<PhysFrameIndex>,
    ) {
        trace!(
            "Predecessor for {phys_frame_index} changed at linear address {lin_page_index}: {old_predecessor:X?} -> {new_predecessor:X?}"
        );
        // TODO: We're doing double work by essentially checking the same thing in `successor_changed` as well.
        self.update_overlap(mapping, phys_frame_index);
        self.update_successor_pages_dirty(mapping, phys_frame_index);
        self.update_successor_page_mapping_outdated(mapping, phys_frame_index);
    }

    fn successor_changed(
        &mut self, mapping: &MappingTracker, phys_frame_index: PhysFrameIndex, lin_page_index: LinPageIndex,
        old_successor: Option<PhysFrameIndex>, new_successor: Option<PhysFrameIndex>,
    ) {
        trace!(
            "Successor for {phys_frame_index} changed at linear address {lin_page_index}: {old_successor:X?} -> {new_successor:X?} (last entry in frame: {:X?})",
            self.phys_cache[phys_frame_index].instr_map.last()
        );

        // If there is an instruction at the end of this frame that crossed page bounds, we need to remove it.
        // Part of that instruction now points or may point to completely different memory.
        if let Some(&last_entry) = self.phys_cache[phys_frame_index].instr_map.last()
            && self.entries[last_entry.entry_index].crosses_page_bounds()
        {
            let removed = self.phys_cache[phys_frame_index].instr_map.pop().unwrap();
            trace!(
                "Removing {:?}, because successor of {phys_frame_index:?} changed",
                removed.entry_index
            );
            self.phys_cache[phys_frame_index].remove_links_to(&mut self.entries, removed.entry_index);
            self.entries.release_id(removed.entry_index);
        }

        if let Some(s) = new_successor {
            self.update_overlap(mapping, s);

            if self.phys_cache[s].is_dirty() && self.phys_cache[s].has_instruction_overlap_from_predecessor {
                self.phys_cache[phys_frame_index]
                    .update_checks_needed(self.checking_flags_cache.get_mut(phys_frame_index), |c| {
                        c.set_successor_page_dirty(true)
                    });
            }

            if !mapping.page_mapping_is_current(s) && self.phys_cache[s].has_instruction_overlap_from_predecessor {
                self.phys_cache[phys_frame_index]
                    .update_checks_needed(self.checking_flags_cache.get_mut(phys_frame_index), |c| {
                        c.set_successor_page_mapping_outdated(true)
                    });
            }
        }
    }

    fn mapping_added(&mut self, phys_frame_index: PhysFrameIndex, new_lin_page: LinPageIndex) {
        trace!("{phys_frame_index} is now mapped at {new_lin_page}");
    }

    fn mapping_removed(&mut self, phys_frame_index: PhysFrameIndex, old_lin_page: LinPageIndex) {
        trace!("{phys_frame_index} is no longer mapped at {old_lin_page}, removing all entry links");
        let frame = &mut self.phys_cache[phys_frame_index];
        for entry in frame.extra.prev_entry_indices.drain(..) {
            // TODO: Improvement: Only clear links to `old_lin_page` of the removed pages
            trace!("Removing links from {:?}", entry.entry);
            self.entries[entry.entry].clear_links();
        }

        self.tlb.clear();
    }

    #[inline(always)]
    fn page_mappings_current_changed(&mut self, mapping: &MappingTracker, phys_frame_index: PhysFrameIndex, is_current: bool) {
        trace!(
            "Mappings for {phys_frame_index} are now is_current={is_current} (mapped: {})",
            mapping
                .current_frame_mappings(phys_frame_index)
                .map(|x| format!("{x}"))
                .format(", ")
        );
        self.phys_cache[phys_frame_index].update_checks_needed(self.checking_flags_cache.get_mut(phys_frame_index), |c| {
            c.set_page_mapping_outdated(!is_current)
        });
        self.update_successor_page_mapping_outdated(mapping, phys_frame_index);
    }
}

impl<'tag> InnerCache<'tag> {
    pub fn new(guard: Guard<'tag>, backend: SingleEncodingExecution<B>, semantics: Arc<PackedInstrSem>) -> Self {
        Self {
            entries: CacheEntries::new(guard),
            backend,
            page_compiler: PageJit::new(semantics.clone()),
            decoder: Decoder::new(semantics),
            phys_cache: PhysIndexedArray::new(),
            page_versioner: Versioner::new(),
            num_frames_dirty: 0,
            dirty_code_frames: Default::default(),
            num_frames_clean: 0,
            num_dirty_frame_checks: 0,
            rng: Xoshiro256Plus::from_os_rng(),
            tlb: Tlb::new(),
            checking_flags_cache: CheckingFlagsCache::new(),
            // Allocation limit of 1GiB of chains
            page_alloc: BumpCodeAlloc::new(1 << 30),
            jits_pending: Default::default(),
            page_jit_enabled: true,
        }
    }

    pub fn advise_mark_dirty(&mut self, addr: PhysAddr, len: u8) -> MarkDirtyAdvice {
        let frame = &mut self.phys_cache[addr.into()];

        // TODO: Use binary search
        let offset = addr.frame_offset();
        if offset > 15
            && !frame.instr_map.is_empty()
            && frame
                .instr_map
                .iter()
                .all(|i| i.offset + i.len as u16 <= offset || offset + len as u16 <= i.offset)
        {
            MarkDirtyAdvice::DoNotMark
        } else {
            MarkDirtyAdvice::DirtyOk
        }
    }

    pub(super) fn prune_old_dirty_frames(&mut self) {
        if !self.dirty_code_frames.is_empty() {
            // If we wanted to be fully uniform, we would have to use random_range.
            // But since a bit of bias is fine, this is probably faster.
            let entry = &self.dirty_code_frames[self.rng.random::<u32>() as usize % self.dirty_code_frames.len()];
            if entry.last_cleaning_check.elapsed() > Duration::from_secs(15)
                && entry.dirty_since.elapsed() >= Duration::from_secs(30)
            {
                trace!("Pruning frame {}", entry.phys_frame_index);
                self.clear_phys_frame(entry.phys_frame_index);
            }
        }
    }

    pub(super) fn notify_memory_dirty(&mut self, mapping: &MappingTracker, phys_frame_index: PhysFrameIndex, memory: &Mem32) {
        debug!("Physical frame {phys_frame_index} is dirty");
        if self.phys_cache[phys_frame_index].cleaning_delay.is_none() {
            self.num_frames_dirty += 1;

            for item in self.phys_cache[phys_frame_index].instr_map.iter() {
                self.entries[item.entry_index].revert_to_single_execution(&mut self.backend, self.decoder.encodings());
            }

            self.phys_cache[phys_frame_index].has_requested_jit = false;
            self.phys_cache[phys_frame_index].has_received_jit = false;

            assert!(self.phys_cache[phys_frame_index].extra.cached_instr_bytes.is_none());
            if !self.phys_cache[phys_frame_index].instr_map.is_empty() {
                self.dirty_code_frames
                    .insert(phys_frame_index, DirtyFrame::new(phys_frame_index));
            }

            // If there are instructions stored on this frame, we need to keep track of what their bytes were.
            // If this frame contains the tail of an instruction on another page, we also need to keep track of its bytes.
            if !self.phys_cache[phys_frame_index].instr_map.is_empty()
                || self.phys_cache[phys_frame_index].has_instruction_overlap_from_predecessor
            {
                let mut data = [0; 4096];
                memory.read_physical_slice_no_mmio(phys_frame_index.start_address().as_u32(), &mut data);
                self.phys_cache[phys_frame_index].extra.cached_instr_bytes = Some(Box::new(data));
            }

            if self.phys_cache[phys_frame_index].counted_as_clean {
                self.num_frames_clean -= 1;
                self.phys_cache[phys_frame_index].counted_as_clean = false;
            }

            self.update_successor_pages_dirty(mapping, phys_frame_index);

            for instr in self.phys_cache[phys_frame_index].instr_map.iter() {
                self.entries[instr.entry_index].make_links_checked();
            }

            self.jits_pending.swap_remove(&phys_frame_index);
        } else {
            debug_assert!(self.phys_cache[phys_frame_index].checks_needed.dirty());
        }

        self.phys_cache[phys_frame_index].cleaning_delay = Some(CLEANING_DELAY);
        self.phys_cache[phys_frame_index]
            .update_checks_needed(self.checking_flags_cache.get_mut(phys_frame_index), |c| c.set_dirty(true));
    }

    pub(super) fn notify_all_page_mappings_updated(&mut self) {
        // TODO: Can we figure out a way to make this fast if CR3 regularly switches (~100x per second) between different values for different processes?
        // TODO: Maybe moving the cleaning cooldowns to a separate array to improve cache locality would help?
        // TODO: Global pages do not need to be updated for CR3 reloads.
        debug!("CR3 was reloaded, increased page version");
        debug!(
            "Checking flags for 0x0000322A___: {:?}",
            self.phys_cache[PhysAddr::new(0x0000322A000).into()].checks_needed
        );

        // Reset page mappings of chunks that had the `page_mapping_checked` flag set.
        // This is typically fast because there will not be many chunks that need updating.
        if self.page_versioner.increment() {
            self.tlb.clear();
        }

        #[cfg(debug_assertions)]
        for (index, entry) in self.phys_cache.iter().enumerate() {
            debug_assert!(
                entry.checks_needed.page_mapping_outdated(),
                "after CR3 reload, {} should have been marked as stale",
                PhysFrameIndex::new(index as u32)
            );
        }
    }

    #[inline(always)]
    pub fn frame_any_checks_needed(&self, phys_frame_index: PhysFrameIndex) -> bool {
        debug_assert_eq!(
            self.phys_cache[phys_frame_index].checks_needed.counters_active(),
            self.phys_cache[phys_frame_index].jit_page_in != 0
        );
        debug_assert_eq!(
            self.phys_cache[phys_frame_index].checks_needed.any(),
            self.checking_flags_cache.get(phys_frame_index),
            "checking flags cache inconsistency for frame {phys_frame_index}"
        );

        self.checking_flags_cache.get(phys_frame_index)
    }

    #[inline(always)]
    pub fn frame_checking_flags(&self, phys_frame_index: PhysFrameIndex) -> CheckingFlags {
        let checking_flags = self.phys_cache[phys_frame_index].checks_needed;
        debug_assert_eq!(
            checking_flags.counters_active(),
            self.phys_cache[phys_frame_index].jit_page_in != 0
        );
        checking_flags
    }

    pub fn find_entry_by_phys_addr(&mut self, phys: PhysAddr) -> Option<CacheEntryId<'tag>> {
        let phys_frame = PhysFrameIndex::from(phys);
        let frame = &mut self.phys_cache[phys_frame];

        let offset = phys.frame_offset();
        match frame.instr_map.binary_search_by_key(&offset, |entry| entry.offset) {
            Ok(index) => {
                let frame_instr = &frame.instr_map[index];
                debug_assert_eq!(frame_instr.offset, offset);
                let entry_index = frame_instr.entry_index;

                Some(entry_index)
            },
            Err(_) => None,
        }
    }

    pub fn update_cached_instr_bytes(&mut self, phys_addr: PhysAddr, bytes: &[u8], memory: &Mem32) {
        let phys_frame = phys_addr.into();
        let frame = &mut self.phys_cache[phys_frame];

        // If there were no instructions on this frame previously,
        // we might have marked it dirty without caching a copy of expected bytes.
        if frame.cleaning_delay.is_some() && frame.extra.cached_instr_bytes.is_none() {
            debug!("Creating missing cached_clean_bytes for frame {phys_frame}");
            assert!(frame.instr_map.is_empty());

            let mut data = [0; 4096];
            memory.read_physical_slice_no_mmio(phys_frame.start_address().as_u32(), &mut data);
            frame.extra.cached_instr_bytes = Some(Box::new(data));
        }

        trace!(
            "Updating cached bytes for phys={phys_addr} to {bytes:02X?} (is_dirty={}, has cached bytes: {})",
            frame.cleaning_delay.is_some(),
            frame.extra.cached_instr_bytes.is_some()
        );
        frame.extra.update_cached_instr_bytes(phys_addr.frame_offset(), bytes);
    }

    /// Fully clears a physical frame.
    /// Releases all cache entries used for instructions on this frame.
    /// If there is no instruction crossing into this page, the cached clean bytes are also removed.
    ///
    /// This is intended to free cache memory when a physical frame used to contain code, but now contains data.
    fn clear_phys_frame(&mut self, phys_frame_index: PhysFrameIndex) {
        debug!("Clearing cache for physical frame {phys_frame_index}");
        let frame = &mut self.phys_cache[phys_frame_index];
        for item in frame.instr_map.drain(..) {
            FrameEntry::remove_links_from_items_to(&mut self.entries, item.entry_index, &mut frame.extra.prev_entry_indices);
            self.entries.release_id(item.entry_index);
        }

        self.tlb.clear();

        if frame.cleaning_delay.is_some() {
            if !frame.has_instruction_overlap_from_predecessor {
                frame.extra.cached_instr_bytes = None;
            }

            self.dirty_code_frames.swap_remove(&phys_frame_index).unwrap();
        }

        self.mark_frame_as_needing_jit(phys_frame_index);

        // TODO: Update overlap from previous page
    }

    pub fn notify_existing_instr_changed(&mut self, mapping: &MappingTracker, phys_frame_index: PhysFrameIndex, memory: &Mem32) {
        let frame = &mut self.phys_cache[phys_frame_index];
        for item in frame.instr_map.iter() {
            self.entries[item.entry_index].revert_to_single_execution(&mut self.backend, self.decoder.encodings());
        }

        self.update_overlap_after_last_instr_changed(mapping, phys_frame_index, memory);
        self.mark_frame_as_needing_jit(phys_frame_index);
        self.tlb.clear();
    }

    fn update_overlap_after_last_instr_changed(
        &mut self, mapping: &MappingTracker, phys_frame_index: PhysFrameIndex, memory: &Mem32,
    ) {
        let frame = &mut self.phys_cache[phys_frame_index];
        if let Some(last) = frame.instr_map.last() {
            let overlap = self.entries[last.entry_index].crosses_page_bounds();
            if overlap {
                trace!("Last instruction on {phys_frame_index} ({last:X?}) now crosses into next frame");
                for successor in mapping.lin_successors(phys_frame_index) {
                    self.phys_cache[successor].has_instruction_overlap_from_predecessor = true;
                    if self.phys_cache[successor].is_dirty() && self.phys_cache[successor].extra.cached_instr_bytes.is_none() {
                        trace!(
                            "Quickly making copy of cached instruction bytes on {successor}, because a linear predecessor has a page-bounds-crossing instruction"
                        );
                        let mut data = [0; 4096];
                        memory.read_physical_slice_no_mmio(successor.start_address().as_u32(), &mut data);
                        self.phys_cache[successor].extra.cached_instr_bytes = Some(Box::new(data));
                    }

                    self.update_successor_pages_dirty(mapping, successor);
                    self.update_successor_page_mapping_outdated(mapping, successor);
                }
            }
        }
    }

    fn update_overlap(&mut self, mapping: &MappingTracker, phys_frame_index: PhysFrameIndex) {
        // TODO: Track whether there are multiple physical frames as predecessors. If there are, mark this in the `cache_validity` flags so we always reverify the page mapping.
        let overlap = mapping.lin_predecessors(phys_frame_index).any(|index| {
            self.phys_cache[index]
                .instr_map
                .last()
                .map(|x| x.crosses_page_bounds())
                .unwrap_or(false)
        });
        self.phys_cache[phys_frame_index].has_instruction_overlap_from_predecessor = overlap;
        trace!("Overlap from predecessors onto {phys_frame_index}: {overlap}");
    }

    fn update_successor_pages_dirty(&mut self, mapping: &MappingTracker, phys_frame_index: PhysFrameIndex) {
        if self.phys_cache[phys_frame_index].has_instruction_overlap_from_predecessor {
            for predecessor in mapping.lin_predecessors(phys_frame_index) {
                let successor_dirty = mapping
                    .lin_successors(predecessor)
                    .any(|n| self.phys_cache[n].cleaning_delay.is_some());
                trace!("Successors for {predecessor} are dirty={successor_dirty}");
                self.phys_cache[predecessor].update_checks_needed(self.checking_flags_cache.get_mut(predecessor), |c| {
                    c.set_successor_page_dirty(successor_dirty)
                });
            }
        }
    }

    fn update_successor_page_mapping_outdated(&mut self, mapping: &MappingTracker, phys_frame_index: PhysFrameIndex) {
        if self.phys_cache[phys_frame_index].has_instruction_overlap_from_predecessor {
            for predecessor in mapping.lin_predecessors(phys_frame_index) {
                let successor_outdated = mapping
                    .lin_successors(predecessor)
                    .any(|n| !mapping.page_mapping_is_current(n));
                trace!("Successors for {predecessor} have outdated page mappings={successor_outdated}");
                self.phys_cache[predecessor].update_checks_needed(self.checking_flags_cache.get_mut(predecessor), |c| {
                    c.set_successor_page_mapping_outdated(successor_outdated)
                });
            }
        }
    }

    pub fn remove_links_to(&mut self, entry_to_remove: CacheEntryId<'tag>) {
        let phys_index = self.entries[entry_to_remove].phys_frame_index();
        self.phys_cache[phys_index].remove_links_to(&mut self.entries, entry_to_remove);
    }

    pub fn add_link(&mut self, from: CacheEntryId<'tag>, to: CacheEntryId<'tag>, to_pc: LinAddr, jump_taken: bool) {
        let phys_frame_index = self.entries[to].phys_frame_index();
        let prev_phys_frame_index = self.entries[from].phys_frame_index();
        let same_frame = prev_phys_frame_index == phys_frame_index;
        let jump = self
            .decoder
            .encodings()
            .get(self.entries[from].encoding_info().encoding_index)
            .semantics
            .jump;

        let needs_checks = !same_frame
            || !jump.is_fixed_relative()
            || self.entries[from].crosses_page_bounds()
            || self.entries[to].crosses_page_bounds()
            || self.phys_cache[phys_frame_index].is_dirty();

        trace!("Adding link from {from:?} to {to:?} (target pc: {to_pc}) jump_taken={jump_taken}, needs_checks={needs_checks}");
        // if let UnpackedLinks::Speculative(s) = self.entries[from].links().unpack() && s.iter().any(|l| l.entry == to && l.lin_addr == to_pc) {
        //     // trace!("Skipping adding link from {from:?} to {to:?} (@ {to_pc} / {to_pc:?}), because links already contain {to:?}: {s:?}");
        //     return;
        // }

        let phys_index_to = self.entries[to].phys_frame_index();
        let removed = self.entries[from].set_link(to, to_pc, jump_taken, needs_checks);

        trace!("links from {from:?} are now: {:X?}", self.entries[from].links());

        if let Some(removed) = removed {
            self.phys_cache[self.entries[removed].phys_frame_index()]
                .extra
                .prev_entry_indices
                .retain_mut(|item| {
                    if item.entry == from {
                        item.count -= 1;
                        // trace!("Link from {from:?} to {removed:?} was replaced, {} remaining", item.count);

                        item.count > 0
                    } else {
                        true
                    }
                });
        }

        let frame_to = &mut self.phys_cache[phys_index_to];
        if let Some(item) = frame_to.extra.prev_entry_indices.iter_mut().find(|item| item.entry == from) {
            item.count += 1;
        } else {
            frame_to.extra.prev_entry_indices.push(PrevEntryIndex {
                entry: from,
                count: 1,
            })
        }
    }

    pub fn update_instr_len(&mut self, mapping: &MappingTracker, phys_addr: PhysAddr, len: u8, memory: &Mem32) {
        let phys_frame_index = PhysFrameIndex::from(phys_addr);
        let frame = &mut self.phys_cache[phys_frame_index];

        let offset = phys_addr.frame_offset();
        match frame.instr_map.binary_search_by_key(&offset, |entry| entry.offset) {
            Ok(index) => {
                let e = &mut frame.instr_map[index];
                e.len = len;

                // Remove extra instructions that might now overlap with this new instruction.
                let next_offset = e.offset + e.len as u16;
                let mut any_removed = false;
                while frame.instr_map.len() > index + 1 && frame.instr_map[index + 1].offset < next_offset {
                    let item = frame.instr_map.remove(index + 1);
                    trace!(
                        "Removing entry {:?}, because it overlaps with instruction at {phys_addr} of length {len}",
                        item.entry_index
                    );

                    FrameEntry::remove_links_from_items_to(
                        &mut self.entries,
                        item.entry_index,
                        &mut frame.extra.prev_entry_indices,
                    );
                    self.entries.release_id(item.entry_index);
                    any_removed = true;
                }

                if next_offset > 4096 {
                    for successor in mapping.lin_successors(phys_frame_index) {
                        self.phys_cache[successor].has_instruction_overlap_from_predecessor = true;
                    }
                }

                if any_removed {
                    self.notify_existing_instr_changed(mapping, phys_frame_index, memory);
                }
            },
            Err(_) => log::error!("called update_entry_len for missing frame entry"),
        }
    }

    pub fn total_frames_dirty(&self) -> usize {
        self.num_frames_dirty
    }

    pub fn code_frames_dirty(&self) -> usize {
        self.dirty_code_frames.len()
    }

    pub fn total_frames_clean(&self) -> usize {
        self.num_frames_clean
    }

    pub fn num_dirty_frame_checks(&self) -> usize {
        self.num_dirty_frame_checks
    }

    pub fn frame_has_chains(&self, phys_frame_index: PhysFrameIndex) -> bool {
        self.phys_cache[phys_frame_index].has_requested_jit
    }

    pub fn resolve_instr(
        &self, entry: &CacheEntry<'tag, <B as Backend>::UninstantiatedFn>, page: LinPageIndex, mem: &Mem32,
        mapping: &MappingTracker,
    ) -> Instruction {
        let bytes_on_first_frame = (4096 - entry.phys_addr().frame_offset() as usize).min(entry.instr_len() as usize);

        let phys_frame_index = entry.phys_frame_index();
        let frame = &self.phys_cache[phys_frame_index];
        let mut bytes = [0; 16];
        if entry.crosses_page_bounds() {
            let second_frame_index = mapping.lookup_cached_phys_addr(page + 1);
            let offset = 4096 - entry.phys_addr().frame_offset() as usize;
            let second_frame = &self.phys_cache[second_frame_index];

            let bytes_on_second_frame = entry.instr_len() as usize - bytes_on_first_frame;
            second_frame
                .extra
                .load_expected_bytes(phys_frame_index.start_address(), bytes_on_second_frame, mem, |data| {
                    bytes[bytes_on_first_frame..bytes_on_first_frame + data.len()].copy_from_slice(data);
                });

            second_frame
                .extra
                .load_actual_bytes(second_frame_index, entry, mem, |_, data| {
                    bytes[offset..offset + data.len()].copy_from_slice(data);
                });
        }

        frame
            .extra
            .load_expected_bytes(entry.phys_addr(), bytes_on_first_frame, mem, |data| {
                bytes[..data.len()].copy_from_slice(data);
            });

        Instruction::new(&bytes[..entry.instr_len() as usize])
    }

    #[inline(always)]
    pub fn should_jit_page(&mut self, phys_frame_index: PhysFrameIndex) -> bool {
        let frame = &mut self.phys_cache[phys_frame_index];
        if frame.jit_page_in != 0 {
            frame.jit_page_in = frame.jit_page_in.saturating_sub(1);

            if frame.jit_page_in == 1 {
                if frame.cleaning_delay.is_some() {
                    debug!("Avoiding chain compilations for frame {phys_frame_index}, because it is dirty");
                    frame.jit_page_in = u16::MAX;
                } else {
                    frame.update_counter_check_flags(self.checking_flags_cache.get_mut(phys_frame_index));
                    return true
                }
            }

            frame.update_counter_check_flags(self.checking_flags_cache.get_mut(phys_frame_index));
        }

        // TODO:
        // debug_assert!(frame.has_chains || frame.cleaning_delay.is_some() || frame.make_chains_in > 1, "frame {phys_frame_index} should either have chains, or have them pending: has_chains={}, dirty={}, make_chains_in={}", frame.has_chains, frame.cleaning_delay.is_some(), frame.make_chains_in);

        false
    }

    pub fn process_pagejit_request(&mut self, mapper: &MappingTracker) {
        if !self.jits_pending.is_empty() && self.page_compiler.num_pending_requests() < 30 {
            let requested_frame_index = self
                .jits_pending
                .swap_remove_index(self.rng.random::<u32>() as usize % self.jits_pending.len())
                .unwrap();
            assert!(!self.phys_cache[requested_frame_index].is_dirty());
            self.request_page_jit(requested_frame_index, mapper);
        }
    }

    pub(super) fn request_page_jit(&mut self, phys_frame_index: PhysFrameIndex, mapper: &MappingTracker) {
        let frame = &mut self.phys_cache[phys_frame_index];
        if frame.cleaning_delay.is_some() {
            // There is no point in generating dispatch tables for dirty pages
            return
        }

        frame.jit_page_in = 0;

        self.jits_pending.swap_remove(&phys_frame_index);

        frame.has_requested_jit = true;
        frame.has_received_jit = false;

        debug!("Requesting JIT for frame {phys_frame_index}");
        let request = PageJitRequest {
            phys_frame_index,
            data: PageJitRequestData {
                instrs: frame
                    .instr_map
                    .iter()
                    .filter(|frame_entry| !self.entries[frame_entry.entry_index].crosses_page_bounds())
                    .map(|frame_entry| {
                        let entry = &self.entries[frame_entry.entry_index];
                        PageInstr {
                            offset: frame_entry.offset,
                            // TODO: We should only need external or higher entry points here, but somehow that leads to >33% misses.
                            is_entry: entry.flags().entry_kind() >= EntryPoint::Local,
                            part_values: entry.encoding_info().part_values,
                            instr_len: entry.instr_len(),
                            encoding_index: entry.encoding_info().encoding_index as u32,
                            protected_mode: entry.env().effective_protected_mode(),
                            segment_sizes: entry.env().segment_sizes(),
                            next: {
                                let mut next = ArrayVec::new();
                                if let Links::Speculative(links) = entry.links() {
                                    // If it is possible for this instruction to jump within the page, we add the jump target offset.
                                    for &addr in links {
                                        if mapper
                                            .current_frame_mappings(phys_frame_index)
                                            .all(|lin| lin == LinPageIndex::from(addr))
                                        {
                                            next.push(addr.page_offset());
                                        }
                                    }
                                }

                                next
                            },
                        }
                    })
                    .collect::<Vec<_>>(),
            },
        };

        self.page_compiler.request_compilation(request);
    }

    pub fn receive_compiled_chains(&mut self) {
        'next: while let Some(result) = self.page_compiler.recv() {
            let phys_frame_index = result.phys_frame_index;
            let frame = &mut self.phys_cache[phys_frame_index];
            // Frame became dirty, ignore stale chains
            if frame.is_dirty() {
                debug!("Discarding compiled page, because physical frame {phys_frame_index} is dirty");
                frame.has_requested_jit = false;
                continue
            }

            let mut ids = Vec::new();
            for instr in result.instrs.iter() {
                let Ok(index) = frame.instr_map.binary_search_by_key(&instr.offset, |e| e.offset) else {
                    debug!("Discarding compiled page, because instruction is missing on physical frame {phys_frame_index} ");
                    continue 'next
                };
                let a = &frame.instr_map[index];
                let e = &self.entries[a.entry_index];

                if a.offset != instr.offset
                    || a.len != instr.instr_len
                    || instr.protected_mode != e.env().effective_protected_mode()
                    || instr.encoding_index != e.encoding_info().encoding_index as u32
                    || instr.part_values != e.encoding_info().part_values
                    || instr.segment_sizes != e.env().segment_sizes()
                {
                    debug!(
                        "Discarding compiled page, because instructions differ on physical frame {phys_frame_index}: {instr:X?}"
                    );
                    frame.has_requested_jit = false;
                    continue 'next
                }

                ids.push(a.entry_index);
            }

            trace!("Received JIT for {phys_frame_index}");
            frame.has_received_jit = true;
            let mut code = PageCode::from_result(&result, &ids, &mut self.page_alloc);
            for (index, (&id, instr)) in ids.iter().zip(result.instrs.iter()).enumerate() {
                if instr.is_entry {
                    let (name, code) = code.next().unwrap();
                    debug!(
                        "Attaching {name:?} ({:p}) to entry #{index} at {}",
                        code.function().as_fptr(),
                        self.entries[id].phys_addr()
                    );
                    self.entries[id].set_jitted_page(code);
                }
            }

            assert!(code.next().is_none());
        }
    }

    pub fn debug_snapshot(&self) -> Vec<FrameSnapshot> {
        self.phys_cache
            .0
            .iter()
            .map(|frame| FrameSnapshot {
                is_dirty: frame.cleaning_delay.is_some(),
                page_jit_pending: frame.jit_page_in != 0,
            })
            .collect()
    }

    pub fn insert_entry(&mut self, mapping: &MappingTracker, id: CacheEntryId<'tag>, memory: &Mem32) {
        let phys_frame_index = self.entries[id].phys_frame_index();

        let frame = &mut self.phys_cache[phys_frame_index];
        let offset = self.entries[id].frame_offset();
        match frame.instr_map.binary_search_by_key(&offset, |entry| entry.offset) {
            Ok(_) => panic!("Should not call insert_entry for existing entries"),
            Err(insert_index) => {
                assert!(
                    !memory.phys_frame_is_dirty(phys_frame_index.start_address().as_u32() as u64)
                        || frame.cleaning_delay.is_some()
                );
                // If we have nothing cached, immediately mark the frame clean if it is dirty.
                if frame.cleaning_delay.is_some() && frame.instr_map.is_empty() {
                    // We can mark the frame clean if there are no entries cached for this frame (or partially on this frame).
                    if !frame.has_instruction_overlap_from_predecessor {
                        debug!("Cleaning frame {phys_frame_index} (which contains no instructions yet)");
                        memory.clean_phys_frame(phys_frame_index.start_address().as_u32() as u64);

                        self.num_frames_dirty -= 1;
                        self.num_frames_clean += 1;
                        frame.counted_as_clean = true;

                        frame.cleaning_delay = None;
                        frame.update_checks_needed(self.checking_flags_cache.get_mut(phys_frame_index), |c| c.set_dirty(false));
                        frame.extra.cached_instr_bytes = None;
                        frame.jit_page_in = 40_000;
                        frame.update_counter_check_flags(self.checking_flags_cache.get_mut(phys_frame_index));
                        self.add_pending_pagejit(phys_frame_index);

                        self.update_successor_pages_dirty(mapping, phys_frame_index);
                    } else {
                        self.dirty_code_frames
                            .insert(phys_frame_index, DirtyFrame::new(phys_frame_index));
                        trace!(
                            "Quickly making copy of cached instruction bytes on {phys_frame_index}, because we're about to insert an entry for it"
                        );
                        let mut data = [0; 4096];
                        memory.read_physical_slice_no_mmio(phys_frame_index.start_address().as_u32(), &mut data);
                        self.phys_cache[phys_frame_index].extra.cached_instr_bytes = Some(Box::new(data));
                    }
                }

                let len = self.entries[id].instr_len();
                let frame = &mut self.phys_cache[phys_frame_index];
                frame.instr_map.insert(
                    insert_index,
                    FrameInstr {
                        entry_index: id,
                        offset,
                        len,
                    },
                );

                // Remove extra instructions that might now overlap with this new instruction.
                let next_offset = offset + len as u16;
                let mut any_removed = false;
                let phys_addr = self.entries[id].phys_addr();
                while frame.instr_map.len() > insert_index + 1 && frame.instr_map[insert_index + 1].offset < next_offset {
                    let item = frame.instr_map.remove(insert_index + 1);
                    FrameEntry::remove_links_from_items_to(
                        &mut self.entries,
                        item.entry_index,
                        &mut frame.extra.prev_entry_indices,
                    );
                    trace!(
                        "Removing entry {:?}, because it overlaps with instruction at {phys_addr} of length {len}",
                        item.entry_index
                    );
                    self.entries.release_id(item.entry_index);
                    any_removed = true;
                }

                if insert_index > 0 {
                    let before = frame.instr_map[insert_index - 1];
                    if before.offset + before.len as u16 > offset {
                        let item = frame.instr_map.remove(insert_index - 1);
                        FrameEntry::remove_links_from_items_to(
                            &mut self.entries,
                            before.entry_index,
                            &mut frame.extra.prev_entry_indices,
                        );
                        trace!(
                            "Removing entry {:?}, because it overlaps with instruction at {phys_addr} of length {len}",
                            item.entry_index
                        );
                        self.entries.release_id(before.entry_index);
                        any_removed = true;
                    }
                }

                frame.jit_page_in = 40_000;
                frame.update_counter_check_flags(self.checking_flags_cache.get_mut(phys_frame_index));

                if insert_index == frame.instr_map.len() - 1 {
                    self.update_overlap_after_last_instr_changed(mapping, phys_frame_index, memory);
                }

                if !self.phys_cache[phys_frame_index].is_dirty() {
                    self.add_pending_pagejit(phys_frame_index);
                }

                if any_removed {
                    self.notify_existing_instr_changed(mapping, phys_frame_index, memory);
                }
            },
        }
    }

    pub fn check_dirty_memory(
        &mut self, mapping: &MappingTracker, id: CacheEntryId<'tag>, memory: &Mem32, progress: &mut impl FnMut(LookupProgress),
    ) -> bool {
        let entry = &self.entries[id];
        let phys_frame = entry.phys_frame_index();
        let frame = &mut self.phys_cache[phys_frame];
        debug_assert_eq!(
            frame.cleaning_delay.is_some(),
            frame.extra.cached_instr_bytes.is_some(),
            "if cleaning delay is set ({:?}), cached_clean_bytes must also be set. frame: {phys_frame}",
            frame.cleaning_delay
        );
        debug_assert!(
            !memory.phys_frame_is_dirty(entry.phys_addr().as_u32() as u64) || frame.cleaning_delay.is_some(),
            "cleaning delay must be set when frame is dirty"
        );
        debug_assert_eq!(frame.checks_needed.dirty(), frame.cleaning_delay.is_some());
        debug_assert_eq!(
            frame.is_dirty(),
            frame.extra.cached_instr_bytes.is_some(),
            "frame {phys_frame} is dirty but does not have cached instruction bytes"
        );

        if let Some(cleaning_delay) = &mut frame.cleaning_delay {
            // trace!("Verifying memory of {id:?} on {phys_frame}");
            self.num_dirty_frame_checks += 1;
            if !frame.extra.entry_matches_memory(phys_frame, entry, memory) {
                trace!("Memory does not match");
                // TODO: We can be more efficient here by signalling that the entry id has not been recycled
                return true
            }

            let next_counter = cleaning_delay.get() - 1;
            if next_counter == CLEANING_RECHECK_THRESHOLD {
                self.dirty_code_frames.get_mut(&phys_frame).unwrap().mark_checked();
                memory.clean_phys_frame(phys_frame.start_address().as_u32() as u64);
            }

            if let Some(remaining) = NonZero::new(next_counter) {
                *cleaning_delay = remaining
            } else {
                progress(LookupProgress::CleanFrame);

                debug!("Cleaning frame {phys_frame}: {frame:X?}");

                // Re-enable notifications for when frame is marked as dirty.
                memory.clean_phys_frame(phys_frame.start_address().as_u32() as u64);

                let frame = &mut self.phys_cache[phys_frame];
                frame.instr_map.retain(|item| {
                    let entry = &self.entries[item.entry_index];
                    // We only check if the part of the entry on this frame is still identical.
                    // The second frame is always checked separately if the instruction crosses page bounds,
                    // so this cannot not cause stale cache entries to be used.
                    let ok = frame.extra.entry_matches_memory(entry.phys_frame_index(), entry, memory);
                    if !ok {
                        FrameEntry::remove_links_from_items_to(
                            &mut self.entries,
                            item.entry_index,
                            &mut frame.extra.prev_entry_indices,
                        );
                        self.entries.release_id(item.entry_index);
                    } else {
                        self.entries[item.entry_index].clear_links();
                        // TODO: Assert no dispatch table/chains exist
                    }

                    ok
                });

                // Count frame as clean
                self.dirty_code_frames.swap_remove(&phys_frame).unwrap();
                self.num_frames_dirty -= 1;
                self.num_frames_clean += 1;
                frame.counted_as_clean = true;

                // No longer needed now that the frame is clean.
                frame.extra.cached_instr_bytes = None;
                frame.cleaning_delay = None;
                frame.jit_page_in = 60_000;
                frame.update_checks_needed(self.checking_flags_cache.get_mut(phys_frame), |c| c.set_dirty(false));
                frame.update_counter_check_flags(self.checking_flags_cache.get_mut(phys_frame));
                frame.has_received_jit = false;
                frame.has_requested_jit = false;
                self.add_pending_pagejit(phys_frame);

                self.tlb.clear();

                self.update_successor_pages_dirty(mapping, phys_frame);

                // Since memory of entry_id matched (see if above), we have not released the ID.
            }
        }

        false
    }

    pub fn all_code_pages_jitted(&self) -> bool {
        self.phys_cache
            .iter()
            .all(|frame| frame.instr_map.is_empty() || frame.cleaning_delay.is_some() || frame.has_received_jit)
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.phys_cache = PhysIndexedArray::new();
        self.page_versioner = Versioner::new();
        self.dirty_code_frames = Default::default();
        self.num_frames_dirty = 0;
        self.num_frames_clean = 0;
        self.tlb.clear();
        self.checking_flags_cache = CheckingFlagsCache::new();
    }

    pub fn mark_frame_as_needing_jit(&mut self, phys_frame_index: PhysFrameIndex) {
        let frame = &mut self.phys_cache[phys_frame_index];
        if frame.jit_page_in == 0 {
            frame.has_requested_jit = false;
            frame.jit_page_in = 40_000;
            frame.update_counter_check_flags(self.checking_flags_cache.get_mut(phys_frame_index));
            self.add_pending_pagejit(phys_frame_index);
        }
    }

    fn add_pending_pagejit(&mut self, phys_frame: PhysFrameIndex) {
        if self.page_jit_enabled {
            trace!("JIT pending: {phys_frame}");
            self.jits_pending.insert(phys_frame);

            self.phys_cache[phys_frame].has_received_jit = false;
        }
    }

    pub fn set_pagejit_enabled(&mut self, enable: bool) {
        if self.page_jit_enabled && !enable {
            self.jits_pending.clear();
        }

        self.page_jit_enabled = enable;
        // TODO: If enabled, reset all counters
        // TODO: If enabled, add all frames without JITed pages
    }
}

/// We can store this struct when we want to store a physical frame index that we are going to use
/// to index into the physical cache.
/// That saves us from having to compute the byte offset every time we index,
/// as this struct effectively has it precomputed already.
#[derive(Copy, Clone, Debug, Default)]
pub struct PhysFrameIndexTimesPhysCacheEntrySize(u32);

impl From<PhysFrameIndex> for PhysFrameIndexTimesPhysCacheEntrySize {
    #[inline(always)]
    fn from(value: PhysFrameIndex) -> Self {
        let size = size_of::<FrameEntry<'_>>();
        assert!(size < 4096);
        Self(value.index() as u32 * size as u32)
    }
}

impl From<PhysFrameIndexTimesPhysCacheEntrySize> for PhysFrameIndex {
    #[inline(always)]
    fn from(value: PhysFrameIndexTimesPhysCacheEntrySize) -> Self {
        let size = size_of::<FrameEntry<'_>>();
        PhysFrameIndex::new(value.0 / size as u32)
    }
}

#[derive(Copy, Clone, Debug)]
pub(super) struct FrameInstr<'tag> {
    offset: u16,
    len: u8,
    pub(super) entry_index: CacheEntryId<'tag>,
}

impl FrameInstr<'_> {
    pub fn crosses_page_bounds(&self) -> bool {
        self.offset + self.len as u16 > 4096
    }
}

#[derive(Copy, Clone, Default)]
pub(super) struct CheckingFlags {
    value: u8,
}

impl CheckingFlags {
    const PAGE_MAPPING_OUTDATED: u8 = 0x01;
    const DIRTY: u8 = 0x02;
    const SUCCESSOR_PAGE_DIRTY: u8 = 0x04;
    const SUCCESSOR_PAGE_MAPPING_OUTDATED: u8 = 0x08;
    const COUNTERS_ACTIVE: u8 = 0x10;

    #[inline(always)]
    pub fn page_mapping_outdated(&self) -> bool {
        self.value & Self::PAGE_MAPPING_OUTDATED != 0
    }

    #[inline(always)]
    pub fn dirty(&self) -> bool {
        self.value & Self::DIRTY != 0
    }

    #[inline(always)]
    pub fn successor_page_dirty(&self) -> bool {
        self.value & Self::SUCCESSOR_PAGE_DIRTY != 0
    }

    #[inline(always)]
    pub fn successor_page_mapping_outdated(&self) -> bool {
        self.value & Self::SUCCESSOR_PAGE_MAPPING_OUTDATED != 0
    }

    #[inline(always)]
    pub fn counters_active(&self) -> bool {
        self.value & Self::COUNTERS_ACTIVE != 0
    }

    #[inline(always)]
    pub fn set_page_mapping_outdated(&mut self, value: bool) {
        if value {
            self.value |= Self::PAGE_MAPPING_OUTDATED;
        } else {
            self.value &= !Self::PAGE_MAPPING_OUTDATED;
        }
    }

    #[inline(always)]
    pub fn set_dirty(&mut self, value: bool) {
        if value {
            self.value |= Self::DIRTY;
        } else {
            self.value &= !Self::DIRTY;
        }
    }

    #[inline(always)]
    pub fn set_successor_page_dirty(&mut self, value: bool) {
        if value {
            self.value |= Self::SUCCESSOR_PAGE_DIRTY;
        } else {
            self.value &= !Self::SUCCESSOR_PAGE_DIRTY;
        }
    }

    #[inline(always)]
    pub fn set_successor_page_mapping_outdated(&mut self, value: bool) {
        if value {
            self.value |= Self::SUCCESSOR_PAGE_MAPPING_OUTDATED;
        } else {
            self.value &= !Self::SUCCESSOR_PAGE_MAPPING_OUTDATED;
        }
    }

    #[inline(always)]
    pub fn set_counters_active(&mut self, value: bool) {
        if value {
            self.value |= Self::COUNTERS_ACTIVE;
        } else {
            self.value &= !Self::COUNTERS_ACTIVE;
        }
    }

    #[inline(always)]
    pub fn any(&self) -> bool {
        self.value != 0
    }

    #[inline(always)]
    pub fn needs_dirty_check(&self) -> bool {
        let check_bits = Self::DIRTY | Self::SUCCESSOR_PAGE_DIRTY | Self::SUCCESSOR_PAGE_MAPPING_OUTDATED;
        self.value & check_bits != 0
    }

    #[inline(always)]
    pub fn needs_mapping_check(&self) -> bool {
        self.page_mapping_outdated()
    }

    pub fn needs_counters_check(&self) -> bool {
        self.counters_active()
    }

    pub fn needs_only_successor_page_mapping_check(&self) -> bool {
        self.value == Self::SUCCESSOR_PAGE_MAPPING_OUTDATED
    }
}

impl Debug for CheckingFlags {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CheckingFlags")
            .field("page_mapping_outdated", &self.page_mapping_outdated())
            .field("dirty", &self.dirty())
            .field("successor_page_dirty", &self.successor_page_dirty())
            .field("successor_page_mapping_outdated", &self.successor_page_mapping_outdated())
            .field("counters_active", &self.counters_active())
            .finish()
    }
}

#[derive(Clone)]
pub(super) struct FrameEntry<'tag> {
    /// This is a general "needs checking" flag.
    /// It is set whenever the frame is dirty or the page mapping might have been updated.
    /// It is also set when a linear successor is dirty and this frame's last instruction crosses page bounds.
    ///
    /// This flag should be the only flag that needs checking when trying to determine if we can execute the next instruction.
    pub(super) checks_needed: CheckingFlags,

    /// If `Some(delay)`, the frame is dirty.
    /// `delay` indicates how many executions we will continue to wait before rechecking all instructions in this frame.
    pub(super) cleaning_delay: Option<NonZero<u16>>,

    /// Maps offsets in this physical frame to instruction entries.
    pub(super) instr_map: Vec<FrameInstr<'tag>>,

    /// Infrequently accessed data is stored in a box to keep the structure size small.
    pub(super) extra: FrameEntryExtra<'tag>,

    pub(super) has_requested_jit: bool,
    pub(super) has_received_jit: bool,
    counted_as_clean: bool,

    // TODO: Don't use counters for page jit. Instead, Add phys_frame_index to list when we need to JIT. Then read some entries from the list in the function called periodically.
    pub(super) jit_page_in: u16,
    has_instruction_overlap_from_predecessor: bool,
}

impl Debug for FrameEntry<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameEntry")
            .field("cleaning_delay", &self.cleaning_delay)
            .field("instr_map", &DebugInstrMap(&self.instr_map))
            .field("extra", &self.extra)
            .field("counted_as_clean", &self.counted_as_clean)
            .finish()
    }
}

struct DebugInstrMap<'a, 'tag>(&'a [FrameInstr<'tag>]);

impl Debug for DebugInstrMap<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut f = f.debug_set();
        for item in self.0.iter() {
            f.entry(&item.offset);
        }

        f.finish()
    }
}

#[derive(Clone)]
pub(super) struct FrameEntryExtra<'tag> {
    /// A list of all entries that link to entries in this physical frame.
    /// We need to remove these links if the page mapping for this frame is updated.
    /// TODO: We might want to group these by linear address so that we can more efficiently remove only the links that need removal
    pub(super) prev_entry_indices: ThinVec<PrevEntryIndex<'tag>>,

    /// Contains the bytes of currently cached instructions.
    /// Is only present when the page is dirty.
    ///
    /// This is used to check whether cached instructions still correspond to the
    /// actual bytes in memory.
    /// It is updated when a cached instruction is updated.
    pub(super) cached_instr_bytes: Option<Box<[u8; 4096]>>,
}

impl Debug for FrameEntryExtra<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameEntryExtra")
            .field("cached_clean_bytes", &self.cached_instr_bytes)
            .finish()
    }
}

#[derive(Clone, Debug)]
pub(super) struct PrevEntryIndex<'tag> {
    entry: CacheEntryId<'tag>,
    count: u32,
}

impl<'tag> FrameEntry<'tag> {
    pub fn remove_links_to<F>(&mut self, entries: &mut CacheEntries<'tag, F>, entry_to_remove: CacheEntryId<'tag>) {
        Self::remove_links_from_items_to(entries, entry_to_remove, &mut self.extra.prev_entry_indices)
    }

    fn remove_links_from_items_to<F>(
        entries: &mut CacheEntries<'tag, F>, entry_to_remove: CacheEntryId<'tag>,
        prev_entry_indices: &mut ThinVec<PrevEntryIndex<'tag>>,
    ) {
        prev_entry_indices.retain_mut(|item| {
            if entries[item.entry].remove_links_matching(entry_to_remove) {
                item.count -= 1;

                trace!(
                    "Removed link from {:?} to {entry_to_remove:?}: {} references remaining",
                    item.entry, item.count
                );
                trace!("Current links from {:?}: {:?}", item.entry, entries[item.entry].links());
                if item.count == 0 {
                    return false
                }
            }

            true
        })
    }

    pub fn is_dirty(&self) -> bool {
        self.cleaning_delay.is_some()
    }

    pub fn update_checks_needed_cache(&self, mut r: CheckingFlagsCacheRef<'_>) {
        r.set(self.checks_needed.any())
    }

    fn update_counter_check_flags(&mut self, r: CheckingFlagsCacheRef<'_>) {
        self.checks_needed.set_counters_active(self.jit_page_in != 0);
        self.update_checks_needed_cache(r);
    }

    fn update_checks_needed(&mut self, r: CheckingFlagsCacheRef<'_>, update: impl FnOnce(&mut CheckingFlags)) {
        update(&mut self.checks_needed);
        self.update_checks_needed_cache(r);
    }
}

impl FrameEntryExtra<'_> {
    fn load_actual_bytes<F, T>(
        &self, frame_index: PhysFrameIndex, entry: &CacheEntry<'_, F>, memory: &Mem32, result: impl FnOnce(u16, &[u8]) -> T,
    ) -> T {
        let (offset, len) = if entry.phys_frame_index() == frame_index {
            // Inspecting current frame
            let frame_offset = entry.phys_addr().frame_offset();
            let len = (4096 - frame_offset).min(entry.instr_len() as u16) as usize;
            (frame_offset, len)
        } else {
            // Instruction crosses page bounds, inspecting second half on second page.
            let frame_offset = entry.phys_addr().frame_offset();
            let len = ((frame_offset + entry.instr_len() as u16) - 4096) as usize;
            (0, len)
        };

        let mut actual_bytes = [0; 16];
        memory.read_physical_slice_no_mmio(frame_index.start_address().as_u32() + offset as u32, &mut actual_bytes[..len]);

        result(offset, &actual_bytes[..len])
    }

    pub fn entry_matches_memory<F>(&self, frame_index: PhysFrameIndex, entry: &CacheEntry<'_, F>, memory: &Mem32) -> bool {
        self.load_actual_bytes(frame_index, entry, memory, |offset, actual_bytes| {
            let Some(expected_bytes) = self.cached_instr_bytes.as_ref() else {
                panic!("entry_matches_memory was called on {frame_index} which does not have `cached_instr_bytes`")
            };
            let expected_bytes = &expected_bytes[offset as usize..offset as usize + actual_bytes.len()];
            debug_assert!(!actual_bytes.is_empty());

            // trace!("expected_bytes={expected_bytes:02X?}, actual_bytes={actual_bytes:02X?}");

            expected_bytes == actual_bytes
        })
    }

    pub fn load_expected_bytes<T>(&self, addr: PhysAddr, len: usize, memory: &Mem32, result: impl FnOnce(&[u8]) -> T) -> T {
        if let Some(expected_bytes) = self.cached_instr_bytes.as_ref() {
            result(&expected_bytes[addr.frame_offset() as usize..addr.frame_offset() as usize + len])
        } else {
            let mut actual_bytes = [0; 16];
            memory.read_physical_slice_no_mmio(addr.as_u32(), &mut actual_bytes[..len]);
            result(&actual_bytes[..len])
        }
    }

    pub fn update_cached_instr_bytes(&mut self, offset: u16, new_bytes: &[u8]) {
        if let Some(current_bytes) = self.cached_instr_bytes.as_mut() {
            let len = (4096 - offset as usize).min(new_bytes.len());
            current_bytes[offset as usize..offset as usize + len].copy_from_slice(&new_bytes[..len]);
        }
    }
}

impl Default for FrameEntry<'_> {
    fn default() -> Self {
        Self {
            has_instruction_overlap_from_predecessor: false,
            checks_needed: {
                let mut f = CheckingFlags::default();
                f.set_page_mapping_outdated(true);
                f.set_counters_active(true);
                f
            },
            instr_map: Vec::new(),
            cleaning_delay: None,
            counted_as_clean: false,
            jit_page_in: 60_000,
            has_requested_jit: false,
            has_received_jit: false,
            extra: FrameEntryExtra {
                prev_entry_indices: Default::default(),
                cached_instr_bytes: None,
            },
        }
    }
}
