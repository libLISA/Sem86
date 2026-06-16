use std::collections::HashMap;
use std::num::NonZero;
use std::ops::Range;
use std::os::raw::c_void;
use std::ptr::NonNull;

use elf::abi::{R_AARCH64_CALL26, R_AARCH64_JUMP26, R_X86_64_PC32, R_X86_64_PLT32, SHF_ALLOC, SHF_WRITE, SHN_UNDEF, STB_GLOBAL};
use itertools::Itertools;
use log::{error, info};
use nix::sys::mman::{MapFlags, MmapAdvise, ProtFlags, madvise, mmap_anonymous, mprotect};

use crate::codegen::functions::FunctionTable;
use crate::codegen::mm::Object;

pub struct BumpCodeAlloc {
    start: *mut u8,
    end: *mut u8,
    pos: *mut u8,
    plt: *mut u8,
    plt_offset: HashMap<String, u64>,
}

unsafe impl Send for BumpCodeAlloc {}
unsafe impl Sync for BumpCodeAlloc {}

impl BumpCodeAlloc {
    pub fn new(size: usize) -> Self {
        assert!(size.is_multiple_of(4096));

        unsafe {
            let start_addr = mmap_anonymous(
                None,
                NonZero::new(size).unwrap(),
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_ANONYMOUS | MapFlags::MAP_PRIVATE,
            )
            .unwrap();
            match madvise(start_addr, size, MmapAdvise::MADV_HUGEPAGE) {
                Ok(_) => (),
                Err(e) => error!("Unable to madvise huge pages: {e}"),
            }

            let plt = start_addr.as_ptr() as *mut u8;
            let data = start_addr.as_ptr().byte_add(4096) as *mut u8;

            let plt_offset = {
                // Fill the PLT with all symbols that we have
                let plt = plt as *mut u64;
                let mut plt_offset = HashMap::new();
                for (n, (symbol, ptr)) in FunctionTable::<()>::symbols_and_ptrs().enumerate() {
                    #[cfg(target_arch = "aarch64")]
                    {
                        fn movz(rd: u8, imm16: u16, shift: u8) -> u32 {
                            0b110100101 << 23 | ((shift as u32 / 16) << 21) | ((imm16 as u32) << 5) | rd as u32
                        }

                        fn movk(rd: u8, imm16: u16, shift: u8) -> u32 {
                            0b111100101 << 23 | ((shift as u32 / 16) << 21) | ((imm16 as u32) << 5) | rd as u32
                        }

                        plt_offset.insert(symbol.to_string(), n as u64 * 16);
                        let r = plt.add(n * 2) as *mut u32;
                        let ptr_val = ptr as u64;
                        assert!(ptr_val >> 48 == 0);
                        // Move value into x16
                        r.add(0).write(movz(16, ptr_val as u16, 0));
                        r.add(1).write(movk(16, (ptr_val >> 16) as u16, 16));
                        r.add(2).write(movk(16, (ptr_val >> 32) as u16, 32));
                        // BR x16
                        r.add(3).write(0xd61f0000 | (16u32 << 5))
                    }

                    #[cfg(target_arch = "x86_64")]
                    {
                        plt_offset.insert(symbol.to_string(), n as u64 * 16);

                        // JMP [rip + 2]
                        plt.add(n * 2).write(0x0225ff);
                        plt.add(n * 2 + 1).write(ptr as u64);
                    }
                }

                let buf = std::slice::from_raw_parts(plt, 4096);
                info!("PLT data: {:02X}", buf.iter().format(""));

                mprotect(
                    NonNull::new(plt as *mut c_void).unwrap(),
                    4096,
                    ProtFlags::PROT_READ | ProtFlags::PROT_EXEC,
                )
                .unwrap();

                plt_offset
            };

            Self {
                start: data,
                pos: data,
                plt,
                end: start_addr.as_ptr().byte_add(size) as *mut u8,
                plt_offset,
            }
        }
    }

