use std::fmt::{Debug, Display};
use std::num::NonZero;
use std::ops::Index;

use log::{debug, trace};
use nix::sys::mman::{MapFlags, ProtFlags, mmap_anonymous, mprotect};

pub mod aarch64;
pub mod x86_64;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Export(usize);

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Label(usize);

pub struct Exports(Vec<*const u8>);

#[cfg(target_arch = "x86_64")]
pub type CurrentTarget = x86_64::X86;

#[cfg(target_arch = "aarch64")]
pub type CurrentTarget = aarch64::AArch64;

impl Index<Export> for Exports {
    type Output = *const u8;

    fn index(&self, index: Export) -> &Self::Output {
        &self.0[index.0]
    }
}

pub enum Ir<T: Target> {
    ReadU32 {
        into: T::Reg,
        base: T::Reg,
        offset: u32,
    },
    ReadU64 {
        into: T::Reg,
        base: T::Reg,
        offset: u32,
    },
    ReadArrayPtrU32 {
        into: T::Reg,
        label: Label,
        index: T::Reg,
    },
    LoadImm {
        into: T::Reg,
        val: u64,
    },
    /// Loads `val` into the lower 8 bits of `into`.
    /// Higher bits are unspecified.
    LoadImm8 {
        into: T::Reg,
        val: u8,
    },
    Load {
        into: T::Reg,
        from: T::Reg,
    },
    CallRipRelative {
        label: Label,
    },
    Return {
        val: T::Reg,
    },
    Jump {
        to: Label,
    },
    BrIfReg8False {
        val: T::Reg,
        to: Label,
    },
    Push {
        val: T::Reg,
    },
    Pop {
        into: T::Reg,
    },
    /// Reads a 32-bit value from memory atomically, and compares it with 0.
    /// If the value was non-zero, jumps to `to`.
    BrIfMem32IsNonZeroAtomic {
        base: T::Reg,
        offset: u32,
        to: Label,
    },
    BrIfMem8IsZero {
        base: T::Reg,
        offset: u32,
        to: Label,
    },
    AlignedDataU64 {
        val: u64,
    },
    AlignedDataU32 {
        val: u32,
    },
    AddU32 {
        dst: T::Reg,
        src: T::Reg,
    },
    BandU32Imm {
        reg: T::Reg,
        imm: u32,
    },
    BrIfEqImm {
        reg: T::Reg,
        imm: u32,
        label: Label,
    },
    SelectBits {
        into: T::Reg,
        from: T::Reg,
        start: u8,
        end: u8,
    },
}

impl<T: Target> Ir<T> {
    fn align(&self, emitter: &mut Emitter) {
        if let Self::AlignedDataU64 {
            ..
        } = self
        {
            emitter.align(8);
        } else if let Self::AlignedDataU32 {
            ..
        } = self
        {
            emitter.align(4);
        }
    }
}

pub trait Target: Sized {
    type Reg: Copy + Clone + Debug + PartialEq + Eq + 'static;

    const RETURN_REG: Self::Reg;
    const RETURN_REG2: Self::Reg;
    const CALLEE_SAVED_REGS: &[Self::Reg];
    const CALLER_SAVED_REGS: &[Self::Reg];
    const PARAMETER_REGS: &[Self::Reg];

    fn compile(x: &Ir<Self>, e: &mut Emitter);
    fn relocation_offset(size: usize) -> usize;
}

struct Relocation {
    pos: usize,
    bits: usize,
    label: Label,
    shift: usize,
    shr: usize,
}

pub struct Emitter {
    bytes: Vec<u8>,
    relocations: Vec<Relocation>,
    label_locations: Vec<usize>,
}

pub trait Emittable {
    fn emit(&self, target: &mut Emitter);
}

impl Emittable for u8 {
    fn emit(&self, target: &mut Emitter) {
        target.bytes.push(*self);
    }
}

