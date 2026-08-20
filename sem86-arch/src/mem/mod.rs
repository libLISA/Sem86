use std::collections::HashMap;
use std::ffi::c_void;
use std::fmt::{Debug, Display, UpperHex};
use std::num::{NonZero, NonZeroUsize};
use std::ops::Range;
use std::os::fd::AsFd;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bilge::prelude::Number;
use bitcode::{Decode, Encode};
use liblisa::utils::bitmap::BitmapSlice;
use liblisa::utils::bitmask_u64;
use log::{debug, error, info, trace, warn};
use metadata::{MetadataBits, MetadataMap, MetadataTest};
use nix::sys::mman::{MapFlags, ProtFlags, mmap, mmap_anonymous, munmap};
use paging::{BigPde, Pde, Pte};
use phys_map::PhysMap;

mod bitmap;
pub mod metadata;
pub mod pae;
mod paging;
mod phys_map;
mod shm;
pub mod snapshot;
mod watcher;

use serde::{Deserialize, Serialize};
pub use shm::{Shm, ShmView};
pub use watcher::MemoryWatcher;

use crate::addr::{PhysAddr, PhysFrameIndex};
use crate::exceptions::{Exception, PageFaultCode};
use crate::mem::bitmap::AtomicBitmap;
use crate::mem::pae::{BigPaePde, PaeAddr32, PaePde, PaePdpe, PaePte};
use crate::mem::phys_map::EntryRef;
use crate::mem::snapshot::MemorySnapshot;

const ALIGNMENT_BITS: u32 = 34;
const MAPPING_KIND_LOGICAL: u64 = 0x0 << 32;
const MAPPING_KIND_PHYSICAL: u64 = 0x1 << 32;
pub const METADATA_SIZE: u64 = 1 << 20;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
#[repr(transparent)]
pub struct MmioId(u64);

impl MmioId {
    pub const fn new(val: u64) -> Self {
        Self(val)
    }

    pub fn as_u64(&self) -> u64 {
        self.0
    }
}

pub trait Mmio {
    fn read_mem<D: MemoryData>(&mut self, id: MmioId, address: u32) -> D;
    fn write_mem<D: MemoryData>(&mut self, id: MmioId, address: u32, val: D);

    /// Notifies that clean memory is about to be written.
    /// This notification is guaranteed to occur before the memory write,
    /// so that copies of the memory can still be made.
    ///
    /// Note that this notification happens before the page is marked dirty.
    /// Performing a memory write may trigger another notification.
    /// You must ensure to not overflow the stack this way.
    fn notify_memory_dirty(&mut self, phys_frame_index: PhysFrameIndex, memory: &Mem32);

    /// Requests advice on whether to mark clean memory dirty before a memory write.
    /// If advised not to mark memory dirty, the write will still occur, but memory is not marked dirty.
    fn advise_memory_dirty(&mut self, addr: PhysAddr, len: u8) -> MarkDirtyAdvice;
}

pub enum MarkDirtyAdvice {
    /// Performs the write, but doesn't mark the frame as dirty.
    /// The next time a write is performed to this frame, advive_memory_dirty and notify_memory_dirty are still triggered.
    DoNotMark,

    /// Performs the write and marks the frame as dirty.
    /// The next time a write is performed to this frame, it is written directly without any notification.
    DirtyOk,
}

#[derive(Debug)]
pub enum PageWalkError {
    PdeNotPresent {
        pde: Pde,
        addr: u32,
    },
    PteNotPresent {
        pde: Pde,
        pte: Pte,
        addr: u32,
    },
    PaePdpeNotPresent {
        pdpe: PaePdpe,
        addr: u32,
    },
    PaePdeNotPresent {
        pdpe: PaePdpe,
        pde: PaePde,
        addr: u32,
    },
    PaePteNotPresent {
        pdpe: PaePdpe,
        pde: PaePde,
        pte: PaePte,
        addr: u32,
    },
}

#[derive(Debug)]
pub struct DirtyMarker {
    addr: *mut u32,
}

impl DirtyMarker {
    pub fn mark_dirty(self) {
        unsafe {
            *self.addr |= 1 << 6;
        }
    }
}

#[derive(Debug)]
pub enum BigPage {
    Normal4Kb,
    Big2Mb,
    Big4Mb,
}

#[derive(Debug)]
pub enum PageWalkResult {
    Unmapped(PageWalkError),
    PhysAddr {
        addr: u64,
        writable: bool,
        user_accessible: bool,
        dirty: bool,
        mark_dirty: DirtyMarker,
        big_page: BigPage,
        global: bool,
    },
}

impl PageWalkResult {
    pub fn unwrap_phys_addr(&self) -> u64 {
        match self {
            PageWalkResult::Unmapped(_) => todo!(),
            PageWalkResult::PhysAddr {
                addr, ..
            } => *addr,
        }
    }
}

pub trait MemoryData: Copy + Debug + Display + UpperHex {
    const NUM_BYTES: usize;
    const MAX: Self;

    fn from_bytes(bytes: &[u8]) -> Self;
    fn to_bytes(self) -> impl AsRef<[u8]>;

    fn from_u32_with_offset(offset: impl Into<u32>, val: u32) -> Self;
    fn into_u32_exact(self) -> Option<u32>;

    fn from_u16_with_offset(offset: impl Into<u16>, val: u16) -> Self;
    fn into_u16_exact(self) -> Option<u16>;

    unsafe fn read_from_unaligned_pointer(ptr: *mut u8) -> Self;
    unsafe fn write_to_unaligned_pointer(self, ptr: *mut u8);
}

/// Provides a 32-bit memory for emulation.
/// We initially map an entire 4GiB region to reserve it.
/// `map_rw` can then be called to make parts of this region readable and writable.
pub struct Mem32 {
    base: *mut u8,
    page_table_addr: AtomicU32,
    a20_enabled: AtomicBool,
    system_write_protect: AtomicBool,
    page_size_extension: AtomicBool,
    physical_address_extension: AtomicBool,
    paging_enabled: AtomicBool,
    phys_map: Mutex<PhysMap>,
    frame_is_clean: AtomicBitmap,
    current_mappings: Mutex<HashMap<u32, u64>>,
    page_faults: AtomicU64,
    mmap_count: AtomicU64,
    num_page_walks: AtomicU64,
    phys_frames_marked_dirty: AtomicU64,
    physical_memory: Arc<Shm>,
    metadata_clears: AtomicU64,
    num_unaligned_reads: AtomicU64,
    num_trapped_reads: AtomicU64,
    num_page_bounds_crossing_reads: AtomicU64,
    slow_writes: AtomicU64,
}

impl Debug for Mem32 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mem32").finish()
    }
}

unsafe impl Send for Mem32 {}
unsafe impl Sync for Mem32 {}

impl AsRef<Mem32> for Mem32 {
    fn as_ref(&self) -> &Mem32 {
        self
    }
}