    /// Allocates a new object.
    /// The object is placed at the current cursor position.
    /// A pointer to the first exported function is returned.
    pub fn alloc(&mut self, object: &Object) -> impl Iterator<Item = (String, *mut u8)> {
        self.align(64);
        self.remap_mutable_if_needed();
        let start_addr = self.pos;

        let elf = object.parse();
        let (Some(headers), Some(strtab)) = elf.section_headers_with_strtab().unwrap() else {
            panic!();
        };
        info!(
            "ELF headers: {:?}",
            headers.iter().map(|h| strtab.get(h.sh_name as usize).unwrap()).format(", ")
        );

        let text_index = headers
            .iter()
            .position(|h| strtab.get(h.sh_name as usize).ok() == Some(".text"))
            .unwrap();
        let rela = elf.section_header_by_name(".rela.text").unwrap();
        let (symtab, strtab) = elf.symbol_table().unwrap().expect("should have symbol table");

        let section_addrs = headers
            .iter()
            .map(|header| {
                if header.sh_flags as u32 & SHF_ALLOC != 0 {
                    assert!(header.sh_flags as u32 & SHF_WRITE == 0, "header must not be writable");
                    let name = strtab.get(header.sh_name as usize).unwrap();
                    info!("Mapping header {name} {header:X?}");

                    self.align(header.sh_addralign as usize);
                    let addr = self.pos;
                    self.write_bytes(elf.section_data(&header).unwrap().0);

                    Some(addr)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        if let Some(rela) = rela {
            let addr = section_addrs[text_index].unwrap();
            for rela in elf.section_data_as_relas(&rela).unwrap() {
                let symbol = symtab.get(rela.r_sym as usize).unwrap();
                let name = strtab.get(symbol.st_name as usize).unwrap();
                let code_addr = unsafe { addr.byte_add(rela.r_offset as usize) };
                info!("Relocation {rela:X?} with symbol {symbol:X?} which is {name:?}");

                match rela.r_type {
                    // TODO: Any other AArch64 relocations?
                    R_AARCH64_JUMP26 | R_AARCH64_CALL26 => {
                        let target_addr = if symbol.st_shndx == 0 {
                            let plt_offset = self.plt_offset[name];
                            unsafe { self.plt.byte_add(plt_offset as usize) }
                        } else {
                            let Some(target_addr) = section_addrs[symbol.st_shndx as usize] else {
                                panic!(
                                    "Did not map section {:?} into memory",
                                    strtab
                                        .get(headers.get(symbol.st_shndx as usize).unwrap().sh_name as usize)
                                        .unwrap()
                                );
                            };
                            unsafe { target_addr.add(symbol.st_value as usize) }
                        };

                        let displacement = (target_addr as i64)
                            .checked_add(rela.r_addend)
                            .unwrap()
                            .checked_sub(code_addr as i64)
                            .expect("Overflow calculating displacement");

                        let imm26 = displacement / 4;
                        assert_eq!(displacement % 4, 0, "AArch64 instruction addresses should be 4-byte aligned");
                        assert!(
                            (-(1 << 25)..(1 << 25)).contains(&imm26),
                            "AArch26 relocation should fit in 26-bit signed integer"
                        );

                        info!("Computed AArch64 CALL26 displacement: {} (imm26: {})", displacement, imm26);

                        unsafe {
                            let instr_ptr = code_addr as *mut u32;
                            let old_instr = instr_ptr.read();
                            instr_ptr.write((old_instr & !0x03FF_FFFF) | ((imm26 as u32) & 0x03FF_FFFF));
                        }
                    },
                    R_X86_64_PC32 | R_X86_64_PLT32 => {
                        if symbol.st_shndx == 0 {
                            let Some(&plt_offset) = self.plt_offset.get(name) else {
                                panic!("{name} missing from PLT")
                            };
                            let plt_addr = unsafe { self.plt.byte_add(plt_offset as usize) };

                            let val = (plt_addr as i64)
                                .checked_add(rela.r_addend)
                                .unwrap()
                                .checked_sub(code_addr as i64)
                                .unwrap();
                            let val = i32::try_from(val).unwrap();

                            info!("Computed relocation: {val}");

                            unsafe {
                                (code_addr as *mut i32).write(val);
                            }
                        } else {
                            let target_addr =
                                unsafe { section_addrs[symbol.st_shndx as usize].unwrap().add(symbol.st_value as usize) };

                            let val = (target_addr as i64)
                                .checked_add(rela.r_addend)
                                .unwrap()
                                .checked_sub(code_addr as i64)
                                .unwrap();
                            let val = i32::try_from(val).unwrap();

                            info!("Computed relocation: {val}");

                            unsafe {
                                (code_addr as *mut i32).write(val);
                            }
                        }
                    },
                    other => {
                        let text = headers.get(text_index).unwrap();
                        todo!(
                            "Relocation type: {other} for code {:02X}",
                            elf.section_data(&text).unwrap().0.iter().format("")
                        )
                    },
                }
            }
        }

        let buf = unsafe { std::slice::from_raw_parts(start_addr, self.pos.offset_from_unsigned(start_addr)) };
        info!("Mapped data: {:02X}", buf.iter().format(""));
        self.make_executable(start_addr..self.pos);

        let mut exports = Vec::new();
        for symb in symtab.iter() {
            if symb.st_bind() == STB_GLOBAL && symb.st_shndx != SHN_UNDEF {
                let name = strtab.get(symb.st_name as usize).unwrap();
                info!("Export symbol: {name:?} @ 0x{:X}", symb.st_value);
                exports.push((name.to_string(), unsafe {
                    section_addrs[symb.st_shndx as usize]
                        .unwrap()
                        .byte_add(symb.st_value as usize)
                }));
            }
        }

        exports.into_iter()
    }

    /// Aligns the cursor to 128 bytes.
    fn align(&mut self, num: usize) {
        unsafe {
            self.pos = self.pos.byte_add(self.pos.align_offset(num));
        }

        info!("Aligned to {:p}", self.pos);
    }

    /// Remaps the page under the current cursor to an RW page.
    /// If the cursor is at the start of a page (and therefore, this page should not have been mapped as RX yet),
    /// nothing is done.
    fn remap_mutable_if_needed(&self) {
        if !(self.pos as usize).is_multiple_of(4096) {
            unsafe {
                mprotect(
                    NonNull::new(self.pos.map_addr(|a| a & !0xfff) as *mut c_void).unwrap(),
                    4096,
                    ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                )
                .unwrap()
            }
        }
    }

    /// Writes the provided to the cursor position.
    fn write_bytes(&mut self, bytes: &[u8]) {
        let end = unsafe { self.pos.byte_add(bytes.len()) };
        assert!(end <= self.end);
        unsafe {
            self.pos.copy_from_nonoverlapping(bytes.as_ptr(), bytes.len());
        }

        self.pos = end;
    }

    /// Marks the specified range of pages as executable.
    /// Automatically expands the provided range to multiples of pages.
    fn make_executable(&self, pos: Range<*mut u8>) {
        let start = pos.start.map_addr(|a| a & !0xfff);
        let end = unsafe { pos.end.add(pos.end.align_offset(4096)) };
        let len = unsafe { end.offset_from(start) } as usize;
        assert!(len.is_multiple_of(4096));
        unsafe {
            mprotect(
                NonNull::new(start as *mut c_void).unwrap(),
                len,
                ProtFlags::PROT_READ | ProtFlags::PROT_EXEC,
            )
            .unwrap()
        }

        flush_icache(pos.start, pos.end);
    }

    pub fn memory_usage(&self) -> usize {
        unsafe { self.pos.offset_from_unsigned(self.start) }
    }
}

#[allow(unused)]
fn flush_icache(start_addr: *const u8, end_addr: *const u8) {
    #[cfg(target_arch = "aarch64")]
    unsafe {
        use std::arch::asm;

        let cache_line_size = 64; // typical AArch64 line size
        let mut ptr = start_addr;
        let end = end_addr;

        // Clean data cache to point of unification
        while ptr < end {
            asm!("dc cvau, {0}", in(reg) ptr);
            ptr = ptr.add(cache_line_size);
        }

        // Ensure completion
        asm!("dsb ish");

        // Invalidate instruction cache
        ptr = start_addr;
        while ptr < end {
            asm!("ic ivau, {0}", in(reg) ptr);
            ptr = ptr.add(cache_line_size);
        }

        // Final barriers
        asm!("dsb ish");
        asm!("isb");
    }
}