impl Emittable for &[u8] {
    fn emit(&self, target: &mut Emitter) {
        target.bytes.extend_from_slice(self);
    }
}

impl<const N: usize> Emittable for &[u8; N] {
    fn emit(&self, target: &mut Emitter) {
        target.bytes.extend_from_slice(*self);
    }
}

impl<const N: usize> Emittable for [u8; N] {
    fn emit(&self, target: &mut Emitter) {
        target.bytes.extend_from_slice(self);
    }
}

impl Emittable for Label {
    fn emit(&self, target: &mut Emitter) {
        target.relocations.push(Relocation {
            pos: target.bytes.len(),
            bits: 32,
            shift: 0,
            shr: 0,
            label: *self,
        });

        target.bytes.extend_from_slice(&[0; 4]);
    }
}

pub struct LabelInU32 {
    label: Label,
    val: u32,
    bits: usize,
    shift: usize,
    shr: usize,
}

impl LabelInU32 {
    pub fn new(label: Label, val: u32, bits: usize, shift: usize, shr: usize) -> Self {
        assert_eq!(
            val & (!(u32::MAX << bits) << shift),
            0,
            "lower {bits} bits (shift {shift}) must be zero"
        );
        Self {
            label,
            val,
            bits,
            shift,
            shr,
        }
    }
}

impl Emittable for LabelInU32 {
    fn emit(&self, target: &mut Emitter) {
        target.relocations.push(Relocation {
            pos: target.bytes.len(),
            bits: self.bits,
            label: self.label,
            shift: self.shift,
            shr: self.shr,
        });

        target.bytes.extend_from_slice(&self.val.to_le_bytes());
    }
}

impl Default for Emitter {
    fn default() -> Self {
        Self::new()
    }
}

impl Emitter {
    pub fn new() -> Self {
        Self {
            bytes: Vec::new(),
            relocations: Vec::new(),
            label_locations: Vec::new(),
        }
    }

    pub fn mark_label(&mut self, label: Label) {
        if self.label_locations.len() <= label.0 {
            self.label_locations.resize(label.0 + 1, usize::MAX);
        }

        self.label_locations[label.0] = self.bytes.len();
    }

    pub fn emit(&mut self, e: impl Emittable) {
        e.emit(self)
    }

    pub fn offset(&self) -> usize {
        self.bytes.len()
    }

    pub fn finish<T: Target>(mut self) -> Vec<u8> {
        trace!("Label locations ({}): {:?}", self.label_locations.len(), self.label_locations);
        for relocation in self.relocations {
            let label_pos = self.label_locations[relocation.label.0];
            assert!(
                label_pos != usize::MAX,
                "label not emitted: {:?} for relocation near: {:02X?}",
                relocation.label,
                &self.bytes[relocation.pos - 8..relocation.pos + 8]
            );
            let offset = (label_pos as u32).wrapping_sub(relocation.pos as u32 + T::relocation_offset(4) as u32);

            let mask = !((!(u32::MAX.checked_shl(relocation.bits as u32).unwrap_or(0)))
                .checked_shl(relocation.shift as u32)
                .unwrap_or(0));
            let current_val = u32::from_le_bytes(self.bytes[relocation.pos..relocation.pos + 4].try_into().unwrap());
            let new_val = (current_val & mask) | (((offset >> relocation.shr) << relocation.shift) & !mask);
            self.bytes[relocation.pos..relocation.pos + 4].copy_from_slice(&new_val.to_le_bytes());
        }

        self.bytes
    }

    pub fn align(&mut self, align: usize) {
        while !self.bytes.len().is_multiple_of(align) {
            self.bytes.push(0);
        }
    }
}

pub struct SinglePass<T: Target> {
    ir: Vec<Ir<T>>,
    label_offsets: Vec<usize>,
    export_offsets: Vec<usize>,
    callee_saved_regs: Vec<T::Reg>,
    caller_saved_regs: Vec<T::Reg>,
}