impl Mem32 {
    pub fn new(physical_memory: Arc<Shm>) -> Self {
        let address_space_size = 1u64 << 32;

        // We want our memory to be aligned such that the lower 34 bits are all zeros.
        // However, we do not know where the OS is going to allocate our memory.
        // So we need to allocate extra, such that we can then use a subset of it and deallocate the rest.
        // In order to ensure that we can always slice correctly, we need to allocate alignment + needed size.
        //
        // For example, if we want to allocate 48 bytes aligned to 32 bytes, we allocate 80 bytes (48 + 32).
        // We can then use `(address & !0x1f) + 0x20` as the starting address of the 48 bytes.
        // At most, this address is 32 bytes into the allocated range, which still leaves us enough free space.
        let size_needed = address_space_size * 3;

        // Align to 16GiB
        let alignment = 1u64 << ALIGNMENT_BITS;
        let size_to_allocate = alignment + size_needed;

        // Allocate the memory
        info!("Allocating 0x{size_to_allocate:X} bytes");
        let addr = unsafe {
            let length = NonZeroUsize::new(size_to_allocate.try_into().unwrap()).unwrap();
            mmap_anonymous(
                None,
                length,
                ProtFlags::PROT_NONE,
                MapFlags::MAP_ANONYMOUS | MapFlags::MAP_PRIVATE,
            )
            .expect("should be able to mmap the full 32-bit memory space")
        }
        .as_ptr() as *mut u8;

        info!("Received memory area at {addr:p}");
        let base = ((addr as u64) & (u64::MAX << ALIGNMENT_BITS)) + (1u64 << ALIGNMENT_BITS);
        let end = base + size_needed;

        info!("Area that we will use: 0x{base:X}..0x{end:X}");
        // Free the space at the start that we will not use
        let unused_bytes_at_start = base - addr as u64;

        // We will always have at least one page available before `base`.
        let bytes_to_unmap_at_start = usize::try_from(unused_bytes_at_start).unwrap();
        if bytes_to_unmap_at_start > 0 {
            info!("Unmapping 0x{bytes_to_unmap_at_start:X} unused bytes at the front of the area");
            unsafe { munmap(NonNull::new(addr as *mut c_void).unwrap(), bytes_to_unmap_at_start).unwrap() }
        }

        // Free the space at the end that we will not use.
        let unused_bytes_at_end = (size_to_allocate - unused_bytes_at_start - size_needed).try_into().unwrap();
        if unused_bytes_at_end > 0 {
            info!("Unmapping 0x{unused_bytes_at_end:X} unused bytes at the end of the area");
            unsafe { munmap(NonNull::new(end as *mut c_void).unwrap(), unused_bytes_at_end).unwrap() }
        }

        let memory_start = base + (1 << 32);
        unsafe {
            let metadata_start = memory_start - METADATA_SIZE;
            let result_addr = mmap_anonymous(
                Some(NonZero::new(metadata_start as usize).unwrap()),
                NonZero::new(1 << 20).unwrap(),
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_PRIVATE | MapFlags::MAP_FIXED,
            )
            .unwrap();
            assert_eq!(result_addr.as_ptr() as u64, metadata_start);

            let result_addr = mmap_anonymous(
                Some(NonZero::new(metadata_start as usize - 4096).unwrap()),
                NonZero::new(4096).unwrap(),
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_PRIVATE | MapFlags::MAP_FIXED,
            )
            .unwrap();
            assert_eq!(result_addr.as_ptr() as u64, metadata_start - 4096);

            MetadataMap::from_ptr(metadata_start as *mut u8).initialize();
        }

        let result = Self {
            base: memory_start as *mut u8,
            frame_is_clean: AtomicBitmap::new_all_ones(0x10_0000),
            physical_memory,
            page_table_addr: AtomicU32::new(0),
            a20_enabled: AtomicBool::new(false),
            paging_enabled: AtomicBool::new(false),
            system_write_protect: AtomicBool::new(false),
            page_size_extension: AtomicBool::new(false),
            physical_address_extension: AtomicBool::new(false),
            phys_map: Mutex::new(PhysMap::new()),
            page_faults: AtomicU64::new(0),
            mmap_count: AtomicU64::new(0),
            num_unaligned_reads: AtomicU64::new(0),
            num_trapped_reads: AtomicU64::new(0),
            num_page_bounds_crossing_reads: AtomicU64::new(0),
            slow_writes: AtomicU64::new(0),
            phys_frames_marked_dirty: AtomicU64::new(0),
            metadata_clears: AtomicU64::new(0),
            num_page_walks: AtomicU64::new(0),
            current_mappings: Mutex::new(HashMap::new()),
        };
        result.map_physical_memory_to_default(0..result.physical_memory.len());

        result
    }

    #[inline(always)]
    fn fast(&self) -> FastMem32<&Mem32> {
        FastMem32::new(self)
    }

    fn metadata_bits(&self) -> MetadataMap {
        self.fast().metadata_bits()
    }

    fn logical_base(&self) -> *mut u8 {
        self.fast().logical_base()
    }

    #[inline(always)]
    fn phys_base(&self) -> *mut u8 {
        self.fast().phys_base()
    }

    pub fn map_physical_memory_to_shm(
        &self, area: Range<u64>, shm: Arc<Shm>, watcher: Option<Arc<dyn MemoryWatcher>>, offset: usize, writable: bool,
    ) {
        assert_eq!(area.start & 0xfff, 0, "area must be aligned to page");
        assert_eq!(area.end & 0xfff, 0, "area must be aligned to page");
        assert!(area.start < area.end, "area start must be less than area end");

        assert!(
            (area.end - area.start) + offset as u64 <= shm.len(),
            "must not go out of bounds of shm"
        );

        debug!("Mapping {area:X?} to SHM {shm:?}");
        self.phys_map
            .lock()
            .unwrap()
            .map_shm(area.clone(), shm.clone(), watcher, offset as u64, writable);

        // We pre-emptively map into the physical unchecked space, because this is needed for page walks
        let addr = NonZeroUsize::new(self.phys_base() as usize + area.start as usize).unwrap();
        let length = NonZeroUsize::new((area.end - area.start) as usize).unwrap();

        unsafe {
            let result_addr = mmap(
                Some(addr),
                length,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_SHARED | MapFlags::MAP_FIXED,
                shm,
                offset as i64,
            )
            .expect("should be able to map shared memory");

            assert_eq!(result_addr.as_ptr() as usize, addr.get());
        }

        self.clean_phys_frame_range(area);
        self.invalidate_all_pages();
    }

    pub fn map_physical_memory_to_mmio(&self, area: Range<u64>, id: MmioId) {
        // TODO: If these constraints are removed, additional checks are needed for prepared writes
        assert_eq!(area.start & 0xfff, 0, "area must be aligned to page");
        assert_eq!(area.end & 0xfff, 0, "area must be aligned to page");
        assert!(area.start < area.end, "area start must be less than area end");

        debug!("Mapping {area:X?} to MMIO {id:?}");
        self.phys_map.lock().unwrap().map_mmio(area.clone(), id);
        self.clean_phys_frame_range(area);
        self.invalidate_all_pages();
    }

    pub fn map_physical_memory_to_default(&self, area: Range<u64>) {
        // TODO: If these constraints are removed, additional checks are needed for prepared writes
        assert_eq!(area.start & 0xfff, 0, "area must be aligned to page");
        assert_eq!(area.end & 0xfff, 0, "area must be aligned to page");
        assert!(area.start < area.end, "area start must be less than area end");

        debug!("Mapping {area:X?} to RAM");
        self.phys_map.lock().unwrap().remove_map(area.clone());

        // We pre-emptively map into the physical unchecked space, because this is needed for page walks
        let addr = NonZeroUsize::new(self.phys_base() as usize + area.start as usize).unwrap();
        let length = NonZeroUsize::new((area.end - area.start) as usize).unwrap();

        unsafe {
            let result_addr = mmap(
                Some(addr),
                length,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_SHARED | MapFlags::MAP_FIXED,
                self.physical_memory.as_fd(),
                area.start as i64,
            )
            .expect("should be able to map shared memory");

            assert_eq!(result_addr.as_ptr() as usize, addr.get());
        }

        self.clean_phys_frame_range(area);
        self.invalidate_all_pages();
    }

    pub fn set_a20_line(&self, enable: bool) {
        self.a20_enabled.store(enable, Ordering::Relaxed);
        self.invalidate_all_pages();
        self.current_mappings.lock().unwrap().clear();
    }

