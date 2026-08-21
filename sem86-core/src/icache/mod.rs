use std::fmt::Display;
use std::ops::{Index, IndexMut};
use std::sync::Arc;

use arrayvec::ArrayVec;
use bilge::prelude::*;
use generativity::Guard;
use liblisa::Instruction;
use log::{error, trace};
use sem86_arch::addr::{LinAddr, LinPageIndex, PhysAddr, PhysFrameIndex};
use sem86_arch::exceptions::Exception;
use sem86_arch::mem::{Mem32, Shm};

use crate::SegmentSizes;
use crate::codegen::backends::Backend;
use crate::codegen::backends::inkwell::InkwellBackend;
use crate::codegen::mir::InstructionEntry;
use crate::codegen::page::PageJit;
use crate::codegen::see::SingleEncodingExecution;
use crate::decoder::{Decoder, EncodingLookup, PackedInstrSem};
use crate::icache::debug::CacheSnapshot;
use crate::icache::entry::{CacheEntry, CacheEntryId, EntryEnv, EntryPoint, Links};
use crate::icache::exec::{EncodingInfo, Executable};
use crate::icache::inner::InnerCache;
use crate::icache::mapping::MappingTracker;
use crate::util::miniprofiler::EncodeU64;

pub mod debug;
pub mod entry;
pub mod exec;
pub mod inner;
pub mod mapping;
pub mod tlb;
pub mod zoc;

struct PhysIndexedArray<T>(Box<[T; 1 << 20]>);

impl<T: Clone + Default> PhysIndexedArray<T> {
    pub fn new() -> Self {
        Self(match vec![T::default(); 1 << 20].into_boxed_slice().try_into() {
            Ok(data) => data,
            Err(_) => unreachable!(),
        })
    }

    fn iter(&self) -> impl Iterator<Item = &T> {
        self.0.iter()
    }
}

impl<T> Index<PhysFrameIndex> for PhysIndexedArray<T> {
    type Output = T;

    fn index(&self, index: PhysFrameIndex) -> &Self::Output {
        &self.0[index.index()]
    }
}

impl<T> IndexMut<PhysFrameIndex> for PhysIndexedArray<T> {
    fn index_mut(&mut self, index: PhysFrameIndex) -> &mut Self::Output {
        &mut self.0[index.index()]
    }
}

#[derive(Copy, Clone, Debug, bitcode::Encode, bitcode::Decode)]
pub enum LookupProgress {
    ScanNextBlocks,
    Lookup,
    Unused1,
    ComputeInstrChain,
    RewalkPages,
    DecodeInstr,
    VerifyPageMapping,
    UpdateNext,
    CacheMiss,
    CacheHit,
    CleanDirtyEntry,
    VerifyDirtyFrame,
    CleanFrame,
    VerifyDirtyEntry,
    CompilationBookkeeping,
    PageWalk,
    SeeJit,
}

impl EncodeU64 for LookupProgress {
    fn encode(&self) -> u64 {
        match self {
            LookupProgress::ScanNextBlocks => 0,
            LookupProgress::Lookup => 1,
            LookupProgress::Unused1 => 2,
            LookupProgress::ComputeInstrChain => 3,
            LookupProgress::RewalkPages => 4,
            LookupProgress::DecodeInstr => 5,
            LookupProgress::VerifyPageMapping => 6,
            LookupProgress::UpdateNext => 7,
            LookupProgress::CacheMiss => 8,
            LookupProgress::CacheHit => 9,
            LookupProgress::CleanDirtyEntry => 10,
            LookupProgress::VerifyDirtyFrame => 11,
            LookupProgress::CleanFrame => 12,
            LookupProgress::VerifyDirtyEntry => 13,
            LookupProgress::CompilationBookkeeping => 14,
            LookupProgress::PageWalk => 15,
            LookupProgress::SeeJit => 16,
        }
    }

    fn decode(val: u64) -> Self {
        match val {
            0 => LookupProgress::ScanNextBlocks,
            1 => LookupProgress::Lookup,
            2 => LookupProgress::Unused1,
            3 => LookupProgress::ComputeInstrChain,
            4 => LookupProgress::RewalkPages,
            5 => LookupProgress::DecodeInstr,
            6 => LookupProgress::VerifyPageMapping,
            7 => LookupProgress::UpdateNext,
            8 => LookupProgress::CacheMiss,
            9 => LookupProgress::CacheHit,
            10 => LookupProgress::CleanDirtyEntry,
            11 => LookupProgress::VerifyDirtyFrame,
            12 => LookupProgress::CleanFrame,
            13 => LookupProgress::VerifyDirtyEntry,
            14 => LookupProgress::CompilationBookkeeping,
            15 => LookupProgress::PageWalk,
            16 => LookupProgress::SeeJit,
            _ => unreachable!(),
        }
    }
}

#[bitsize(4)]
#[derive(Copy, Clone, DebugBits)]
pub struct ContextFlags {
    entry_env: EntryEnv,

    is_userspace: bool,
}

impl ContextFlags {
    #[inline(always)]
    pub fn build(protected_mode: bool, is_userspace: bool, segment_sizes: SegmentSizes) -> Self {
        Self::new(EntryEnv::new(segment_sizes, protected_mode), is_userspace)
    }
}

pub struct InstructionCache<'tag> {
    inner: InnerCache<'tag>,
    mapping: MappingTracker,
    link_cache_misses: u64,
    num_cache_consistency_checks: u64,
}

/// Trait that is used to look up IP, CS base and memory if needed.
/// By implementing this as a trait, these fields do not need to be read before the function is invoked.
/// In the fast path, the function does not need these values and thus some memory reads can be avoided.
pub trait CurrentState {
    fn cs_base(&self) -> u32;
    fn ip(&self) -> u32;
    fn memory(&self) -> &Mem32;

    #[inline(always)]
    fn pc(&self) -> LinAddr {
        LinAddr::new(self.cs_base().wrapping_add(self.ip()))
    }
}

impl CurrentState for (u32, u32, &Mem32) {
    fn cs_base(&self) -> u32 {
        self.0
    }

    fn ip(&self) -> u32 {
        self.1
    }

    fn memory(&self) -> &Mem32 {
        self.2
    }
}

// TODO: reintroduce backend configuration, maybe as a Box<dyn> so we don't need to add generics everywhere?
type B = InkwellBackend<'static>;

