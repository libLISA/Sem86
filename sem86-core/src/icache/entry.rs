use std::collections::VecDeque;
use std::fmt::Debug;
use std::ops::{Index, IndexMut};

use bilge::prelude::*;
use liblisa::Instruction;
use log::trace;
use rand::RngCore;
use sem86_arch::addr::{LinAddr, LinPageIndex, PhysAddr, PhysFrameIndex};
use sem86_arch::mem::Mem32;
use serde::{Deserialize, Serialize};

use super::debug::EntrySnapshot;
use crate::SegmentSizes;
use crate::codegen::backends::Backend;
use crate::codegen::backends::inkwell::UninstantiatedInkwellFunction;
use crate::codegen::page::PageCode;
use crate::codegen::see::SingleEncodingExecution;
use crate::decoder::EncodingLookup;
use crate::icache::debug::{ExecutionKind, LinksSnapshot};
use crate::icache::exec::{EncodingInfo, Executable};
use crate::icache::inner::InnerCache;
use crate::icache::mapping::MappingTracker;
use crate::icache::zoc::{Zoc, ZocIndex};
use crate::il::part_values::PartValues;

#[derive(Copy, Clone, PartialEq, Eq, Hash)]
pub struct CacheEntryId<'tag>(ZocIndex<'tag>);

impl Debug for CacheEntryId<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "${}", self.0)
    }
}

impl CacheEntryId<'_> {
    #[inline(always)]
    pub fn as_u32(&self) -> u32 {
        self.0.as_u32()
    }

    #[inline(always)]
    pub unsafe fn new_unchecked(index: u32) -> Self {
        unsafe { Self(ZocIndex::new_unchecked(index)) }
    }
}

#[bitsize(3)]
#[derive(Copy, Clone, DebugBits, FromBits, PartialEq, Eq)]
pub struct EntryEnv {
    pub segment_sizes: SegmentSizes,

    /// Whether we're running in protected mode and VM=0
    pub effective_protected_mode: bool,
}

#[bitsize(2)]
#[derive(Copy, Clone, Debug, FromBits, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum EntryPoint {
    #[default]
    None,
    /// This is an entry point for an unpredictable jump from the same page.
    /// For example, this may be an entry in a jump table.
    Local,

    /// This is an entry point for a jump from another page.
    External,

    /// This is the entry point for, for example, an interrupt handler.
    Global,
}

impl EntryPoint {
    pub fn combine(self, other: EntryPoint) -> Self {
        self.max(other)
    }
}

#[bitsize(8)]
#[derive(Copy, Clone, DebugBits, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EntryFlags {
    pub entry_kind: EntryPoint,
    reserved: u6,
}

/// Helper type to show rust-analyzer type size hint
type _EntrySize<'tag> = CacheEntry<'tag, fn()>;

#[repr(align(64))]
pub struct CacheEntry<'tag, F> {
    /// The environment for which this entry was created.
    /// Upon first entry, we need to ensure that the environment in which we created this entry is still the same.
    /// We do not need to check this after following links, as we do not create links for instructions that can change the environment.
    env: EntryEnv,

    // The index of the physical frame in which this entry is stored in memory.
    phys_frame_index: PhysFrameIndex,

    flags: EntryFlags,

    /// The offset in the physical frame.
    frame_offset: u16,

    /// The single encoding at this address.
    execute: ExecutionFunction<'tag, F>,

    encoding_index: u32,
    part_values: PackedPartValues,
    /// The length of the instruction at this address.
    instr_len: u8,

    /// Links to the next entry that should be executed.
    links: Links<'tag>,
}

impl<F: Default> Default for CacheEntry<'_, F> {
    fn default() -> Self {
        Self {
            env: EntryEnv::new(SegmentSizes::Cs16Ss16, false),
            frame_offset: 0,
            execute: ExecutionFunction::default(),
            phys_frame_index: Default::default(),
            links: Links::EMPTY,
            flags: EntryFlags::default(),
            encoding_index: 0,
            part_values: PackedPartValues::from(PartValues::ALL_ZERO),
            instr_len: 0,
        }
    }
}