    pub fn set_page_directory_base(&self, addr: u32) {
        self.page_table_addr.store(addr, Ordering::Relaxed);

        info!("Page table directory base address changed: 0x{addr:X}");

        self.invalidate_all_pages();
    }

    pub fn enable_paging(&self, enable: bool) {
        self.paging_enabled.store(enable, Ordering::Relaxed);
    }

    pub fn paging_enabled(&self) -> bool {
        self.paging_enabled.load(Ordering::Relaxed)
    }

    pub fn set_system_write_protect(&self, write_protect: bool) {
        self.system_write_protect.store(write_protect, Ordering::Relaxed)
    }

    pub fn set_page_size_extension(&self, page_size_extension: bool) {
        self.page_size_extension.store(page_size_extension, Ordering::Relaxed)
    }

    pub fn enable_physical_address_extension(&self, enable: bool) {
        if self
            .physical_address_extension
            .compare_exchange(!enable, enable, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            self.invalidate_all_pages();
        }
    }

    pub fn invalidate_all_pages(&self) {
        info!("Invalidating all mapped pages");

        self.metadata_clears.fetch_add(1, Ordering::Relaxed);
        self.metadata_bits().clear();
    }

    #[inline]
    pub fn invalidate_page(&self, addr: u32) {
        info!("Invalidating page 0x{addr:X}");

        self.metadata_bits().set(addr, MetadataBits::all());
    }

    pub fn phys_frames_marked_dirty(&self) -> u64 {
        self.phys_frames_marked_dirty.load(Ordering::SeqCst)
    }

    pub fn mmap_count(&self) -> u64 {
        self.mmap_count.load(Ordering::SeqCst)
    }

    pub fn num_page_walks(&self) -> u64 {
        self.num_page_walks.load(Ordering::SeqCst)
    }

    pub fn page_fault_count(&self) -> u64 {
        self.page_faults.load(Ordering::SeqCst)
    }

    #[inline]
    pub fn phys_frame_is_dirty(&self, phys_frame_addr: u64) -> bool {
        !self.frame_is_clean.get((phys_frame_addr >> 12) as usize)
    }

    pub fn clean_phys_frame(&self, phys_frame_addr: u64) {
        debug!("Cleaning physical frame 0x{phys_frame_addr:X}");

        self.frame_is_clean.set((phys_frame_addr >> 12) as usize);

        // TODO: Only invalidate pages that have this specific frame mapped
        self.metadata_clears.fetch_add(1, Ordering::Relaxed);
        self.metadata_bits().enable_for_all(MetadataBits::new().trap_on_write(true));
    }

    pub fn clean_phys_frame_range(&self, area: Range<u64>) {
        assert!(area.start.is_multiple_of(4096) && area.end.is_multiple_of(4096));
        for addr in area.step_by(4096) {
            self.frame_is_clean.set((addr >> 12) as usize);
        }

        // TODO: Only invalidate pages that have this specific frame mapped
        self.metadata_clears.fetch_add(1, Ordering::Relaxed);
        self.metadata_bits().enable_for_all(MetadataBits::new().trap_on_write(true));
    }

    pub fn clean_all_phys_frames(&self) {
        self.clean_phys_frame_range(0..0x1_0000_0000);
    }

    #[inline]
    pub fn mark_phys_frame_dirty(&self, phys_frame_addr: u64, mmio: &mut impl Mmio) -> bool {
        let changed = self.frame_is_clean.reset((phys_frame_addr >> 12) as usize);
        if changed {
            debug!("Marked physical frame 0x{phys_frame_addr:X} as dirty");
            mmio.notify_memory_dirty(PhysFrameIndex::from(PhysAddr::new(phys_frame_addr as u32)), self);
            self.phys_frames_marked_dirty.fetch_add(1, Ordering::Relaxed);
        }

        changed
    }

    #[inline]
    pub fn page_walk(&self, emu_addr: u32, mark_accessed: bool) -> PageWalkResult {
        self.num_page_walks.fetch_add(1, Ordering::Relaxed);
        if self.physical_address_extension.load(Ordering::Relaxed) {
            self.page_walk_pae(emu_addr, mark_accessed)
        } else {
            self.page_walk_non_pae(emu_addr, mark_accessed)
        }
    }

    fn page_walk_pae(&self, emu_addr: u32, mark_accessed: bool) -> PageWalkResult {
        let page_table_addr = self.page_table_addr.load(Ordering::Relaxed);

        let addr = PaeAddr32::from(emu_addr);

        // We must read from physical-unchecked here to ensure we never cause a segfault.
        // This does mean that if we set an accessed or dirty flag, this won't be reflected in the physical_frames_dirty list.
        // This seems an acceptable trade-off.
        let addr_base = self.phys_base();
        let phys_pdpe_addr = page_table_addr as u64 + addr.pdpe_offset().as_u64() * 8;

        let pdpe_addr = unsafe { addr_base.add(phys_pdpe_addr as usize) };
        let pdpe = PaePdpe::from(unsafe { *(pdpe_addr as *const u64) });

        if !pdpe.present() {
            // trace!("Walked page tables for address 0x{emu_addr:X}: PDE: {pde:X?}, need #PF");
            return PageWalkResult::Unmapped(PageWalkError::PaePdpeNotPresent {
                pdpe,
                addr: phys_pdpe_addr as u32,
            })
        }

        let page_directory_addr = pdpe.pd_base_addr();
        let phys_pde_addr = page_directory_addr + addr.pde_offset().as_u64() * 8;

        let pde_addr = unsafe { addr_base.add(phys_pde_addr as usize) };
        let mut pde = PaePde::from(unsafe { *(pde_addr as *const u64) });

        if !pde.present() {
            // trace!("Walked page tables for address 0x{emu_addr:X}: PDE: {pde:X?}, need #PF");
            return PageWalkResult::Unmapped(PageWalkError::PaePdeNotPresent {
                pdpe,
                pde,
                addr: phys_pde_addr as u32,
            })
        }

        if mark_accessed && !pde.accessed() {
            debug!("Marking as accessed: {pde:X?} @ 0x{phys_pde_addr:X} for address {emu_addr:X}");
            pde.set_accessed(true);
            unsafe {
                (pde_addr as *mut u64).write(pde.into());
            }
        }

        // Big 2MB pages -- lowest 21 bits used as offset
        // PSE is ignored in PAE mode
        if pde.big_page() {
            let pde = BigPaePde::from(u64::from(pde));
            let offset = emu_addr & bitmask_u64(21) as u32;
            let phys_addr = pde.phys_base_addr() | offset as u64;

            trace!("Big page (2MiB) for address 0x{emu_addr:X} at 0x{phys_pde_addr:X}: {pde:X?}");

            return PageWalkResult::PhysAddr {
                addr: phys_addr,
                writable: pde.writeable(),
                user_accessible: pde.user_accessible(),
                dirty: pde.dirty(),
                mark_dirty: DirtyMarker {
                    addr: pde_addr as *mut u32,
                },
                big_page: BigPage::Big4Mb,
                global: pde.global(),
            }
        }

        let phys_pte_addr = pde.pt_base_addr() + addr.pte_offset().as_u64() * 8;
        let pte_addr = unsafe { addr_base.add(phys_pte_addr as usize) };

        {
            let phys_map = self.phys_map.lock().unwrap();
            debug_assert!(
                matches!(phys_map.lookup(phys_pdpe_addr).0, EntryRef::Default),
                "page table lookup: PDE must be located in physical memory"
            );
            debug_assert!(
                matches!(phys_map.lookup(phys_pde_addr).0, EntryRef::Default),
                "page table lookup: PDE must be located in physical memory"
            );
            debug_assert!(
                matches!(phys_map.lookup(phys_pte_addr).0, EntryRef::Default),
                "page table lookup: PTE must be located in physical memory"
            );
        }

        let mut pte = PaePte::from(unsafe { *(pte_addr as *const u64) });
        if !pte.present() {
            // trace!("Walked page tables for address 0x{emu_addr:X}: PDE: {pde:X?}, PTE: {pte:X?}, need #PF");
            return PageWalkResult::Unmapped(PageWalkError::PaePteNotPresent {
                pdpe,
                pde,
                pte,
                addr: phys_pte_addr as u32,
            })
        }

        if mark_accessed && !pte.accessed() {
            debug!("Marking as accessed: {pte:X?} @ 0x{phys_pte_addr:X} for address {emu_addr:X}");
            pte.set_accessed(true);
            unsafe {
                (pte_addr as *mut u64).write(pte.into());
            }
        }

        let phys_addr = pte.phys_base_addr() | addr.page_offset().as_u64();
        trace!(
            "Walked pages: 0x{phys_pde_addr:X} = {pde:X?}, 0x{phys_pte_addr:X} = {pte:X?} to map {emu_addr:X} -> {phys_addr:X?}"
        );

        PageWalkResult::PhysAddr {
            addr: phys_addr,
            writable: pde.writeable() && pte.writeable(),
            user_accessible: pde.user_accessible() && pte.user_accessible(),
            dirty: pte.dirty(),
            mark_dirty: DirtyMarker {
                addr: pte_addr as *mut u32,
            },
            big_page: BigPage::Normal4Kb,
            global: pte.global(),
        }
    }

