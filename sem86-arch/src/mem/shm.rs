use std::ffi::{CString, c_void};
use std::num::NonZeroUsize;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::ptr::NonNull;

use nix::libc::{ftruncate64, memfd_create};
use nix::sys::mman::{MapFlags, ProtFlags, mmap, munmap};

#[derive(Debug)]
pub struct Shm {
    fd: OwnedFd,
    size: usize,
    view: ShmView,
}

impl Eq for Shm {}

impl PartialEq for Shm {
    fn eq(&self, other: &Self) -> bool {
        self.fd.as_raw_fd() == other.fd.as_raw_fd() && self.size == other.size
    }
}

impl Shm {
    pub fn new(name: &str, size: usize) -> Shm {
        let fd = unsafe {
            let name = CString::new(name).unwrap();

            let fd = memfd_create(name.as_ptr(), 0);
            assert!(fd != -1, "memfd_create failed");

            let ret = ftruncate64(fd, size as i64);
            assert_eq!(ret, 0, "ftruncate failed");

            OwnedFd::from_raw_fd(fd)
        };

        let view = Self::make_view(size, &fd);
        let result = Self {
            fd,
            size,
            view,
        };

        // Clear memory
        unsafe {
            result.view.base.write_bytes(0, size);
        }

        result
    }

    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> u64 {
        self.size as u64
    }

    fn make_view(size: usize, fd: &OwnedFd) -> ShmView {
        let ptr = unsafe {
            mmap(
                None,
                NonZeroUsize::new(size).unwrap(),
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_SHARED,
                fd,
                0,
            )
            .expect("should be able to map shm into memory")
        };

        ShmView {
            base: ptr.as_ptr() as *mut u8,
            size,
        }
    }

    pub fn view(&self) -> &ShmView {
        &self.view
    }
}

impl AsFd for Shm {
    fn as_fd(&self) -> std::os::unix::prelude::BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

#[derive(Debug)]
pub struct ShmView {
    base: *mut u8,
    size: usize,
}

unsafe impl Send for ShmView {}
unsafe impl Sync for ShmView {}

impl ShmView {
    pub fn read_slice(&self, addr: u32, slice: &mut [u8]) {
        assert!(
            (slice.len() + addr as usize) <= self.size,
            "offset {addr} out of bounds of {}-byte memory view",
            self.size
        );

        // SAFETY: We can safely cast u8s to u64s.
        let (before, middle, after) = unsafe { slice.align_to_mut::<u64>() };
        if !before.is_empty() {
            self.read_u8s(addr, before);
        }

        let middle_addr = addr + before.len() as u32;
        self.read_u64s(middle_addr, middle);

        self.read_u8s(middle_addr + middle.len() as u32 * 8, after);
    }

    fn read_u8s(&self, addr: u32, slice: &mut [u8]) {
        assert!(
            (slice.len() + addr as usize) <= self.size,
            "offset {addr} out of bounds of {}-byte memory view",
            self.size
        );

        for (index, b) in slice.iter_mut().enumerate() {
            *b = unsafe { self.base.add(addr as usize + index).read_volatile() }
        }
    }

    fn read_u64s(&self, addr: u32, slice: &mut [u64]) {
        assert!(
            (slice.len() * 8 + addr as usize) <= self.size,
            "offset {addr} out of bounds of {}-byte memory view",
            self.size
        );
        if addr.is_multiple_of(8) {
            for (index, b) in slice.iter_mut().enumerate() {
                *b = unsafe { (self.base.add(addr as usize + index * 8) as *mut u64).read_volatile() }
            }
        } else {
            self.read_u8s(addr, bytemuck::cast_slice_mut(slice));
        }
    }

    pub fn write_slice(&self, addr: u32, slice: &[u8]) {
        assert!(
            (slice.len() + addr as usize) <= self.size,
            "offset {addr} out of bounds of {}-byte memory view",
            self.size
        );
        for (index, b) in slice.iter().enumerate() {
            unsafe { self.base.add(addr as usize + index).write_volatile(*b) }
        }
    }

    pub fn read_byte(&self, addr: u32) -> u8 {
        let mut buf = [0];
        self.read_slice(addr, &mut buf);
        buf[0]
    }

    pub fn write_byte(&self, addr: u32, val: u8) {
        let buf = [val];
        self.write_slice(addr, &buf);
    }

    pub fn to_vec(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(self.size);
        for index in 0..self.size {
            v.push(unsafe { self.base.add(index).read_volatile() });
        }

        v
    }
}

impl Drop for ShmView {
    fn drop(&mut self) {
        unsafe { munmap(NonNull::new(self.base as *mut c_void).unwrap(), self.size).unwrap() }
    }
}