impl<'tag> CacheEntry<'tag, UninstantiatedInkwellFunction> {
    pub fn new(
        phys_addr: PhysAddr, entry_env: EntryEnv, execute: UninstantiatedInkwellFunction, info: EncodingInfo,
        predictable_jump: bool,
    ) -> Self {
        Self {
            frame_offset: phys_addr.frame_offset(),
            phys_frame_index: PhysFrameIndex::from(phys_addr),
            env: entry_env,
            flags: EntryFlags::default(),
            links: if predictable_jump {
                Links::EMPTY
            } else {
                Links::EMPTY_SPECULATIVE
            },
            execute: ExecutionFunction::Single {
                execute,
            },
            instr_len: info.instr_len,
            encoding_index: info.encoding_index as u32,
            part_values: info.part_values.into(),
        }
    }

    #[inline(always)]
    pub(super) fn resolve_instr(
        &self, phys_cache: &InnerCache<'tag>, page: LinPageIndex, mem: &Mem32, mapping: &MappingTracker,
    ) -> Instruction {
        phys_cache.resolve_instr(self, page, mem, mapping)
    }

    pub fn make_links_checked(&mut self) {
        self.links.make_checked()
    }
}

impl<'tag, F> CacheEntry<'tag, F> {
    #[inline(always)]
    pub fn as_executable(&self) -> Executable<'_, 'tag, F>
    where
        F: Copy,
    {
        match &self.execute {
            ExecutionFunction::Single {
                execute,
            } => Executable::Single {
                part_values: self.part_values.into(),
                instr_len: self.instr_len,
                execute: *execute,
            },
            ExecutionFunction::JittedPage {
                page,
            } => Executable::JittedPage {
                page,
            },
        }
    }

    #[inline(always)]
    pub fn encoding_info(&self) -> EncodingInfo
    where
        F: Copy,
    {
        EncodingInfo {
            encoding_index: self.encoding_index as usize,
            part_values: self.part_values.into(),
            instr_len: self.instr_len,
        }
    }

    #[inline(always)]
    pub fn env(&self) -> EntryEnv {
        self.env
    }

    #[inline(always)]
    pub fn phys_addr(&self) -> PhysAddr {
        self.phys_frame_index.with_offset(self.frame_offset)
    }

    #[inline(always)]
    pub fn phys_frame_index(&self) -> PhysFrameIndex {
        self.phys_frame_index
    }

    #[inline(always)]
    pub fn crosses_page_bounds(&self) -> bool {
        self.frame_offset + self.instr_len as u16 > 4096
    }

    #[inline(always)]
    pub fn instr_len(&self) -> u8 {
        self.instr_len
    }

    /// Returns true if any links were removed.
    pub fn remove_links_matching(&mut self, entry_to_remove: CacheEntryId<'tag>) -> bool {
        self.links.remove(entry_to_remove)
    }