    fn page_walk_non_pae(&self, emu_addr: u32, mark_accessed: bool) -> PageWalkResult {
        let page_table_addr = self.page_table_addr.load(Ordering::Relaxed);

        let l1_bits = ((emu_addr >> 22) & 0x3ff) as u64;
        let l2_bits = ((emu_addr >> 12) & 0x3ff) as u64;

        // We must read from physical-unchecked here to ensure we never cause a segfault.
        // This does mean that if we set an accessed or dirty flag, this won't be reflected in the physical_frames_dirty list.
        // This seems an acceptable trade-off.
        let addr_base = self.phys_base();
        let phys_pde_addr = page_table_addr as u64 + l1_bits * 4;

        let pde_addr = unsafe { addr_base.add(phys_pde_addr as usize) };
        let mut pde = Pde::from(unsafe { *(pde_addr as *const u32) });

        if !pde.present() {
            // trace!("Walked page tables for address 0x{emu_addr:X}: PDE: {pde:X?}, need #PF");
            return PageWalkResult::Unmapped(PageWalkError::PdeNotPresent {
                pde,
                addr: phys_pde_addr as u32,
            })
        }

        if mark_accessed && !pde.accessed() {
            debug!("Marking as accessed: {pde:X?} @ 0x{phys_pde_addr:X} for address {emu_addr:X}");
            pde.set_accessed(true);
            unsafe {
                (pde_addr as *mut u32).write(pde.into());
            }
        }

        // Big 4MB pages -- lowest 22 bits used as offset
        if pde.big_page() && self.page_size_extension.load(Ordering::Relaxed) {
            let pde = BigPde::from(u32::from(pde));
            let offset = emu_addr & 0x3fffff;
            let phys_addr = pde.phys_base_addr() | offset as u64;

            trace!("Big page (4MiB) for address 0x{emu_addr:X} at 0x{phys_pde_addr:X}: {pde:X?}");

            return PageWalkResult::PhysAddr {
                addr: phys_addr,
                writable: pde.writeable(),
                user_accessible: pde.user_accessible(),
                dirty: pde.dirty(),
                mark_dirty: DirtyMarker {
                    addr: pde_addr as *mut u32,
                },
                big_page: BigPage::Big4Mb,
                global: pde.global(),
            }
        }

        let phys_pte_addr = pde.pt_base_addr() as u64 + l2_bits * 4;
        let pte_addr = unsafe { addr_base.add(phys_pte_addr as usize) };

        {
            let phys_map = self.phys_map.lock().unwrap();
            debug_assert!(
                matches!(phys_map.lookup(phys_pde_addr).0, EntryRef::Default),
                "page table lookup: PDE must be located in physical memory"
            );
            debug_assert!(
                matches!(phys_map.lookup(phys_pte_addr).0, EntryRef::Default),
                "page table lookup: PTE must be located in physical memory"
            );
        }

        let mut pte = Pte::from(unsafe { *(pte_addr as *const u32) });
        if !pte.present() {
            // trace!("Walked page tables for address 0x{emu_addr:X}: PDE: {pde:X?}, PTE: {pte:X?}, need #PF");
            return PageWalkResult::Unmapped(PageWalkError::PteNotPresent {
                pde,
                pte,
                addr: phys_pte_addr as u32,
            })
        }

        if mark_accessed && !pte.accessed() {
            debug!("Marking as accessed: {pte:X?} @ 0x{phys_pte_addr:X} for address {emu_addr:X}");
            pte.set_accessed(true);
            unsafe {
                (pte_addr as *mut u32).write(pte.into());
            }
        }

        let phys_addr = pte.phys_base_addr() as u64 | (emu_addr & 0xfff) as u64;
        trace!(
            "Walked pages: 0x{phys_pde_addr:X} = {pde:X?}, 0x{phys_pte_addr:X} = {pte:X?} to map {emu_addr:X} -> {phys_addr:X?}"
        );

        PageWalkResult::PhysAddr {
            addr: phys_addr,
            writable: pde.writeable() && pte.writeable(),
            user_accessible: pde.user_accessible() && pte.user_accessible(),
            dirty: pte.dirty(),
            mark_dirty: DirtyMarker {
                addr: pte_addr as *mut u32,
            },
            big_page: BigPage::Normal4Kb,
            global: pte.global(),
        }
    }

    fn resolve_access_address(&self, emu_addr: u32, is_write: bool, is_user: bool) -> Result<ResolvedAccess, Exception> {
        Ok(if !self.paging_enabled.load(Ordering::Relaxed) {
            let phys_frame_dirty = self.phys_frame_is_dirty(emu_addr as u64);
            ResolvedAccess {
                emu_addr,
                phys_addr: emu_addr as u64,
                writable: is_write || phys_frame_dirty,
                phys_frame_needs_dirty_mark: is_write && !phys_frame_dirty,
                user_accessible: true,
            }
        } else {
            // trace!("Resolving linear address: {base:X}");
            match self.page_walk(emu_addr, true) {
                // TODO: Should we ever raise a general error directly, or should we just keep stacking page faults?
                PageWalkResult::Unmapped(e) => {
                    warn!("Tried to access unmapped memory for linear address 0x{emu_addr:X}: {e:#X?}");
                    return Err(Exception::PageFault {
                        code: PageFaultCode::from_normal_access(false, is_write, is_user, false),
                        address: emu_addr,
                    })
                },
                // TODO: Page walk result for when reserved bits are set
                PageWalkResult::PhysAddr {
                    addr,
                    writable,
                    user_accessible,
                    mut dirty,
                    mark_dirty,
                    ..
                } => {
                    let system_write_protect = self.system_write_protect.load(Ordering::Relaxed);
                    let always_allow_write = !is_user && !system_write_protect;
                    let writeable = writable || always_allow_write;

                    // TODO: If we fail the write at this point, the page will still have been marked dirty. Can we delay marking the page dirty?
                    if !writeable && is_write {
                        warn!(
                            "Address {emu_addr:X} is not writeable, system_write_protect={system_write_protect}, is_user={is_user}"
                        );
                        return Err(Exception::PageFault {
                            code: PageFaultCode::from_normal_access(true, is_write, is_user, false),
                            address: emu_addr,
                        })
                    }

                    if !user_accessible && is_user {
                        warn!("Address {emu_addr:X} is not user-accessible");
                        return Err(Exception::PageFault {
                            code: PageFaultCode::from_normal_access(true, is_write, is_user, false),
                            address: emu_addr,
                        })
                    }

                    if is_write && !dirty {
                        debug!("Marking PTE for {addr:X} as dirty");
                        mark_dirty.mark_dirty();
                        dirty = true;
                    }

                    // We should only mark the page as writeable if we do not need to update any 'dirty' flag.
                    let phys_frame_dirty = self.phys_frame_is_dirty(addr);
                    let writeable = is_write || (writeable && dirty && phys_frame_dirty);

                    ResolvedAccess {
                        emu_addr,
                        phys_addr: addr,
                        writable: writeable,
                        phys_frame_needs_dirty_mark: is_write && !phys_frame_dirty,
                        user_accessible,
                    }
                },
            }
        })
    }