impl<'tag, 'sem> InstructionCache<'tag> {
    pub fn new(
        guard: Guard<'tag>, semantics: Arc<PackedInstrSem>, backend: SingleEncodingExecution<InkwellBackend<'static>>,
    ) -> Self {
        Self {
            inner: InnerCache::new(guard, backend, semantics),
            link_cache_misses: 0,
            mapping: MappingTracker::new(),
            num_cache_consistency_checks: 0,
        }
    }

    pub fn set_page_jit_enabled(&mut self, enable: bool) {
        self.inner.set_pagejit_enabled(enable);
    }

    pub fn set_semantics(&mut self, semantics: Arc<PackedInstrSem>, backend: SingleEncodingExecution<InkwellBackend<'static>>) {
        self.inner.decoder = Decoder::new(semantics.clone());
        self.inner.page_compiler = PageJit::new(semantics);
        self.inner.backend = backend;
        self.clear();
    }

    /// Finds or creates the CacheEntry for `pc`.
    ///
    /// This function is guaranteed to return the correct CacheEntry.
    /// That is, it returns the entry that is supposed to belong to the physical address of `pc`.
    /// However, this function doesn't reread the memory contents if the frame is dirty.
    ///
    /// If the frame is dirty and the memory contents have changed, you must update the entry.
    /// You should not create a new entry.
    fn find_or_create_entry(
        &mut self, pc: LinAddr, flags: ContextFlags, memory: &Mem32, mut progress: impl FnMut(LookupProgress),
    ) -> Result<CacheEntryId<'tag>, Exception> {
        progress(LookupProgress::Lookup);

        let entry = if let Some(entry) = self.inner.tlb.lookup(pc, self.inner.page_versioner.current_version()) {
            debug_assert_eq!(entry, {
                let phys_addr = self
                    .mapping
                    .resolve_phys_frame_index(&mut self.inner, pc, flags.is_userspace(), memory)
                    .unwrap();
                self.inner.find_entry_by_phys_addr(phys_addr).unwrap()
            });

            entry
        } else {
            let phys_addr = self
                .mapping
                .resolve_phys_frame_index(&mut self.inner, pc, flags.is_userspace(), memory)?;
            let id = match self.inner.find_entry_by_phys_addr(phys_addr) {
                Some(id) => id,
                None => self.decode_new_entry(pc, flags, memory, &mut progress, phys_addr)?,
            };

            self.inner.tlb.insert(pc, self.inner.page_versioner.current_version(), id);

            id
        };

        // Recreate entry if environment doesn't match
        if self.inner.entries[entry].env() != flags.entry_env() {
            // We must destroy any links to this entry, since its environment has changed.
            self.inner.remove_links_to(entry);

            let phys_addr = self.inner.entries[entry].phys_addr();
            self.inner.entries[entry] = self.decode_instr_at(pc, phys_addr, flags, memory, &mut progress)?;
            self.inner
                .update_instr_len(&self.mapping, phys_addr, self.inner.entries[entry].instr_len(), memory);
            self.inner
                .notify_existing_instr_changed(&self.mapping, phys_addr.into(), memory);

            trace!("Updated instruction at physical address {phys_addr} ({entry:?}) after environment changed");
        }

        Ok(entry)
    }

    #[inline(never)]
    fn decode_new_entry(
        &mut self, pc: LinAddr, flags: ContextFlags, memory: &Mem32, progress: &mut impl FnMut(LookupProgress),
        phys_addr: PhysAddr,
    ) -> Result<CacheEntryId<'tag>, Exception> {
        let entry = self.decode_instr_at(pc, phys_addr, flags, memory, progress)?;
        let id = self.inner.entries.create_new(entry);
        trace!("Decoded instruction at physical address {phys_addr} as {id:?}");
        self.inner.insert_entry(&self.mapping, id, memory);
        Ok(id)
    }

    /// Checks if the physical frame is up-to-date with the most recent page version.
    /// If it is not, rewalks pages for all linear addresses known to be associated with this frame.
    /// Any addresses where the page walk returns a different physical frame are removed.
    /// If any addresses are removed, links that may no longer be valid are pruned.
    ///
    /// The happy path of this function consists of a single integer comparison.
    #[inline(always)]
    fn page_mapping_mismatch(
        &mut self, phys_frame_index: PhysFrameIndex, expected_page: LinPageIndex, flags: ContextFlags, memory: &Mem32,
        mut progress: impl FnMut(LookupProgress),
    ) -> bool {
        if !self.mapping.page_mapping_is_current(phys_frame_index) {
            progress(LookupProgress::RewalkPages);
            trace!("Rewalking pages for {phys_frame_index} to make sure mappings didn't change");
            self.mapping
                .rewalk_pages(&mut self.inner, phys_frame_index, flags.is_userspace(), memory);

            debug_assert!(!self.inner.phys_cache[phys_frame_index].checks_needed.needs_mapping_check());

            trace!(
                "Mappings for {phys_frame_index} contain {expected_page}: {}",
                self.mapping.frame_is_mapped_as(phys_frame_index, expected_page)
            );

            return !self.mapping.frame_is_mapped_as(phys_frame_index, expected_page)
        } else {
            debug_assert!(
                self.mapping.frame_is_mapped_as(phys_frame_index, expected_page),
                "page mapping of {expected_page} (phys: {phys_frame_index}) is current, but mapping change was not propagated"
            );

            debug_assert!(!self.inner.phys_cache[phys_frame_index].checks_needed.needs_mapping_check());

            false
        }
    }

    #[inline(always)]
    pub fn executable_entry(&self, id: CacheEntryId<'tag>) -> Executable<'_, 'tag, <B as Backend>::UninstantiatedFn> {
        self.inner.entries[id].as_executable()
    }

    #[inline(always)]
    pub fn encoding_info(&self, id: CacheEntryId<'tag>) -> EncodingInfo {
        self.inner.entries[id].encoding_info()
    }

    pub fn resolve_instr_for(&self, id: CacheEntryId<'tag>, page: LinPageIndex, mem: &Mem32) -> Instruction {
        self.inner.entries[id].resolve_instr(&self.inner, page, mem, &self.mapping)
    }

    pub fn entry_phys_addr(&self, id: CacheEntryId<'tag>) -> PhysAddr {
        self.inner.entries[id].phys_addr()
    }

    pub fn lookup_first(
        &mut self, cs_base: u32, ip: u32, flags: ContextFlags, memory: &Mem32, mut progress: impl FnMut(LookupProgress),
        is_entry: bool,
    ) -> Result<CacheEntryId<'tag>, Exception> {
        progress(LookupProgress::Lookup);
        let pc = LinAddr::new(cs_base.wrapping_add(ip));
        let entry = self.find_or_create_entry(pc, flags, memory, &mut progress)?;

        // Here, we can assume the physical address is correct and we do not need to check for page mapping updates.
        // However, we do need to make sure the entry environment is the same as the current environment.
        // Since we do not know the environment of the previous instruction, we cannot assume it has stayed the same since.
        let recreate_entry = self.entry_memory_has_changed(entry, pc.into(), flags, memory, &mut progress);
        if recreate_entry {
            let entry = self.find_or_create_entry(pc, flags, memory, &mut progress)?;
            let phys_addr = self.inner.entries[entry].phys_addr();
            self.inner.entries[entry] = self.decode_instr_at(pc, phys_addr, flags, memory, &mut progress)?;
            self.inner
                .update_instr_len(&self.mapping, phys_addr, self.inner.entries[entry].instr_len(), memory);
            self.inner
                .notify_existing_instr_changed(&self.mapping, phys_addr.into(), memory);

            trace!("Updated instruction at physical address {phys_addr} ({entry:?}) after memory change");
        }

        if is_entry {
            let f = self.inner.entries[entry].flags_mut();
            f.set_entry_kind(f.entry_kind().combine(EntryPoint::Global));
        }

        progress(LookupProgress::Unused1);
        Ok(entry)
    }

    #[inline(never)]
    pub fn decode_instr_at(
        &mut self, pc: LinAddr, phys_addr: PhysAddr, flags: ContextFlags, memory: &Mem32,
        mut progress: impl FnMut(LookupProgress),
    ) -> Result<CacheEntry<'tag, <B as Backend>::UninstantiatedFn>, Exception> {
        progress(LookupProgress::DecodeInstr);
        let mut n = 0;
        let mut buf = 0u64;
        let mut num_in_buf = 0;
        let (result, instr) = self.inner.decoder.lookup_iteratively(
            || {
                if num_in_buf > 0 {
                    n += 1;
                    num_in_buf -= 1;
                    let result = buf as u8;
                    buf >>= 8;

                    Result::<u8, Exception>::Ok(result)
                } else {
                    let addr = pc + n as u32;
                    n += 1;
                    // TODO: Use an MMIO that will panick if it is accessed
                    if addr.page_offset() <= 0xff8 {
                        buf = memory.read_u64(addr.as_u32(), flags.is_userspace(), &mut ())?;
                        num_in_buf = 7;

                        let result = buf as u8;
                        buf >>= 8;
                        Ok(result)
                    } else {
                        let result = memory.read::<u8>(addr.as_u32(), flags.is_userspace(), &mut ())?;
                        Ok(result)
                    }
                }
            },
            flags.entry_env().segment_sizes(),
        );

        match (result?, instr) {
            (Some((encoding_index, encoding)), instr) => {
                trace!("Decoded {instr:X} @ {phys_addr}");
                let predictable_jump = encoding.semantics.jump.is_fixed_relative();
                let base_instr = encoding.try_extract_base_instr(instr).unwrap();
                let part_values = encoding.extract_parts(&base_instr);
                let part_values = encoding.semantics.part_packing.pack(&part_values);

                let execute = self.inner.backend.get_or_build(
                    encoding_index,
                    encoding,
                    flags.entry_env().effective_protected_mode(),
                    flags.entry_env().segment_sizes(),
                    || progress(LookupProgress::SeeJit),
                );

                let crosses_page_bounds = phys_addr.frame_offset() + instr.byte_len() as u16 > 0x1000;

                self.inner.update_cached_instr_bytes(phys_addr, instr.bytes(), memory);
                if crosses_page_bounds {
                    let second_lin_addr = pc + (instr.byte_len() as u32 - 1);
                    let second_phys_addr =
                        self.mapping
                            .resolve_phys_frame_index(&mut self.inner, second_lin_addr, flags.is_userspace(), memory)?;

                    let bytes_on_second_page = &instr.bytes()[4096 - phys_addr.frame_offset() as usize..];
                    self.inner.update_cached_instr_bytes(
                        PhysFrameIndex::from(second_phys_addr).start_address(),
                        bytes_on_second_page,
                        memory,
                    );
                }

                Ok(CacheEntry::new(
                    phys_addr,
                    flags.entry_env(),
                    execute,
                    EncodingInfo {
                        encoding_index,
                        part_values,
                        instr_len: instr.byte_len() as u8,
                    },
                    predictable_jump,
                ))
            },

            (None, instr) => {
                error!("Missing encoding (flags={flags:?}) for: {instr:X} at {pc}");
                Err(Exception::InvalidOpcode)
            },
        }
    }

    #[inline(always)]
    pub fn lookup_next_from_entry(
        &mut self, previous: CacheEntryId<'tag>, state: impl CurrentState, flags: ContextFlags,
        mut progress: impl FnMut(LookupProgress), is_entry: bool, jump_taken: bool,
    ) -> Result<CacheEntryId<'tag>, Exception> {
        progress(LookupProgress::ScanNextBlocks);
        let mut next_entry = match self.inner.entries[previous].links() {
            Links::Unchecked(links) => {
                let next_entry = links[jump_taken as usize];
                if let Some(next_entry) = next_entry {
                    return Ok(next_entry)
                } else {
                    // We cannot return early here, because the `make_link` below might alter the Links.
                    let pc = state.pc();
                    progress(LookupProgress::CacheMiss);
                    self.link_cache_misses += 1;

                    let next_entry = self.find_or_create_entry(pc, flags, state.memory(), &mut progress)?;
                    if self.inner.entries[previous].env() == flags.entry_env() {
                        self.make_link(previous, pc, next_entry, is_entry, jump_taken);
                    }

                    next_entry
                }
            },
            Links::Checked(links) => {
                let next_entry = links[jump_taken as usize];
                if let Some(next_entry) = next_entry {
                    next_entry
                } else {
                    let pc = state.pc();
                    progress(LookupProgress::CacheMiss);
                    self.link_cache_misses += 1;

                    // println!("Cache Miss after {:X}: links are empty", self.inner.entries[previous].resolve_instr(&self.phys, memory));
                    let next_entry = self.find_or_create_entry(pc, flags, state.memory(), &mut progress)?;
                    if self.inner.entries[previous].env() == flags.entry_env() {
                        self.make_link(previous, pc, next_entry, is_entry, jump_taken);
                    }

                    next_entry
                }
            },
            Links::Speculative(_) => {
                let pc = state.pc();
                // println!("Cache Miss after {:X}: links are empty", self.inner.entries[previous].resolve_instr(&self.phys, memory));
                if let Some(next_entry) = self.inner.tlb.lookup(pc, self.inner.page_versioner.current_version()) {
                    next_entry
                } else {
                    progress(LookupProgress::CacheMiss);
                    self.link_cache_misses += 1;
                    let next_entry = self.find_or_create_entry(pc, flags, state.memory(), &mut progress)?;
                    if self.inner.entries[previous].env() == flags.entry_env() {
                        self.make_link(previous, pc, next_entry, is_entry, jump_taken);
                    }

                    next_entry
                }
            },
        };

        debug_assert!(
            !self.inner.entries.is_released(next_entry),
            "entry {previous:?} contains link to {next_entry:?}, which no longer exists"
        );

        let phys_frame_index = self.inner.entries[next_entry].phys_frame_index();
        debug_assert_eq!(
            self.inner.phys_cache[phys_frame_index].checks_needed.dirty(),
            self.inner.phys_cache[phys_frame_index].cleaning_delay.is_some()
        );
        if self.inner.frame_any_checks_needed(phys_frame_index) {
            next_entry = self.perform_cache_frame_checks(
                previous,
                state.cs_base(),
                state.ip(),
                flags,
                state.memory(),
                &mut progress,
                next_entry,
                phys_frame_index,
                jump_taken,
            )?;
        } else {
            debug_assert!(
                self.inner.phys_cache[self.inner.entries[next_entry].phys_frame_index()]
                    .cleaning_delay
                    .is_none()
            );
            debug_assert!(
                self.mapping
                    .frame_is_mapped_as(self.inner.entries[next_entry].phys_frame_index(), state.pc().into()),
                "jump from {} to {} is incorrect: {}\n\nPhys frame index {} is not mapped as {}",
                self.inner.entries[previous].phys_addr(),
                self.inner.entries[next_entry].phys_addr(),
                self.inner.entries[previous]
                    .encoding_info()
                    .display_instance(self.inner.decoder.encodings()),
                self.inner.entries[next_entry].phys_frame_index(),
                state.pc()
            );
        }

        Ok(next_entry)
    }

    #[inline(never)]
    fn perform_cache_frame_checks(
        &mut self, previous: CacheEntryId<'tag>, cs_base: u32, ip: u32, flags: ContextFlags, memory: &Mem32,
        mut progress: impl FnMut(LookupProgress), mut next_entry: CacheEntryId<'tag>, phys_frame_index: PhysFrameIndex,
        jump_taken: bool,
    ) -> Result<CacheEntryId<'tag>, Exception> {
        self.num_cache_consistency_checks += 1;
        let pc = LinAddr::new(cs_base.wrapping_add(ip));
        progress(LookupProgress::VerifyPageMapping);
        let checking_flags = self.inner.frame_checking_flags(phys_frame_index);
        trace!("Checking frame {phys_frame_index} for {checking_flags:?}");
        if checking_flags.needs_only_successor_page_mapping_check() {
            // Force a check of the next page so the flag will be cleared
            for successor in self.mapping.lin_successors(phys_frame_index).collect::<ArrayVec<_, 8>>() {
                // The expected page only affects return value, which we don't use.
                let zero_page = LinPageIndex::from(LinAddr::new(0));
                self.page_mapping_mismatch(successor, zero_page, flags, memory, &mut progress);
            }

            return Ok(next_entry)
        }

        if checking_flags.needs_mapping_check()
            && self.page_mapping_mismatch(phys_frame_index, pc.into(), flags, memory, &mut progress)
        {
            trace!("Page mapping mismatch at {pc}, recreating entry");
            // Any links to this page have now been removed.
            // We can add a new link after finding the correct next entry.
            next_entry = self.find_or_create_entry(pc, flags, memory, &mut progress)?;

            if self.inner.entries[previous].env() == flags.entry_env() {
                self.make_link(previous, pc, next_entry, false, jump_taken);
            }
        }

        progress(LookupProgress::VerifyDirtyFrame);
        if checking_flags.needs_dirty_check()
            && self.entry_memory_has_changed(next_entry, pc.into(), flags, memory, &mut progress)
        {
            // TODO: If this returns a newly created entry, we do not need to, again, recreate it right below.
            // TODO: If `entry_memory_has_changed` has not released the current entry, this is unnecessary.
            next_entry = self.find_or_create_entry(pc, flags, memory, &mut progress)?;

            let phys_addr = self.inner.entries[next_entry].phys_addr();
            self.inner.entries[next_entry] = self.decode_instr_at(pc, phys_addr, flags, memory, &mut progress)?;
            self.inner
                .update_instr_len(&self.mapping, phys_addr, self.inner.entries[next_entry].instr_len(), memory);
            self.inner
                .notify_existing_instr_changed(&self.mapping, phys_frame_index, memory);

            trace!("Updated instruction at physical address {phys_addr} ({next_entry:?}) after memory change");
        }

        if checking_flags.needs_counters_check() {
            self.page_jit_if_needed(next_entry);
        }

        Ok(next_entry)
    }

    #[inline(always)]
    fn make_link(
        &mut self, previous: CacheEntryId<'tag>, pc: LinAddr, next_entry: CacheEntryId<'tag>, is_global_entry: bool,
        jump_taken: bool,
    ) {
        let phys_frame_index = self.inner.entries[next_entry].phys_frame_index();
        let prev_phys_frame_index = self.inner.entries[previous].phys_frame_index();
        let same_frame = prev_phys_frame_index == phys_frame_index;
        let jump = self
            .inner
            .decoder
            .encodings()
            .get(self.inner.entries[previous].encoding_info().encoding_index)
            .semantics
            .jump;
        let is_fixed_relative = jump.is_fixed_relative();
        self.inner.add_link(previous, next_entry, pc, jump_taken);

        if is_global_entry || !same_frame || !is_fixed_relative {
            // TODO: If the same physical frame is mapped at multiple locations, entry point could be external even if it is on the same frame
            let f = self.inner.entries[next_entry].flags_mut();
            let entry_kind = f.entry_kind();
            let new_entry_kind = entry_kind.combine(if is_global_entry {
                EntryPoint::Global
            } else if !same_frame {
                EntryPoint::External
            } else {
                EntryPoint::Local
            });

            f.set_entry_kind(new_entry_kind);

            if new_entry_kind != entry_kind {
                let has_received_jit = self.inner.phys_cache[phys_frame_index].has_received_jit;
                let has_requested_jit = self.inner.phys_cache[phys_frame_index].has_requested_jit;
                let jit_page_in = self.inner.phys_cache[phys_frame_index].jit_page_in;
                trace!(
                    "Added entry point in {phys_frame_index} at lin addr {pc}: {new_entry_kind:?}, has_received_jit={has_received_jit}"
                );

                if entry_kind == EntryPoint::None && (has_received_jit || has_requested_jit) && jit_page_in != 0 {
                    log::debug!(
                        "Requesting new JIT because an entry {new_entry_kind:?} was added in {phys_frame_index} at lin addr {pc}"
                    );
                    self.inner.mark_frame_as_needing_jit(phys_frame_index);
                }
            }
        }
    }

    pub fn notify_all_page_mappings_updated(&mut self, _memory: &Mem32) {
        self.mapping.mark_all_mappings_as_stale(&mut self.inner);
        self.inner.notify_all_page_mappings_updated();
    }

    pub fn notify_page_mapping_updated(&mut self, page: LinPageIndex, memory: &Mem32) {
        trace!("Page mapping for {page:?} was invalidated");
        self.mapping
            .resolve_phys_frame_index(&mut self.inner, page.start_addr(), false, memory)
            .ok();
    }

    pub fn notify_memory_dirty(&mut self, phys_frame_index: PhysFrameIndex, memory: &Mem32) {
        self.inner.notify_memory_dirty(&self.mapping, phys_frame_index, memory);
    }

    pub fn advise_mark_dirty(&mut self, addr: PhysAddr, len: u8) -> sem86_arch::mem::MarkDirtyAdvice {
        self.inner.advise_mark_dirty(addr, len)
    }

    pub fn most_executed_blocks(&self) -> impl Iterator<Item = (u64, BlockInfo<'_>)> {
        [].into_iter()
        // self.inner.entries.iter()
        //     .flat_map(|e| e.block.as_ref().map(|b| (b, e.env.segment_sizes())))
        //     .map(|(b, segment_sizes)| (b.num_executed.load(Ordering::Relaxed), BlockInfo {
        //         entry_point: b.entry_point,
        //         instrs: &b.instrs[..],
        //         protected_mode_accesses: b.env.effective_protected_mode(),
        //         segment_sizes,
        //     }))
        //     .sorted_by_key(|(n, _)| Reverse(*n))
    }

    pub fn get_cache_block_instrs(&self, _cache_block_id: ()) -> Vec<(u32, Instruction)> {
        todo!()
    }

    pub fn num_see_jits(&self) -> usize {
        self.inner.backend.num_compiled()
    }

    pub fn num_pages_jitted(&self) -> u64 {
        0
    }

    pub fn entry_memory_usage(&self) -> u64 {
        self.inner.entries.len() as u64 * size_of::<CacheEntry<'tag, <B as Backend>::UninstantiatedFn>>() as u64
    }

    pub fn decoder(&mut self) -> &mut Decoder<PackedInstrSem> {
        &mut self.inner.decoder
    }

    #[inline(always)]
    fn page_jit_if_needed(&mut self, id: CacheEntryId<'tag>) {
        let entry = &mut self.inner.entries[id];
        let phys_frame_index = entry.phys_frame_index();

        if self.inner.should_jit_page(phys_frame_index) && self.inner.page_jit_enabled {
            if self.inner.page_compiler.num_pending_requests() > 30 {
                self.inner.phys_cache[phys_frame_index].jit_page_in = u16::MAX;
            } else {
                log::debug!("Requesting compilations for chains on {phys_frame_index}");
                self.inner.request_page_jit(phys_frame_index, &self.mapping);
            }
        }
    }

    pub fn receive_compiled_chains(&mut self) {
        self.inner.receive_compiled_chains();
    }

    /// Should be called regularly to allow the cache to cleanup stale data.
    /// Is guaranteed to run for O(1) time.
    pub fn periodic_work(&mut self) {
        self.inner.prune_old_dirty_frames();
    }

    pub fn greedily_request_pagejit(&mut self) {
        self.inner.process_pagejit_request(&self.mapping);
    }

    pub fn num_dirty_frame_checks(&self) -> usize {
        self.inner.num_dirty_frame_checks()
    }

    pub fn code_frames_dirty(&self) -> usize {
        self.inner.code_frames_dirty()
    }

    pub fn total_frames_dirty(&self) -> usize {
        self.inner.total_frames_dirty()
    }

    pub fn total_frames_clean(&self) -> usize {
        self.inner.total_frames_clean()
    }

    pub fn debug_snapshot(&self, phys_mem: &Shm) -> CacheSnapshot {
        CacheSnapshot {
            phys_memory: phys_mem.view().to_vec(),
            phys_frames: self.inner.debug_snapshot(),
            entries: self.inner.entries.iter().map(|e| e.debug_snapshot()).collect::<Vec<_>>(),
        }
    }

    /// Return an `impl Display` that avoids making the `CacheSnapshot` until `Display::fmt` is invoked.
    /// This allows you to place it inside a logging statement without any performance impact.
    pub fn display_debug_snapshot<'r>(&'r self, phys_mem: &'r Shm) -> impl Display + 'r {
        DisplayDebugSnapshot {
            cache: self,
            phys_mem,
        }
    }

    pub fn has_chains(&self, id: CacheEntryId<'tag>) -> bool {
        self.inner.frame_has_chains(self.inner.entries[id].phys_frame_index())
    }

    pub fn num_link_cache_misses(&self) -> u64 {
        self.link_cache_misses
    }

    pub fn seejit_memory_usage(&self) -> usize {
        self.inner.backend.memory_usage()
    }

    pub fn page_jit_memory_usage(&self) -> usize {
        self.inner.page_alloc.memory_usage()
    }

    pub fn num_instrs_crossing_page_bounds(&self) -> usize {
        self.inner.entries.iter().filter(|e| e.crosses_page_bounds()).count()
    }

    /// Checks if the frame is dirty and the entry no longer corresponds to the actual memory contents.
    /// If so, the entry should be recreated from the memory contents.
    ///
    /// The happy path of this function consists of two comparisons.
    /// The first comparison checks if the instruction crosses page bounds.
    /// The second comparison checks if there is currently a cleaning delay active.
    /// If neither of those are the case, the function returns false immediately.
    #[inline(always)]
    pub fn entry_memory_has_changed(
        &mut self, entry_id: CacheEntryId<'tag>, page: LinPageIndex, flags: ContextFlags, memory: &Mem32,
        mut progress: impl FnMut(LookupProgress),
    ) -> bool {
        let phys_frame = self.inner.entries[entry_id].phys_frame_index();
        if self.inner.entries[entry_id].crosses_page_bounds() {
            let second_frame_index = if let Some(index) = self.mapping.try_lookup_cached_phys_addr(page + 1) {
                index
            } else {
                return true
            };

            // We might need to rewalk pages of the second frame.
            // We cannot do this `fn page_mapping_mismatch`, as it is not called in `lookup_first`.
            // Even though we walk pages to determine the physical address in `lookup_first`,
            // we do not check the second page if the instruction crosses page bounds.
            // When we return true here, this forces a fetch and decode on the instruction.
            // This will either update the instruction to be correct, or throw
            // an exception if the second page has been unmapped.
            if !self.mapping.page_mapping_is_current(second_frame_index) {
                trace!("Rewalking pages for second frame {second_frame_index}");
                self.mapping
                    .rewalk_pages(&mut self.inner, second_frame_index, flags.is_userspace(), memory);

                if !self.mapping.frame_is_mapped_as(second_frame_index, page + 1) {
                    return true
                }
            } else {
                // debug_assert_eq!(
                //     Self::resolve_phys_frame_index((page + 1).start_addr(), false, memory).unwrap(),
                //     second_frame_index.start_address(),
                //     "`second_frame_index` should correctly reflect the physical frame of the second page"
                // );
            }

            let entry = &self.inner.entries[entry_id];
            if self.inner.phys_cache[second_frame_index].cleaning_delay.is_some() {
                self.inner.num_dirty_frame_checks += 1;

                let second_frame = &self.inner.phys_cache[second_frame_index];
                if !second_frame.extra.entry_matches_memory(second_frame_index, entry, memory) {
                    return true
                }
            }
        }

        let entry = &self.inner.entries[entry_id];
        let frame = &mut self.inner.phys_cache[phys_frame];
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

        if frame.cleaning_delay.is_some() {
            self.inner.check_dirty_memory(&self.mapping, entry_id, memory, &mut progress)
        } else {
            false
        }
    }

    pub fn num_cache_consistency_checks(&self) -> u64 {
        self.num_cache_consistency_checks
    }

    pub fn all_code_pages_jitted(&self) -> bool {
        self.inner.all_code_pages_jitted()
    }

    /// Clears the entire cache.
    /// You must ensure that all memory pages are marked as clean, otherwise cache inconsistencies may occur.
    pub fn clear(&mut self) {
        self.mapping = MappingTracker::new();
        self.inner.clear();
    }

    pub fn frame_is_jitted(&self, id: CacheEntryId<'tag>) -> bool {
        let phys_frame_index = self.inner.entries[id].phys_frame_index();
        self.inner.phys_cache[phys_frame_index].has_received_jit && !self.inner.jits_pending.contains(&phys_frame_index)
    }
}