    #[inline(always)]
    pub fn links(&self) -> &Links<'tag> {
        &self.links
    }

    #[inline(always)]
    pub fn clear_links(&mut self) {
        self.links.clear()
    }

    #[inline(always)]
    pub fn set_link(
        &mut self, target: CacheEntryId<'tag>, to_pc: LinAddr, jump_taken: bool, needs_checks: bool,
    ) -> Option<CacheEntryId<'tag>> {
        match &mut self.links {
            Links::Unchecked(links) => {
                let old = links[jump_taken as usize];
                if needs_checks {
                    let mut links = *links;
                    links[jump_taken as usize] = Some(target);
                    self.links = Links::Checked(links);
                } else {
                    links[jump_taken as usize] = Some(target);
                }

                old
            },
            Links::Checked(links) => {
                let old = links[jump_taken as usize];
                links[jump_taken as usize] = Some(target);
                old
            },
            Links::Speculative(links) => {
                links[rand::rng().next_u32() as usize % 2] = to_pc;
                None
            },
        }
    }

    #[inline(always)]
    pub fn jitted_page(&self) -> Option<&PageCode<'tag>> {
        if let ExecutionFunction::JittedPage {
            page,
        } = &self.execute
        {
            Some(page)
        } else {
            None
        }
    }

    pub fn set_jitted_page(&mut self, page: PageCode<'tag>) {
        self.execute = ExecutionFunction::JittedPage {
            page,
        };
    }

    pub fn flags(&self) -> EntryFlags {
        self.flags
    }

    #[inline(always)]
    pub fn flags_mut(&mut self) -> &mut EntryFlags {
        &mut self.flags
    }

    pub fn revert_to_single_execution<B: Backend<UninstantiatedFn = F>>(
        &mut self, backend: &mut SingleEncodingExecution<B>, encodings: &impl EncodingLookup,
    ) {
        if !matches!(self.execute, ExecutionFunction::Single { .. }) {
            self.execute = ExecutionFunction::Single {
                execute: backend.get_or_build(
                    self.encoding_index as usize,
                    encodings.get(self.encoding_index as usize),
                    self.env.effective_protected_mode(),
                    self.env.segment_sizes(),
                    || (),
                ),
            };
        }
    }

    pub fn debug_snapshot(&self) -> EntrySnapshot {
        EntrySnapshot {
            phys_addr: self.phys_addr(),
            encoding_index: self.encoding_index,
            part_values: self.part_values.into(),
            instr_len: self.instr_len,
            links: match self.links {
                Links::Unchecked(links) | Links::Checked(links) => {
                    LinksSnapshot::Conditional(links.map(|link| link.map(|link| link.into())))
                },
                Links::Speculative(links) => LinksSnapshot::Speculative(links),
            },
            execution_kind: match &self.execute {
                ExecutionFunction::Single {
                    ..
                } => ExecutionKind::Single,
                ExecutionFunction::JittedPage {
                    ..
                } => ExecutionKind::JittedPage,
            },
            flags: self.flags,
        }
    }

    fn clear_for_release(&mut self) {
        self.phys_frame_index = PhysFrameIndex::new(0);
        self.clear_links();
    }

    pub fn frame_offset(&self) -> u16 {
        self.frame_offset
    }
}