    fn map_address_before_access(
        &self, emu_addr: u32, phys_addr: u64, shm: &Shm, phys_addr_to_shm_delta: i64, range: Range<u64>,
    ) {
        let page_addr = phys_addr & !0xfff;
        if range.start <= page_addr && range.end >= page_addr + 4096 {
            let logical_addr = emu_addr & !0xfff;
            let shm_offset = page_addr.wrapping_add(phys_addr_to_shm_delta as u64) as u32 as u64;

            let mut current_mappings = self.current_mappings.lock().unwrap();
            let current = current_mappings.entry(logical_addr).or_insert(u64::MAX);
            if *current != shm_offset {
                info!(
                    "Mapping logical address 0x{logical_addr:X} to shared memory at offset 0x{shm_offset:X} (was: 0x{current:X})"
                );
                *current = shm_offset;

                let addr = NonZeroUsize::new(self.logical_base() as usize + logical_addr as usize).unwrap();
                let length = NonZeroUsize::new(4096).unwrap();

                unsafe {
                    // Call with MAP_POPULATE to immediately load the page into the page table.
                    // By default, Linux only updates the page table when a page fault occurs.
                    // But we already know the page fault will occur immediately after returning from this function.
                    // Therefore, it makes no sense to have Linux wait for another page fault before updating the page table.
                    let result_addr = mmap(
                        Some(addr),
                        length,
                        ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                        MapFlags::MAP_SHARED | MapFlags::MAP_FIXED | MapFlags::MAP_POPULATE,
                        shm,
                        shm_offset as i64,
                    )
                    .expect("should be able to map shared memory");

                    assert_eq!(result_addr.as_ptr() as usize, addr.get());
                }

                self.mmap_count.fetch_add(1, Ordering::SeqCst);
            }
        } else {
            panic!("Cannot map non-page aligned memory regions")
        }
    }

    fn resolve_physical_address(&self, addr: u64) -> Option<u64> {
        let addr = if self.a20_enabled.load(Ordering::Relaxed) {
            addr
        } else {
            addr & 0xfffff
        };

        if addr < self.physical_memory.len() { Some(addr) } else { None }
    }

    #[inline(never)]
    fn handle_trapped_linear_memory_read<D: MemoryData>(&self, access: ResolvedAccess, mmio: &mut impl Mmio) -> Option<D> {
        let phys_map = self.phys_map.lock().unwrap();
        let (entry, range) = phys_map.lookup(access.phys_addr);
        match entry {
            EntryRef::Mmio {
                id,
            } => Some(mmio.read_mem(id, (access.phys_addr - range.start) as u32)),
            EntryRef::Shm {
                shm,
                phys_addr_to_shm_delta,
                ..
            } => {
                assert!(!access.phys_frame_needs_dirty_mark);

                let metadata = MetadataBits::new()
                    .trap_on_userspace_access(!access.user_accessible)
                    .trap_on_write(!access.writable);

                self.metadata_bits().set(access.emu_addr, metadata);
                self.map_address_before_access(access.emu_addr, access.phys_addr, shm, phys_addr_to_shm_delta, range);

                None
            },
            EntryRef::Default => {
                if let Some(actual_addr) = self.resolve_physical_address(access.phys_addr) {
                    assert!(!access.phys_frame_needs_dirty_mark);

                    let metadata = MetadataBits::new()
                        .trap_on_userspace_access(!access.user_accessible)
                        .trap_on_write(!access.writable);

                    self.metadata_bits().set(access.emu_addr, metadata);
                    // If A20 is disabled, this will ensure the bottom 1MiB of memory gets mapped again above 0x100000
                    let phys_addr_to_shm_delta = actual_addr.wrapping_sub(access.phys_addr) as i64;
                    self.map_address_before_access(
                        access.emu_addr,
                        access.phys_addr,
                        &self.physical_memory,
                        phys_addr_to_shm_delta,
                        range,
                    );

                    None
                } else {
                    Some(D::MAX)
                }
            },
        }
    }

    #[inline(never)]
    fn handle_trapped_linear_memory_write<D: MemoryData>(&self, access: ResolvedAccess, val: D, mmio: &mut impl Mmio) -> bool {
        let locked = self.phys_map.lock().unwrap();
        let (entry, range) = locked.lookup(access.phys_addr);
        // trace!("Memory access will be directed to {entry:?} in range {range:X?}");
        match entry {
            EntryRef::Mmio {
                id,
            } => {
                mmio.write_mem(id, (access.phys_addr - range.start) as u32, val);
                true
            },
            EntryRef::Shm {
                shm,
                phys_addr_to_shm_delta,
                watcher,
                writable,
            } => {
                let tmp;
                let shm = if access.phys_frame_needs_dirty_mark {
                    // If not writable, we silently ignore the write
                    if !writable {
                        return true
                    }

                    tmp = shm.clone();
                    let watcher = watcher.cloned();

                    // Drop to prevent deadlock when advise_memory_dirty ends up doing memory operations
                    // For example, it might try to read physical memory.
                    drop(locked);

                    let offset = access.phys_addr.wrapping_add(phys_addr_to_shm_delta as u64);
                    if let Some(watcher) = watcher {
                        watcher.notify_dirty(offset);
                    }

                    match mmio.advise_memory_dirty(PhysAddr::new(access.phys_addr as u32), D::NUM_BYTES as u8) {
                        MarkDirtyAdvice::DirtyOk => {
                            self.mark_phys_frame_dirty(access.phys_addr, mmio);
                        },
                        MarkDirtyAdvice::DoNotMark => {
                            // If we are advised to not mark the frame dirty, we manually write the data.
                            // println!("Writing data to physical address 0x{:X} by writing to Shm at offset 0x{:X}", access.phys_addr, offset);
                            tmp.view().write_slice(offset as u32, val.to_bytes().as_ref());

                            // Return true to signal that the data has already been written.
                            return true
                        },
                    }

                    &tmp
                } else {
                    shm
                };

                let metadata = MetadataBits::new()
                    .trap_on_userspace_access(!access.user_accessible)
                    .trap_on_write(!access.writable);

                self.metadata_bits().set(access.emu_addr, metadata);
                self.map_address_before_access(access.emu_addr, access.phys_addr, shm, phys_addr_to_shm_delta, range);

                false
            },
            EntryRef::Default => {
                if access.phys_addr < self.physical_memory.len() {
                    // Drop to prevent deadlock when advise_memory_dirty ends up doing memory operations
                    // For example, it might try to read physical memory.
                    drop(locked);

                    if access.phys_frame_needs_dirty_mark {
                        // TODO: Only needs to happen for physical memory
                        match mmio.advise_memory_dirty(PhysAddr::new(access.phys_addr as u32), D::NUM_BYTES as u8) {
                            MarkDirtyAdvice::DirtyOk => {
                                self.mark_phys_frame_dirty(access.phys_addr, mmio);
                            },
                            MarkDirtyAdvice::DoNotMark => {
                                // If we are advised to not mark the frame dirty, we manually write the data.
                                let offset = access.phys_addr;
                                // println!("Writing data to physical address 0x{:X} by writing to Shm at offset 0x{:X}", access.phys_addr, offset);
                                self.physical_memory
                                    .view()
                                    .write_slice(offset as u32, val.to_bytes().as_ref());

                                // Return true to signal that the data has already been written.
                                return true
                            },
                        }
                    }

                    // TODO: wrap at 0x100000 if a20 line is unset

                    let metadata = MetadataBits::new()
                        .trap_on_userspace_access(!access.user_accessible)
                        .trap_on_write(!access.writable);

                    self.metadata_bits().set(access.emu_addr, metadata);
                    self.map_address_before_access(access.emu_addr, access.phys_addr, &self.physical_memory, 0, range);

                    false
                } else {
                    true
                }
            },
        }
    }