struct DisplayBytes<'a>(&'a [u8]);

impl Display for DisplayBytes<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in self.0.iter() {
            write!(f, "{byte:02X}")?;
        }

        Ok(())
    }
}

impl<T: Target> Default for SinglePass<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Target> SinglePass<T> {
    pub fn new() -> Self {
        Self {
            ir: Vec::new(),
            label_offsets: Vec::new(),
            export_offsets: Vec::new(),
            callee_saved_regs: T::CALLEE_SAVED_REGS.to_vec(),
            caller_saved_regs: T::CALLER_SAVED_REGS.to_vec(),
        }
    }

    pub fn alloc_caller_saved_reg(&mut self) -> T::Reg {
        self.caller_saved_regs.pop().unwrap()
    }

    pub fn alloc_callee_saved_reg(&mut self) -> T::Reg {
        self.callee_saved_regs.pop().unwrap()
    }

    pub fn alloc_label(&mut self) -> Label {
        let val = Label(self.label_offsets.len());
        self.label_offsets.push(usize::MAX);
        val
    }

    pub fn export(&mut self) -> Export {
        let val = Export(self.export_offsets.len());
        self.export_offsets.push(self.ir.len());
        val
    }

    pub fn mark_label(&mut self, label: Label) {
        trace!("Marking label {label:?} at {}", self.ir.len());
        self.label_offsets[label.0] = self.ir.len();
    }

    pub fn parameter_reg(&self, index: u8) -> T::Reg {
        T::PARAMETER_REGS[index as usize]
    }

    pub fn mov(&mut self, into: T::Reg, from: T::Reg) {
        self.ir.push(Ir::Load {
            into,
            from,
        })
    }

    pub fn push(&mut self, val: T::Reg) {
        self.ir.push(Ir::Push {
            val,
        })
    }

    pub fn pop(&mut self, into: T::Reg) {
        self.ir.push(Ir::Pop {
            into,
        })
    }

    pub fn load_imm(&mut self, into: T::Reg, val: u64) {
        self.ir.push(Ir::LoadImm {
            into,
            val,
        })
    }

    pub fn load_imm_u8(&mut self, into: T::Reg, val: u8) {
        self.ir.push(Ir::LoadImm8 {
            into,
            val,
        })
    }

    pub fn call_rip_relative(&mut self, label: Label) -> T::Reg {
        self.ir.push(Ir::CallRipRelative {
            label,
        });

        self.return_reg()
    }

    pub fn br_if_reg8_false(&mut self, val: T::Reg, to: Label) {
        self.ir.push(Ir::BrIfReg8False {
            val,
            to,
        });
    }

    pub fn ret(&mut self, val: T::Reg) {
        self.ir.push(Ir::Return {
            val,
        })
    }

    pub fn return_reg(&self) -> T::Reg {
        T::RETURN_REG
    }

    pub fn return_reg2(&self) -> T::Reg {
        T::RETURN_REG2
    }

    pub fn br_if_mem_is_nonzero_atomic(&mut self, base: T::Reg, offset: u32, to: Label) {
        self.ir.push(Ir::BrIfMem32IsNonZeroAtomic {
            base,
            offset,
            to,
        });
    }

    pub fn br_if_mem8_is_zero(&mut self, base: T::Reg, offset: u32, to: Label) {
        self.ir.push(Ir::BrIfMem8IsZero {
            base,
            offset,
            to,
        });
    }

    pub fn jump(&mut self, to: Label) {
        self.ir.push(Ir::Jump {
            to,
        })
    }