#[derive(Clone, Debug)]
pub enum Links<'tag> {
    /// Links for jump taken/jump not taken.
    /// The frame of the next entry is **not checked** for changes.
    ///
    /// You can use this, for example, when the next instruction is guaranteed to be on the same page.
    Unchecked([Option<CacheEntryId<'tag>>; 2]),

    /// Links for jump taken/jump not taken.
    /// The frame of the next entry is checked for changes.
    Checked([Option<CacheEntryId<'tag>>; 2]),

    /// Next addresses; Collected only for
    Speculative([LinAddr; 2]),
}

impl Default for Links<'_> {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl<'tag> Links<'tag> {
    pub const EMPTY: Self = Self::Unchecked([None, None]);
    pub const EMPTY_SPECULATIVE: Self = Self::Speculative([LinAddr::new(0), LinAddr::new(0)]);

    #[inline(always)]
    fn clear(&mut self) {
        match self {
            Links::Unchecked(_) | Links::Checked(_) => *self = Self::EMPTY,
            Links::Speculative(_) => *self = Self::EMPTY_SPECULATIVE,
        }
    }

    #[inline(always)]
    fn remove(&mut self, entry_to_remove: CacheEntryId<'tag>) -> bool {
        let mut any = false;
        match self {
            Links::Unchecked(links) | Links::Checked(links) => {
                for link in links.iter_mut() {
                    if *link == Some(entry_to_remove) {
                        *link = None;
                        any = true;
                    }
                }
            },
            Links::Speculative(_) => (),
        }

        any
    }

    pub fn make_checked(&mut self) {
        if let Links::Unchecked(links) = self {
            *self = Links::Checked(*links)
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub struct SpeculativeLink<'tag> {
    pub lin_addr: LinAddr,
    pub entry: CacheEntryId<'tag>,
}

type _ExecutionFunctionSize = ExecutionFunction<'static, fn()>;

enum ExecutionFunction<'tag, F> {
    Single { execute: F },
    JittedPage { page: PageCode<'tag> },
}

impl<F: Default> Default for ExecutionFunction<'_, F> {
    fn default() -> Self {
        ExecutionFunction::Single {
            execute: Default::default(),
        }
    }
}

#[derive(Copy, Clone)]
struct PackedPartValues(u64, u64);

impl From<PartValues> for PackedPartValues {
    #[inline(always)]
    fn from(value: PartValues) -> Self {
        Self(value.as_u128() as u64, (value.as_u128() >> 64) as u64)
    }
}

impl From<PackedPartValues> for PartValues {
    #[inline(always)]
    fn from(value: PackedPartValues) -> Self {
        PartValues::from_u128(value.0 as u128 | ((value.1 as u128) << 64))
    }
}

pub struct CacheEntries<'tag, F> {
    data: Zoc<'tag, CacheEntry<'tag, F>>,
    free_entry_indices: VecDeque<ZocIndex<'tag>>,
}

impl<'tag, F> CacheEntries<'tag, F> {
    pub fn new(guard: generativity::Guard<'tag>) -> Self {
        Self {
            data: Zoc::new(guard),
            free_entry_indices: VecDeque::new(),
        }
    }

    pub fn create_new(&mut self, entry: CacheEntry<'tag, F>) -> CacheEntryId<'tag> {
        // We ensure there is some delay between freeing an entry and re-using it
        CacheEntryId(if self.free_entry_indices.len() > 10 {
            let index = self.free_entry_indices.pop_front().unwrap();
            self.data[index] = entry;
            index
        } else {
            self.data.push(entry)
        })
    }

    pub fn iter(&self) -> impl Iterator<Item = &CacheEntry<'tag, F>> {
        self.data.iter()
    }

    pub fn release_id(&mut self, entry: CacheEntryId<'tag>) {
        trace!("Entry {entry:?} released");
        self.free_entry_indices.push_back(entry.0);

        self.data[entry.0].clear_for_release();

        // TODO: This is very expensive
        // #[cfg(debug_assertions)]
        // for (index, entry) in self.data.iter_with_indices() {
        //     if !self.free_entry_indices.contains(&index) {
        //         match entry.links().unpack() {
        //             UnpackedLinks::Certain(id) => {
        //                 assert!(!self.free_entry_indices.contains(&id.0), "entry {index:?} should not contain a reference to a released entry {id:X?}")
        //             },
        //             UnpackedLinks::Speculative(links) => for link in links {
        //                 assert!(!self.free_entry_indices.contains(&link.entry.0), "entry {index:?} should not contain a reference to a released entry {link:X?}");
        //             }
        //             UnpackedLinks::Empty => (),
        //         }
        //     }
        // }
    }

    pub fn is_released(&self, id: CacheEntryId<'tag>) -> bool {
        self.free_entry_indices.contains(&id.0)
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn clear(&mut self) {
        // TODO: Make this an unsafe function that actually releases all memory in `self.data`
        self.free_entry_indices.clear();
        self.free_entry_indices
            .extend(self.data.iter_with_indices().map(|(index, _)| index));
    }
}

impl<'tag, F> Index<CacheEntryId<'tag>> for CacheEntries<'tag, F> {
    type Output = CacheEntry<'tag, F>;

    fn index(&self, index: CacheEntryId<'tag>) -> &Self::Output {
        &self.data[index.0]
    }
}

impl<'tag, F> IndexMut<CacheEntryId<'tag>> for CacheEntries<'tag, F> {
    fn index_mut(&mut self, index: CacheEntryId<'tag>) -> &mut Self::Output {
        &mut self.data[index.0]
    }
}