    #[inline(always)]
    fn should_trap_linear_access(&self, addr: u32, is_write: bool, is_user: bool) -> bool {
        let test = const { MetadataTest::new().require_present() };

        let test = if is_write { test.require_writable() } else { test };

        let test = if is_user {
            test.require_accessible_from_userspace()
        } else {
            test
        };

        self.metadata_bits().get_for_address(addr).should_trap(test)
    }

    #[inline(always)]
    pub fn read<D: MemoryData>(&self, addr: u32, userspace: bool, mmio: &mut impl Mmio) -> Result<D, Exception> {
        self.fast().read(addr, userspace, mmio)
    }

    #[inline(always)]
    pub fn write<D: MemoryData>(&self, addr: u32, userspace: bool, val: D, mmio: &mut impl Mmio) -> Result<(), Exception> {
        self.fast().write(addr, userspace, val, mmio)
    }

    #[inline(never)]
    fn read_slow<D: MemoryData>(&self, addr: u32, userspace: bool, mmio: &mut impl Mmio) -> Result<D, Exception> {
        if !self.should_trap_linear_access(addr, false, userspace)
            && !self.should_trap_linear_access(addr.wrapping_add(D::NUM_BYTES as u32 - 1), false, userspace)
        {
            self.num_unaligned_reads.fetch_add(1, Ordering::Relaxed);
            unsafe { Ok(D::read_from_unaligned_pointer(self.logical_base().add(addr as usize))) }
        } else if (addr & 0xfff) + D::NUM_BYTES as u32 - 1 < 4096 {
            self.num_trapped_reads.fetch_add(1, Ordering::Relaxed);
            // If the access does not wrap, we can resolve the access in one go
            let access = self.resolve_access_address(addr, false, userspace)?;
            if let Some(result) = self.handle_trapped_linear_memory_read(access, mmio) {
                return Ok(result)
            }

            unsafe { Ok(D::read_from_unaligned_pointer(self.logical_base().add(addr as usize))) }
        } else {
            self.num_page_bounds_crossing_reads.fetch_add(1, Ordering::Relaxed);
            // If the access wraps, we fall back to u8 reads
            // TODO: more efficient reads
            let mut buf = [0; 16];
            self.read_slice(addr, &mut buf[..D::NUM_BYTES], userspace, mmio)?;
            Ok(D::from_bytes(&buf[..D::NUM_BYTES]))
        }
    }

    #[inline]
    pub fn prepare_write<D: MemoryData>(&self, addr: u32, userspace: bool, val: D) -> Result<PreparedWrite<D>, Exception> {
        if (addr & 0xfff) + D::NUM_BYTES as u32 - 1 < 4096 {
            Ok(PreparedWrite {
                split: false,
                resolved: if self.should_trap_linear_access(addr, true, userspace) {
                    [Some(self.resolve_access_address(addr, true, userspace)?), None]
                } else {
                    [None, None]
                },
                val,
                addr,
            })
        } else {
            let next_page = (addr + 0xfff) & !0xfff;
            let resolved = [
                if self.should_trap_linear_access(addr, true, userspace) {
                    Some(self.resolve_access_address(addr, true, userspace)?)
                } else {
                    None
                },
                if self.should_trap_linear_access(next_page, true, userspace) {
                    Some(self.resolve_access_address(next_page, true, userspace)?)
                } else {
                    None
                },
            ];

            Ok(PreparedWrite {
                split: true,
                resolved,
                val,
                addr,
            })
        }
    }

    #[inline]
    pub fn execute_prepared_write<D: MemoryData>(&self, prepared: PreparedWrite<D>, mmio: &mut impl Mmio) {
        if prepared.split {
            let bytes = prepared.val.to_bytes();
            let bytes = bytes.as_ref();

            match prepared.resolved {
                [None, None] => {
                    for (n, b) in bytes.iter().enumerate() {
                        unsafe {
                            self.fast().write_unchecked(prepared.addr + n as u32, *b);
                        }
                    }
                },
                [a, b] => {
                    for (n, byte) in bytes.iter().enumerate() {
                        let resolved = if ((prepared.addr & 0xfff) + n as u32) < 4096 { &a } else { &b };

                        if let Some(access) = resolved
                            && self.handle_trapped_linear_memory_write(access.with_offset(n), *byte, mmio)
                        {
                            continue
                        }

                        unsafe {
                            self.fast().write_unchecked(prepared.addr + n as u32, *byte);
                        }
                    }
                },
            }
        } else {
            if let [Some(access), _] = prepared.resolved
                && self.handle_trapped_linear_memory_write(access, prepared.val, mmio)
            {
                return
            }

            unsafe {
                self.fast().write_unchecked(prepared.addr, prepared.val);
            }
        }
    }

    #[inline(never)]
    fn write_slow<D: MemoryData>(&self, addr: u32, userspace: bool, val: D, mmio: &mut impl Mmio) -> Result<(), Exception> {
        self.slow_writes.fetch_add(1, Ordering::Relaxed);
        if !self.should_trap_linear_access(addr, true, userspace)
            && !self.should_trap_linear_access(addr.wrapping_add(D::NUM_BYTES as u32 - 1), true, userspace)
        {
            unsafe {
                self.fast().write_unchecked(addr, val);
            }
        } else {
            let write = self.prepare_write(addr, userspace, val)?;
            self.execute_prepared_write(write, mmio);
        }

        Ok(())
    }

    #[inline]
    pub fn write_slice(&self, addr: u32, bytes: &[u8], userspace: bool, mmio: &mut impl Mmio) -> Result<(), Exception> {
        for (n, &b) in bytes.iter().enumerate() {
            self.write(addr + n as u32, userspace, b, mmio)?;
        }

        Ok(())
    }

    #[inline(always)]
    pub fn read_slice(&self, addr: u32, bytes: &mut [u8], userspace: bool, mmio: &mut impl Mmio) -> Result<(), Exception> {
        for (n, b) in bytes.iter_mut().enumerate() {
            *b = self.read(addr + n as u32, userspace, mmio)?;
        }

        Ok(())
    }

    #[inline(always)]
    pub fn read_u16(&self, addr: u32, userspace: bool, mmio: &mut impl Mmio) -> Result<u16, Exception> {
        self.read(addr, userspace, mmio)
    }

    #[inline(always)]
    pub fn read_u32(&self, addr: u32, userspace: bool, mmio: &mut impl Mmio) -> Result<u32, Exception> {
        self.read(addr, userspace, mmio)
    }

    #[inline(always)]
    pub fn read_u64(&self, addr: u32, userspace: bool, mmio: &mut impl Mmio) -> Result<u64, Exception> {
        self.read(addr, userspace, mmio)
    }