struct DisplayDebugSnapshot<'r, 'tag> {
    cache: &'r InstructionCache<'tag>,
    phys_mem: &'r Shm,
}

impl Display for DisplayDebugSnapshot<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.cache.debug_snapshot(self.phys_mem).fmt(f)
    }
}

pub struct BlockInfo<'a> {
    pub entry_point: u32,
    pub instrs: &'a [(u32, InstructionEntry)],
    pub segment_sizes: SegmentSizes,
    pub protected_mode_accesses: bool,
}

// TODO: Test happy path
// TODO: Test proper reload when dirty
// TODO: Test proper reload when mapping changes
// TODO: Test proper error when second page of instruction that crosses page bounds is unmapped
#[cfg(test)]
mod tests {
    use std::io::{BufReader, Cursor};
    use std::pin::Pin;
    use std::sync::mpsc::channel;
    use std::sync::{Arc, LazyLock};

    use generativity::make_guard;
    use lz4_flex::frame::FrameDecoder;
    use sem86_arch::addr::{LinAddr, PhysAddr};
    use sem86_arch::mem::{Mem32, Shm};
    use test_log::test;

    use crate::SegmentSizes;
    use crate::codegen::backends::inkwell::{InkwellBackend, InkwellContext};
    use crate::codegen::see::SingleEncodingExecution;
    use crate::decoder::{EncodingLookup, PackedInstrSem};
    use crate::emulator::exec::ExecutionContext;
    use crate::hw::Hw;
    use crate::hw::intr::Intr;
    use crate::icache::{ContextFlags, InstructionCache};
    use crate::time::EmulatorClock;