    pub fn build(&self) -> Exports {
        let mut e = Emitter::new();
        let mut exports = vec![0; self.export_offsets.len()];

        // TODO: Make this truely single-pass by doing this in the various methods that now call `self.ir.push`.
        for (index, item) in self.ir.iter().enumerate() {
            item.align(&mut e);

            // TODO: Performance
            for (label_index, &n) in self.label_offsets.iter().enumerate() {
                if n == index {
                    e.mark_label(Label(label_index));
                }
            }

            // TODO: Performance
            if let Some(export_index) = self.export_offsets.iter().position(|&n| n == index) {
                exports[export_index] = e.offset();
            }

            T::compile(item, &mut e);
        }

        let bytes = e.finish::<T>();
        debug!("Compiled single pass bytes: {}", DisplayBytes(&bytes));

        // TODO: Right now we just leak this memory. While not a safety issue, we should properly track the memory and free it when it becomes unused.

        let allocation_length = bytes.len().div_ceil(4096) * 4096;
        // SAFETY: length is guaranteed to be multiple of 4096 bytes.
        let addr = unsafe {
            mmap_anonymous(
                None,
                NonZero::new(allocation_length).unwrap(),
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS,
            )
            .unwrap()
        };

        // SAFETY: We never copy more bytes than stored in `bytes`.
        // SAFETY: The allocation is at least as big as `bytes` (see above).
        // SAFETY: We do not create a &mut to avoid uninitialized data.
        unsafe {
            (addr.as_ptr() as *mut u8).copy_from(bytes.as_ptr(), bytes.len());
        };

        flush_icache(
            addr.as_ptr() as *const _,
            unsafe { addr.as_ptr().byte_add(allocation_length) } as *const _,
        );

        // SAFETY: `addr` and `allocation_length` correspond to the pages allocated above.
        // This guarantees that the function behaves correctly.
        unsafe {
            mprotect(addr, allocation_length, ProtFlags::PROT_EXEC).unwrap();
        }

        Exports(
            exports
                .into_iter()
                .map(|offset| unsafe {
                    // SAFETY: Offset is an offset within the bounds of `bytes`, which is what we mapped.
                    addr.as_ptr().byte_add(offset) as *const u8
                })
                .collect(),
        )
    }

    pub fn emit_aligned_data_u64(&mut self, val: u64) {
        self.ir.push(Ir::AlignedDataU64 {
            val,
        })
    }

    pub fn emit_aligned_data_u32(&mut self, val: u32) {
        self.ir.push(Ir::AlignedDataU32 {
            val,
        })
    }

    pub fn load_ptr_u32(&mut self, into: T::Reg, base: T::Reg, offset: u32) {
        self.ir.push(Ir::ReadU32 {
            into,
            base,
            offset,
        })
    }

    pub fn load_ptr_u64(&mut self, into: T::Reg, base: T::Reg, offset: u32) {
        self.ir.push(Ir::ReadU64 {
            into,
            base,
            offset,
        })
    }

    pub fn add_u32_into(&mut self, dst: T::Reg, src: T::Reg) {
        self.ir.push(Ir::AddU32 {
            dst,
            src,
        })
    }

    pub fn band_u32_imm(&mut self, reg: T::Reg, imm: u32) {
        self.ir.push(Ir::BandU32Imm {
            reg,
            imm,
        })
    }

    pub fn br_if_eq_imm(&mut self, reg: T::Reg, imm: u32, label: Label) {
        self.ir.push(Ir::BrIfEqImm {
            reg,
            imm,
            label,
        })
    }

    pub fn select_bits(&mut self, target: T::Reg, source: T::Reg, start: u8, end: u8) {
        self.ir.push(Ir::SelectBits {
            into: target,
            from: source,
            start,
            end,
        })
    }

    pub fn load_array_ptr_u32(&mut self, target: T::Reg, label: Label, index: T::Reg) {
        self.ir.push(Ir::ReadArrayPtrU32 {
            into: target,
            label,
            index,
        })
    }

    pub fn find_temporaries_not_in<const N: usize>(&self, excluded: &[T::Reg]) -> [T::Reg; N] {
        let mut iter = T::CALLER_SAVED_REGS.iter().copied().filter(|reg| !excluded.contains(reg));

        std::array::from_fn(|_| iter.next().unwrap())
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