    #[inline]
    pub fn write_physical_slice(&self, addr: u32, bytes: &[u8], mmio: &mut impl Mmio) -> Result<(), Exception> {
        for page_offset in (0..bytes.len()).step_by(4096) {
            self.mark_phys_frame_dirty(addr.wrapping_add(page_offset as u32) as u64, mmio);
        }

        let phys_map = self.phys_map.lock().unwrap();
        for (n, &b) in bytes.iter().enumerate() {
            let (entry, _range) = phys_map.lookup(addr as u64);
            match entry {
                // TODO: Handle MMIO
                EntryRef::Mmio {
                    id: _,
                } => (),
                EntryRef::Shm {
                    ..
                } => unsafe {
                    // TODO: This doesn't seem safe.
                    self.phys_base().add((addr + n as u32) as usize).write(b);
                },
                EntryRef::Default => {
                    if let Some(addr) = self.resolve_physical_address((addr + n as u32) as u64) {
                        unsafe {
                            self.phys_base().add(addr as usize).write(b);
                        }
                    }
                },
            }
        }

        Ok(())
    }

    #[inline]
    pub fn read_physical_slice(&self, addr: u32, bytes: &mut [u8], mmio: &mut impl Mmio) -> Result<(), Exception> {
        let phys_map = self.phys_map.lock().unwrap();
        for (n, b) in bytes.iter_mut().enumerate() {
            let (entry, _range) = phys_map.lookup(addr as u64);
            *b = match entry {
                EntryRef::Mmio {
                    id,
                } => mmio.read_mem(id, addr + n as u32),
                EntryRef::Shm {
                    ..
                } => unsafe { self.phys_base().add((addr + n as u32) as usize).read() },
                EntryRef::Default => {
                    if let Some(addr) = self.resolve_physical_address((addr + n as u32) as u64) {
                        unsafe { self.phys_base().add(addr as usize).read() }
                    } else {
                        0xff
                    }
                },
            }
        }

        Ok(())
    }

    #[inline]
    pub fn read_physical_slice_no_mmio(&self, addr: u32, bytes: &mut [u8]) {
        let phys_map = self.phys_map.lock().unwrap();
        for (n, b) in bytes.iter_mut().enumerate() {
            let (entry, _range) = phys_map.lookup(addr as u64);
            *b = match entry {
                EntryRef::Mmio {
                    ..
                } => 0xff,
                EntryRef::Shm {
                    ..
                } => unsafe { self.phys_base().add((addr + n as u32) as usize).read() },
                EntryRef::Default => {
                    if let Some(addr) = self.resolve_physical_address((addr + n as u32) as u64) {
                        unsafe { self.phys_base().add(addr as usize).read() }
                    } else {
                        0xff
                    }
                },
            }
        }
    }

    pub fn num_altered_mappings(&self) -> u64 {
        0
    }

    pub fn physical_memory(&self) -> Arc<Shm> {
        self.physical_memory.clone()
    }

    pub fn snapshot(&self) -> MemorySnapshot {
        MemorySnapshot {
            phys_mem: self.physical_memory.view().to_vec(),
        }
    }

    pub fn restore(&self, memory: MemorySnapshot) {
        let phys_mem = &self.physical_memory;
        assert_eq!(phys_mem.len(), memory.phys_mem.len() as u64);
        phys_mem.view().write_slice(0, &memory.phys_mem);
    }

    pub fn num_metadata_clears(&self) -> u64 {
        self.metadata_clears.load(Ordering::Relaxed)
    }

    pub fn num_unaligned_reads(&self) -> u64 {
        self.num_unaligned_reads.load(Ordering::Relaxed)
    }

    pub fn num_trapped_reads(&self) -> u64 {
        self.num_trapped_reads.load(Ordering::Relaxed)
    }

    pub fn num_page_bounds_crossing_reads(&self) -> u64 {
        self.num_page_bounds_crossing_reads.load(Ordering::Relaxed)
    }

    pub fn num_slow_writes(&self) -> u64 {
        self.slow_writes.load(Ordering::Relaxed)
    }

    pub fn a20_line(&self) -> bool {
        self.a20_enabled.load(Ordering::Relaxed)
    }
}

unsafe impl<T: Send> Send for FastMem32<T> {}
unsafe impl<T: Sync> Sync for FastMem32<T> {}

#[derive(Copy, Clone)]
pub struct FastMem32<T> {
    pub base: *mut u8,
    inner: T,
}

impl<T: AsRef<Mem32>> FastMem32<T> {
    #[inline(always)]
    pub fn new(inner: T) -> Self {
        Self {
            base: inner.as_ref().base,
            inner,
        }
    }

    #[inline(always)]
    pub fn as_inner(&self) -> &T {
        &self.inner
    }

    #[inline(always)]
    fn metadata_bits(&self) -> MetadataMap {
        unsafe { MetadataMap::from_ptr(self.base.sub(METADATA_SIZE as usize)) }
    }

    #[inline(always)]
    fn logical_base(&self) -> *mut u8 {
        unsafe { self.base.add(MAPPING_KIND_LOGICAL as usize) }
    }

    #[inline(always)]
    fn phys_base(&self) -> *mut u8 {
        unsafe { self.base.add(MAPPING_KIND_PHYSICAL as usize) }
    }

    /// Returns true if the access *may* need to be trapped.
    /// Also returns true if the access is unaligned, even though this does not necessarily require trapping.
    #[inline(always)]
    fn multibyte_access_can_use_fast_path<D: MemoryData>(&self, addr: u32, is_write: bool, is_user: bool) -> bool {
        let test = const { MetadataTest::new().require_present() };

        let test = if is_write { test.require_writable() } else { test };

        let test = if is_user {
            test.require_accessible_from_userspace()
        } else {
            test
        };

        !self.metadata_bits().get_for_address(addr).should_trap(test)
            && (addr.is_multiple_of(D::NUM_BYTES as u32)
                || !self
                    .metadata_bits()
                    .get_for_address(addr + D::NUM_BYTES as u32 - 1)
                    .should_trap(test))
    }

    #[inline(always)]
    pub fn read<D: MemoryData>(&self, addr: u32, userspace: bool, mmio: &mut impl Mmio) -> Result<D, Exception> {
        if self.multibyte_access_can_use_fast_path::<D>(addr, false, userspace) {
            // Fast path if we can directly write the value
            unsafe { Ok(D::read_from_unaligned_pointer(self.logical_base().add(addr as usize))) }
        } else {
            // Slow path in a separate function that isn't inlined.
            // This way we don't pay the cost of pushing/popping extra registers that are only needed in this path.
            self.inner.as_ref().read_slow(addr, userspace, mmio)
        }
    }

    #[inline(always)]
    pub unsafe fn write_unchecked<D: MemoryData>(&self, addr: u32, val: D) {
        unsafe {
            val.write_to_unaligned_pointer(self.logical_base().add(addr as usize));
        }
    }

    #[inline(always)]
    pub fn write<D: MemoryData>(&self, addr: u32, userspace: bool, val: D, mmio: &mut impl Mmio) -> Result<(), Exception> {
        if self.multibyte_access_can_use_fast_path::<D>(addr, true, userspace) {
            // Fast path if we can directly write the value
            unsafe { self.write_unchecked::<D>(addr, val) }
        } else {
            self.inner.as_ref().write_slow(addr, userspace, val, mmio)?;
        }

        Ok(())
    }
}

pub struct PreparedWrite<D> {
    resolved: [Option<ResolvedAccess>; 2],
    val: D,
    addr: u32,
    split: bool,
}

#[derive(Clone, Debug)]
struct ResolvedAccess {
    emu_addr: u32,
    phys_addr: u64,
    writable: bool,
    phys_frame_needs_dirty_mark: bool,
    user_accessible: bool,
}
impl ResolvedAccess {
    fn with_offset(&self, n: usize) -> ResolvedAccess {
        // TODO: What invariants do we need to uphold here?
        assert!(((self.phys_addr & 0xfff) + n as u64) < 4096);
        Self {
            emu_addr: self.emu_addr.wrapping_add(n as u32),
            phys_addr: self.phys_addr.wrapping_add(n as u64),
            ..self.clone()
        }
    }
}