    static SEMANTICS: LazyLock<PackedInstrSem> = LazyLock::new(|| {
        let f = BufReader::new(Cursor::new(include_bytes!("../../../x86.semantics")));
        let f = FrameDecoder::new(f);
        let instr_semantics: PackedInstrSem = pot::from_reader(f).unwrap();
        instr_semantics
    });

    // ICPB: instruction crossing page boudns

    #[test]
    fn icpb_invalidated_when_second_page_invlpg_only() {
        let instr_semantics = &SEMANTICS;
        make_guard!(guard);
        let cache = InstructionCache::new(
            guard,
            Arc::new((**instr_semantics).clone()),
            SingleEncodingExecution::new(InkwellBackend::new(InkwellContext::leak_new()), instr_semantics.len()),
        );
        let shm = Arc::new(Shm::new("test", 1 << 20));
        let mem = Arc::new(Mem32::new(shm.clone()));
        mem.enable_paging(true);
        let intr = Intr::new();
        let intr = Pin::new(&intr);
        let hw = Hw::new(
            mem.clone(),
            Vec::new(),
            channel().0,
            channel().1,
            Arc::new(Shm::new("vgabios", 16)),
            Intr::handle(intr),
            EmulatorClock::new_asynchronous(),
        );
        let mut ctx = ExecutionContext::new(hw, &mem, None, cache);
        ctx.protected_mode = true;
        // Clear page tables
        mem.write_physical_slice(0, &[0; 4096 * 2], &mut ctx.mmio_ctx).unwrap();

        // Write PDE for
        const PDE_OFFSET: u32 = (0x7c814000u32 >> 22) * 4;
        mem.write_physical_slice(PDE_OFFSET, &0x00001007u32.to_le_bytes(), &mut ctx.mmio_ctx)
            .unwrap();

        const PTE_OFFSET1: u32 = 0x1000 + ((0x7c814000u32 >> 12) & 0x3ff) * 4;
        const PTE_OFFSET2: u32 = 0x1000 + ((0x7c815000u32 >> 12) & 0x3ff) * 4;

        mem.write_physical_slice(PTE_OFFSET1, &0x00003007u32.to_le_bytes(), &mut ctx.mmio_ctx)
            .unwrap();
        mem.write_physical_slice(PTE_OFFSET2, &0x00004007u32.to_le_bytes(), &mut ctx.mmio_ctx)
            .unwrap();

        // Place instruction crossing page bounds
        mem.write_slice(0x7c814fff, &[0x8b, 0x45, 0x18], false, &mut ctx.mmio_ctx)
            .unwrap();

        // Load entry into cache
        let entry1 = ctx.mmio_ctx.icache.lookup_first(
            0,
            0x7c814fff,
            ContextFlags::build(true, false, SegmentSizes::Cs32Ss32),
            &mem,
            |_| (),
            false,
        );
        assert!(entry1.is_ok(), "Entry creation at 0x7C814000 should succeed");

        // Unmap 0x7C815000 by zeroing its PTE
        mem.write_physical_slice(PTE_OFFSET2, &0u32.to_le_bytes(), &mut ctx.mmio_ctx)
            .unwrap();

        // Invalidate the second page
        ctx.mmio_ctx
            .icache
            .notify_page_mapping_updated(LinAddr::new(0x7C815000u32).into(), &mem);
        mem.invalidate_page(0x7C815000u32);

        // Which should now cause this to fail
        let entry2 = ctx.mmio_ctx.icache.lookup_first(
            0,
            0x7c814fff,
            ContextFlags::build(true, false, SegmentSizes::Cs32Ss32),
            &mem,
            |_| (),
            false,
        );
        assert!(
            entry2.is_err(),
            "Entry lookup at 0x7C815000 should fail after unmapping, but returned: {entry2:?}"
        );
    }

