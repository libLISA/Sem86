use std::ptr::write_bytes;

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MetadataTest(u8);

const METADATA_BIT_TRAP_USER: u8 = 0b0001;
const METADATA_BIT_TRAP_WRITE: u8 = 0b0010;
const METADATA_BIT_TRAP_ACCESS: u8 = 0b0100;
// const METADATA_BIT_TRAP_EXECUTE: u8 = 0b1000;

impl Default for MetadataTest {
    fn default() -> Self {
        Self::new()
    }
}

impl MetadataTest {
    pub const fn new() -> Self {
        Self(0)
    }

    pub const fn require_present(self) -> Self {
        Self(self.0 | METADATA_BIT_TRAP_ACCESS)
    }

    pub const fn require_writable(self) -> Self {
        Self(self.0 | METADATA_BIT_TRAP_WRITE)
    }

    pub const fn require_accessible_from_userspace(self) -> Self {
        Self(self.0 | METADATA_BIT_TRAP_USER)
    }

    pub const fn as_bits(self) -> u8 {
        self.0
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct MetadataBits(u8);

impl Default for MetadataBits {
    fn default() -> Self {
        Self::new()
    }
}

impl MetadataBits {
    pub fn new() -> Self {
        Self(0)
    }

    pub fn all() -> Self {
        Self(0xff)
    }

    /// Returns true if all the requirements in `test` are met, false otherwise.
    #[inline(always)]
    pub fn should_trap(self, test: MetadataTest) -> bool {
        self.0 & test.0 != 0
    }

    pub fn trap_on_write(self, trap: bool) -> Self {
        if trap {
            Self(self.0 | METADATA_BIT_TRAP_WRITE)
        } else {
            Self(self.0 & !METADATA_BIT_TRAP_WRITE)
        }
    }

    pub fn trap_on_userspace_access(self, trap: bool) -> Self {
        if trap {
            Self(self.0 | METADATA_BIT_TRAP_USER)
        } else {
            Self(self.0 & !METADATA_BIT_TRAP_USER)
        }
    }
}

struct PtrBitmap<const LEN: usize> {
    ptr: *mut u64,
}

impl<const LEN: usize> PtrBitmap<LEN> {
    /// # Safety
    /// You must ensure that ptr points to a memory region of `LEN` bytes, aligned to 8 bytes.
    fn from_ptr(ptr: *mut u8) -> Self {
        Self {
            ptr: ptr as *mut u64,
        }
    }

    fn get(&self, n: usize) -> bool {
        let (index, offset) = Self::index(n);
        (unsafe { *self.ptr.add(index) } >> offset) & 1 != 0
    }

    #[inline]
    fn index(x: usize) -> (usize, usize) {
        assert!(x < LEN);
        (x / 64, x % 64)
    }

    /// Sets `self[n]` to true.
    /// Returns true if `self[n]` changed; False if `self[n]` was already true.
    #[inline]
    pub fn set(&self, n: usize) -> bool {
        let (index, offset) = Self::index(n);

        let mask = 1 << offset;
        let old_value = unsafe { *self.ptr.add(index) };
        unsafe {
            *self.ptr.add(index) |= mask;
        }

        old_value & mask == 0
    }

    pub fn clear(&mut self) {
        unsafe {
            self.ptr.write_bytes(0, LEN / 64);
        }
    }
}

/// The number of metadata entries.
/// Equal to the number of pages (2**20).
const METADATA_MAP_SIZE: usize = 1 << 20;

/// Number of entries (=bits) in the modified map.
const MODIFIED_MAP_SIZE: usize = 128;

/// Number of metadata entries for each modified map entry.
const METADATA_MAP_CHUNK_SIZE: usize = METADATA_MAP_SIZE / MODIFIED_MAP_SIZE;

pub struct MetadataMap {
    ptr: *mut u8,
}

impl MetadataMap {
    /// # Safety
    ///
    /// `ptr` should point to a page-aligned 2MiB (`1 << 20` bytes) sized, writable region of memory.
    /// There should be 4096 bytes of scratch space available directly before this region.
    /// This type takes exclusive ownership of the region and assumes no other code writes to it.
    ///
    /// You must call `initialize` after creating the map for the first time.
    /// If you do not, the map will not be cleared and `get_for_address` might return true for clean pages.
    pub unsafe fn from_ptr(ptr: *mut u8) -> Self {
        Self {
            ptr,
        }
    }

    fn modified_bitmap(&self) -> PtrBitmap<MODIFIED_MAP_SIZE> {
        unsafe { PtrBitmap::from_ptr(self.ptr.byte_sub(MODIFIED_MAP_SIZE / 8)) }
    }

    #[inline(always)]
    pub fn get_for_address(&self, addr: u32) -> MetadataBits {
        unsafe { MetadataBits(self.ptr.add(addr as usize >> 12).read()) }
    }

    pub fn set(&self, addr: u32, val: MetadataBits) {
        let offset = addr as usize >> 12;
        self.modified_bitmap().set(offset / METADATA_MAP_CHUNK_SIZE);
        unsafe {
            self.ptr.add(offset).write(val.0);
        }
    }

    /// Must be called when first creating the map.
    pub fn initialize(&self) {
        self.modified_bitmap().clear();
        unsafe {
            write_bytes(self.ptr, 0xff, METADATA_MAP_SIZE);
        }
    }

    /// Enables all trapping.
    pub fn clear(&self) {
        for (index, offset) in (0..METADATA_MAP_SIZE).step_by(METADATA_MAP_CHUNK_SIZE).enumerate() {
            if self.modified_bitmap().get(index) {
                unsafe {
                    write_bytes(self.ptr.byte_add(offset), 0xff, METADATA_MAP_CHUNK_SIZE);
                }
            }
        }

        self.modified_bitmap().clear();
    }

    pub fn enable_for_all(&self, extra_bits: MetadataBits) {
        for (index, offset) in (0..METADATA_MAP_SIZE).step_by(METADATA_MAP_CHUNK_SIZE).enumerate() {
            if self.modified_bitmap().get(index) {
                for n in offset..offset + METADATA_MAP_CHUNK_SIZE {
                    unsafe {
                        let ptr = self.ptr.add(n);
                        *ptr |= extra_bits.0;
                    }
                }
            }
        }
    }
}