impl MemoryData for u8 {
    const NUM_BYTES: usize = 1;
    const MAX: Self = u8::MAX;

    fn from_bytes(bytes: &[u8]) -> Self {
        bytes[0]
    }

    fn to_bytes(self) -> impl AsRef<[u8]> {
        [self]
    }

    fn from_u32_with_offset(offset: impl Into<u32>, val: u32) -> Self {
        let shift = offset.into() * 8;
        (val >> shift) as u8
    }

    fn into_u32_exact(self) -> Option<u32> {
        None
    }

    fn from_u16_with_offset(offset: impl Into<u16>, val: u16) -> Self {
        let shift = offset.into() * 8;
        (val >> shift) as u8
    }

    fn into_u16_exact(self) -> Option<u16> {
        None
    }

    unsafe fn read_from_unaligned_pointer(ptr: *mut u8) -> Self {
        unsafe { ptr.read_unaligned() }
    }

    unsafe fn write_to_unaligned_pointer(self, ptr: *mut u8) {
        unsafe { ptr.write_unaligned(self) }
    }
}

impl MemoryData for u16 {
    const NUM_BYTES: usize = 2;
    const MAX: Self = u16::MAX;

    fn from_bytes(bytes: &[u8]) -> Self {
        u16::from_le_bytes(bytes.try_into().unwrap())
    }

    fn to_bytes(self) -> impl AsRef<[u8]> {
        u16::to_le_bytes(self)
    }

    fn from_u32_with_offset(offset: impl Into<u32>, val: u32) -> Self {
        let shift = offset.into() * 8;
        (val >> shift) as u16
    }

    fn into_u32_exact(self) -> Option<u32> {
        None
    }

    fn from_u16_with_offset(offset: impl Into<u16>, val: u16) -> Self {
        let shift = offset.into() * 8;
        val >> shift
    }

    fn into_u16_exact(self) -> Option<u16> {
        Some(self)
    }

    unsafe fn read_from_unaligned_pointer(ptr: *mut u8) -> Self {
        unsafe { (ptr as *mut u16).read_unaligned() }
    }

    unsafe fn write_to_unaligned_pointer(self, ptr: *mut u8) {
        unsafe { (ptr as *mut u16).write_unaligned(self) }
    }
}

impl MemoryData for u32 {
    const NUM_BYTES: usize = 4;
    const MAX: Self = u32::MAX;

    fn from_bytes(bytes: &[u8]) -> Self {
        u32::from_le_bytes(bytes.try_into().unwrap())
    }

    fn to_bytes(self) -> impl AsRef<[u8]> {
        u32::to_le_bytes(self)
    }

    fn from_u32_with_offset(offset: impl Into<u32>, val: u32) -> Self {
        let shift = offset.into() * 8;
        val >> shift
    }

    fn into_u32_exact(self) -> Option<u32> {
        Some(self)
    }

    fn from_u16_with_offset(offset: impl Into<u16>, val: u16) -> Self {
        let shift = offset.into() * 8;
        (val >> shift) as u32
    }

    fn into_u16_exact(self) -> Option<u16> {
        None
    }

    unsafe fn read_from_unaligned_pointer(ptr: *mut u8) -> Self {
        unsafe { (ptr as *mut u32).read_unaligned() }
    }

    unsafe fn write_to_unaligned_pointer(self, ptr: *mut u8) {
        unsafe { (ptr as *mut u32).write_unaligned(self) }
    }
}

impl MemoryData for u64 {
    const NUM_BYTES: usize = 8;
    const MAX: Self = u64::MAX;

    fn from_bytes(bytes: &[u8]) -> Self {
        u64::from_le_bytes(bytes.try_into().unwrap())
    }

    fn to_bytes(self) -> impl AsRef<[u8]> {
        u64::to_le_bytes(self)
    }

    fn from_u32_with_offset(_offset: impl Into<u32>, _val: u32) -> Self {
        error!("TODO: 64-bit read of 32-bit value must be split into two reads");
        Self::MAX
    }

    fn into_u32_exact(self) -> Option<u32> {
        None
    }

    fn from_u16_with_offset(offset: impl Into<u16>, val: u16) -> Self {
        let shift = offset.into() * 8;
        (val >> shift) as u64
    }

    fn into_u16_exact(self) -> Option<u16> {
        None
    }

    unsafe fn read_from_unaligned_pointer(ptr: *mut u8) -> Self {
        unsafe { (ptr as *mut u64).read_unaligned() }
    }

    unsafe fn write_to_unaligned_pointer(self, ptr: *mut u8) {
        unsafe { (ptr as *mut u64).write_unaligned(self) }
    }
}


impl MemoryData for u128 {
    const NUM_BYTES: usize = 16;
    const MAX: Self = u128::MAX;

    fn from_bytes(bytes: &[u8]) -> Self {
        u128::from_le_bytes(bytes.try_into().unwrap())
    }

    fn to_bytes(self) -> impl AsRef<[u8]> {
        u128::to_le_bytes(self)
    }

    fn from_u32_with_offset(_offset: impl Into<u32>, _val: u32) -> Self {
        error!("TODO: 128-bit read of 32-bit value must be split into two reads");
        Self::MAX
    }

    fn into_u32_exact(self) -> Option<u32> {
        None
    }

    fn from_u16_with_offset(offset: impl Into<u16>, val: u16) -> Self {
        let shift = offset.into() * 8;
        (val >> shift) as u128
    }

    fn into_u16_exact(self) -> Option<u16> {
        None
    }

    unsafe fn read_from_unaligned_pointer(ptr: *mut u8) -> Self {
        unsafe { (ptr as *mut u128).read_unaligned() }
    }

    unsafe fn write_to_unaligned_pointer(self, ptr: *mut u8) {
        unsafe { (ptr as *mut u128).write_unaligned(self) }
    }
}

impl Mmio for () {
    fn read_mem<D: MemoryData>(&mut self, _id: MmioId, _address: u32) -> D {
        D::MAX
    }

    fn write_mem<D: MemoryData>(&mut self, _id: MmioId, _address: u32, _val: D) {}

    fn notify_memory_dirty(&mut self, _phys_frame_index: PhysFrameIndex, _memory: &Mem32) {}

    fn advise_memory_dirty(&mut self, _addr: PhysAddr, _len: u8) -> MarkDirtyAdvice {
        MarkDirtyAdvice::DirtyOk
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{Mem32, Shm};

    #[test]
    fn readback() {
        let mem = Mem32::new(Arc::new(Shm::new("mem", 1 << 12)));

        for _ in 0..10 {
            for n in 0..64 << 8 {
                mem.read::<u8>(0x1000 * n + 0x123, false, &mut ()).unwrap();
                mem.write::<u8>(0x1000 * n + 0x123, false, 5, &mut ()).unwrap();
                assert_eq!(mem.read::<u8>(0x1000 * n + 0x123, false, &mut ()).unwrap(), 5);
            }

            mem.invalidate_all_pages();
        }
    }

    #[test]
    fn invaliate_all_pages() {
        let mem = Mem32::new(Arc::new(Shm::new("mem", 1 << 12)));
        mem.invalidate_all_pages();
    }

    #[test]
    fn unmapped_is_all_ones() {
        let mem = Mem32::new(Arc::new(Shm::new("mem", 1 << 12)));
        let mut buf = [0; 16];
        mem.read_physical_slice(0x8000_0000, &mut buf, &mut ()).unwrap();
        assert_eq!(buf, [0xff; 16]);
    }
}