    #[test]
    fn icbp_invalidated_when_second_page_invlpg_and_first_page_cleaned() {
        let instr_semantics = &SEMANTICS;
        make_guard!(guard);
        let cache = InstructionCache::new(
            guard,
            Arc::new((**instr_semantics).clone()),
            SingleEncodingExecution::new(InkwellBackend::new(InkwellContext::leak_new()), instr_semantics.len()),
        );
        let shm = Arc::new(Shm::new("test", 1 << 20));
        let mem = Arc::new(Mem32::new(shm.clone()));
        mem.enable_paging(true);
        let intr = Intr::new();
        let intr = Pin::new(&intr);
        let hw = Hw::new(
            mem.clone(),
            Vec::new(),
            channel().0,
            channel().1,
            Arc::new(Shm::new("vgabios", 16)),
            Intr::handle(intr),
            EmulatorClock::new_asynchronous(),
        );
        let mut ctx = ExecutionContext::new(hw, &mem, None, cache);
        ctx.protected_mode = true;
        // Clear page tables
        mem.write_physical_slice(0, &[0; 4096 * 2], &mut ctx.mmio_ctx).unwrap();

        // Write PDE for
        const PDE_OFFSET: u32 = (0x7c814000u32 >> 22) * 4;
        mem.write_physical_slice(PDE_OFFSET, &0x00001007u32.to_le_bytes(), &mut ctx.mmio_ctx)
            .unwrap();

        const PTE_OFFSET1: u32 = 0x1000 + ((0x7c814000u32 >> 12) & 0x3ff) * 4;
        const PTE_OFFSET2: u32 = 0x1000 + ((0x7c815000u32 >> 12) & 0x3ff) * 4;

        mem.write_physical_slice(PTE_OFFSET1, &0x00003007u32.to_le_bytes(), &mut ctx.mmio_ctx)
            .unwrap();
        mem.write_physical_slice(PTE_OFFSET2, &0x00004007u32.to_le_bytes(), &mut ctx.mmio_ctx)
            .unwrap();

        // Place instruction crossing page bounds
        mem.write_slice(0x7c814fff, &[0x8b, 0x45, 0x18], false, &mut ctx.mmio_ctx)
            .unwrap();

        // Load entry into cache
        let entry1 = ctx.mmio_ctx.icache.lookup_first(
            0,
            0x7c814fff,
            ContextFlags::build(true, false, SegmentSizes::Cs32Ss32),
            &mem,
            |_| (),
            false,
        );
        assert!(entry1.is_ok(), "Entry creation at 0x7C814000 should succeed");

        // Unmap 0x7C815000 by zeroing its PTE
        mem.write_physical_slice(PTE_OFFSET2, &0u32.to_le_bytes(), &mut ctx.mmio_ctx)
            .unwrap();

        // Invalidate the second page
        ctx.mmio_ctx
            .icache
            .notify_page_mapping_updated(LinAddr::new(0x7C815000u32).into(), &mem);
        mem.invalidate_page(0x7C815000u32);

        // Mark first page as dirty
        ctx.mmio_ctx.icache.notify_memory_dirty(PhysAddr::new(0x3000).into(), &mem);

        // Wait for the first page to be cleaned
        for _ in 0..25_000 {
            let entry = ctx.mmio_ctx.icache.lookup_first(
                0,
                0x7c814000,
                ContextFlags::build(true, false, SegmentSizes::Cs32Ss32),
                &mem,
                |_| (),
                false,
            );
            assert!(entry.is_ok());
        }

        // Which should now cause this to fail
        let entry2 = ctx.mmio_ctx.icache.lookup_first(
            0,
            0x7c814fff,
            ContextFlags::build(true, false, SegmentSizes::Cs32Ss32),
            &mem,
            |_| (),
            false,
        );
        assert!(
            entry2.is_err(),
            "Entry lookup at 0x7C815000 should fail after unmapping, but returned: {entry2:?}"
        );
    }

    #[test]
    fn icpb_entry_must_be_updated_even_if_no_longer_mapped_dirty() {
        let instr_semantics = &SEMANTICS;
        make_guard!(guard);
        let cache = InstructionCache::new(
            guard,
            Arc::new((**instr_semantics).clone()),
            SingleEncodingExecution::new(InkwellBackend::new(InkwellContext::leak_new()), instr_semantics.len()),
        );
        let shm = Arc::new(Shm::new("test", 1 << 20));
        let mem = Arc::new(Mem32::new(shm.clone()));
        mem.enable_paging(true);
        let intr = Intr::new();
        let intr = Pin::new(&intr);
        let hw = Hw::new(
            mem.clone(),
            Vec::new(),
            channel().0,
            channel().1,
            Arc::new(Shm::new("vgabios", 16)),
            Intr::handle(intr),
            EmulatorClock::new_asynchronous(),
        );
        let mut ctx = ExecutionContext::new(hw, &mem, None, cache);
        ctx.protected_mode = true;
        // Clear page tables
        mem.write_physical_slice(0, &[0; 4096 * 2], &mut ctx.mmio_ctx).unwrap();

        // Write PDE for
        const PDE_OFFSET: u32 = (0x7c814000u32 >> 22) * 4;
        mem.write_physical_slice(PDE_OFFSET, &0x00001007u32.to_le_bytes(), &mut ctx.mmio_ctx)
            .unwrap();

        const PTE_OFFSET1: u32 = 0x1000 + ((0x7c814000u32 >> 12) & 0x3ff) * 4;
        const PTE_OFFSET2: u32 = 0x1000 + ((0x7c815000u32 >> 12) & 0x3ff) * 4;

        mem.write_physical_slice(PTE_OFFSET1, &0x00003007u32.to_le_bytes(), &mut ctx.mmio_ctx)
            .unwrap();
        mem.write_physical_slice(PTE_OFFSET2, &0x00004007u32.to_le_bytes(), &mut ctx.mmio_ctx)
            .unwrap();

        // Entry instruction
        mem.write_slice(0x7c814ffe, &[0x50], false, &mut ctx.mmio_ctx).unwrap();

        // Place instruction crossing page bounds
        mem.write_slice(0x7c814fff, &[0x8b, 0x45, 0x18, 0x51], false, &mut ctx.mmio_ctx)
            .unwrap();

        // Load entries into cache
        let flags = ContextFlags::build(true, false, SegmentSizes::Cs32Ss32);

        // Unmap 0x7C814000 and 0x7C815000 by zeroing their PTEs
        mem.write_physical_slice(PTE_OFFSET1, &0u32.to_le_bytes(), &mut ctx.mmio_ctx)
            .unwrap();
        mem.write_physical_slice(PTE_OFFSET2, &0u32.to_le_bytes(), &mut ctx.mmio_ctx)
            .unwrap();
        ctx.mmio_ctx
            .icache
            .notify_page_mapping_updated(LinAddr::new(0x7C814000u32).into(), &mem);
        mem.invalidate_page(0x7C814000u32);

        ctx.mmio_ctx
            .icache
            .notify_page_mapping_updated(LinAddr::new(0x7C815000u32).into(), &mem);
        mem.invalidate_page(0x7C815000u32);

        // 0x3000 is now no longer bound to 0x7C814000
        assert!(
            ctx.mmio_ctx
                .icache
                .lookup_first(0, 0x7c814ffe, flags, &mem, |_| (), false)
                .is_err(),
            "entry lookup at 0x7C814000 should fail"
        );

        // 0x4000 is now no longer bound to 0x7C815000
        assert!(
            ctx.mmio_ctx
                .icache
                .lookup_first(0, 0x7c814fff, flags, &mem, |_| (), false)
                .is_err(),
            "entry lookup at 0x7C815000 should fail"
        );

        // But if we now remap only the first page
        mem.write_physical_slice(PTE_OFFSET1, &0x00003007u32.to_le_bytes(), &mut ctx.mmio_ctx)
            .unwrap();
        ctx.mmio_ctx
            .icache
            .notify_page_mapping_updated(LinAddr::new(0x7C814000u32).into(), &mem);
        mem.invalidate_page(0x7C814000u32);

        // The first access should succeed again
        assert!(
            ctx.mmio_ctx
                .icache
                .lookup_first(0, 0x7c814ffe, flags, &mem, |_| (), false)
                .is_ok(),
            "entry lookup at 0x7C814000 should succeed"
        );

        // But the second access should still fail
        assert!(
            ctx.mmio_ctx
                .icache
                .lookup_first(0, 0x7c814fff, flags, &mem, |_| (), false)
                .is_err(),
            "Entry lookup at 0x7C815000 should fail after unmapping"
        );
    }

    #[test]
    fn icpb_entry_must_be_updated_even_if_no_longer_mapped_clean() {
        let instr_semantics = &SEMANTICS;
        make_guard!(guard);
        let cache = InstructionCache::new(
            guard,
            Arc::new((**instr_semantics).clone()),
            SingleEncodingExecution::new(InkwellBackend::new(InkwellContext::leak_new()), instr_semantics.len()),
        );
        let shm = Arc::new(Shm::new("test", 1 << 20));
        let mem = Arc::new(Mem32::new(shm.clone()));
        mem.enable_paging(true);
        let intr = Intr::new();
        let intr = Pin::new(&intr);
        let hw = Hw::new(
            mem.clone(),
            Vec::new(),
            channel().0,
            channel().1,
            Arc::new(Shm::new("vgabios", 16)),
            Intr::handle(intr),
            EmulatorClock::new_asynchronous(),
        );
        let mut ctx = ExecutionContext::new(hw, &mem, None, cache);
        ctx.protected_mode = true;
        // Clear page tables
        mem.write_physical_slice(0, &[0; 4096 * 2], &mut ctx.mmio_ctx).unwrap();

        // Write PDE for
        const PDE_OFFSET: u32 = (0x7c814000u32 >> 22) * 4;
        mem.write_physical_slice(PDE_OFFSET, &0x00001007u32.to_le_bytes(), &mut ctx.mmio_ctx)
            .unwrap();

        const PTE_OFFSET1: u32 = 0x1000 + ((0x7c814000u32 >> 12) & 0x3ff) * 4;
        const PTE_OFFSET2: u32 = 0x1000 + ((0x7c815000u32 >> 12) & 0x3ff) * 4;

        mem.write_physical_slice(PTE_OFFSET1, &0x00003007u32.to_le_bytes(), &mut ctx.mmio_ctx)
            .unwrap();
        mem.write_physical_slice(PTE_OFFSET2, &0x00004007u32.to_le_bytes(), &mut ctx.mmio_ctx)
            .unwrap();

        // Entry instruction
        mem.write_slice(0x7c814ffe, &[0x50], false, &mut ctx.mmio_ctx).unwrap();

        // Place instruction crossing page bounds
        mem.write_slice(0x7c814fff, &[0x8b, 0x45, 0x18, 0x51], false, &mut ctx.mmio_ctx)
            .unwrap();

        // Load entries into cache
        let flags = ContextFlags::build(true, false, SegmentSizes::Cs32Ss32);

        // Wait until the entries are cleaned
        for _ in 0..50_000 {
            let entry1 = ctx
                .mmio_ctx
                .icache
                .lookup_first(0, 0x7c814ffe, flags, &mem, |_| (), false)
                .unwrap();
            let entry2 = ctx
                .mmio_ctx
                .icache
                .lookup_next_from_entry(entry1, (0, 0x7c814fff, &*mem), flags, |_| (), false, false)
                .unwrap();
            ctx.mmio_ctx
                .icache
                .lookup_next_from_entry(entry2, (0, 0x7c815002, &*mem), flags, |_| (), false, false)
                .unwrap();
        }

        // Unmap 0x7C814000 and 0x7C815000 by zeroing their PTEs
        mem.write_physical_slice(PTE_OFFSET1, &0u32.to_le_bytes(), &mut ctx.mmio_ctx)
            .unwrap();
        mem.write_physical_slice(PTE_OFFSET2, &0u32.to_le_bytes(), &mut ctx.mmio_ctx)
            .unwrap();
        ctx.mmio_ctx
            .icache
            .notify_page_mapping_updated(LinAddr::new(0x7C814000u32).into(), &mem);
        mem.invalidate_page(0x7C814000u32);

        ctx.mmio_ctx
            .icache
            .notify_page_mapping_updated(LinAddr::new(0x7C815000u32).into(), &mem);
        mem.invalidate_page(0x7C815000u32);

        // 0x3000 is now no longer bound to 0x7C814000
        assert!(
            ctx.mmio_ctx
                .icache
                .lookup_first(0, 0x7c814ffe, flags, &mem, |_| (), false)
                .is_err(),
            "entry lookup at 0x7C814000 should fail"
        );

        // 0x4000 is now no longer bound to 0x7C815000
        assert!(
            ctx.mmio_ctx
                .icache
                .lookup_first(0, 0x7c814fff, flags, &mem, |_| (), false)
                .is_err(),
            "entry lookup at 0x7C815000 should fail"
        );

        // But if we now remap only the first page
        mem.write_physical_slice(PTE_OFFSET1, &0x00003007u32.to_le_bytes(), &mut ctx.mmio_ctx)
            .unwrap();
        ctx.mmio_ctx
            .icache
            .notify_page_mapping_updated(LinAddr::new(0x7C814000u32).into(), &mem);
        mem.invalidate_page(0x7C814000u32);

        // The first access should succeed again
        assert!(
            ctx.mmio_ctx
                .icache
                .lookup_first(0, 0x7c814ffe, flags, &mem, |_| (), false)
                .is_ok(),
            "entry lookup at 0x7C814000 should succeed"
        );

        // But the second access should still fail
        assert!(
            ctx.mmio_ctx
                .icache
                .lookup_first(0, 0x7c814fff, flags, &mem, |_| (), false)
                .is_err(),
            "Entry lookup at 0x7C815000 should fail after unmapping"
        );
    }

    // TODO: If page is already dirty before we first create the entry, we might not have marked the physical frame as dirty.
}
