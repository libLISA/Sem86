use std::marker::PhantomData;

use arbitrary_int::{Number, u1, u2, u4, u24};
use liblisa::arch::CpuState;
use liblisa::utils::bitmask_u64;
use log::{debug, info, trace, warn};
use sem86_arch::addr::PhysFrameIndex;
use sem86_arch::exceptions::Exception;
use sem86_arch::mem::{FastMem32, MarkDirtyAdvice, Mem32, Mmio};

use crate::DisplayK;
use crate::arch::intel386::{GpReg, Intel386, Intel386Flag, State};
use crate::hw::Hw;
use crate::icache::InstructionCache;
use crate::il::PackedExecResult;
use crate::system::{
    CachedDescriptorAccessRights, Db, Descriptor, DescriptorFlags, DescriptorInfo, SegmentAccessByte, SegmentSelector, Tss,
};
use crate::tracefile::{TraceEntry, TraceEntryReader};

pub struct StackTransaction {
    mask: u32,
    esp: u32,
    ss_base: u32,
    op_size: Db,
    is_userspace: bool,
}

impl StackTransaction {
    #[inline(always)]
    pub fn new(ss_size: Db, op_size: Db, cpu: &State) -> StackTransaction {
        let stack_addr_size = match ss_size {
            Db::Protected16 => 2,
            Db::Protected32 => 4,
        };
        let mask = bitmask_u64(stack_addr_size * 8);
        StackTransaction {
            mask: mask as u32,
            esp: cpu.gpreg(GpReg::Sp) as u32,
            ss_base: cpu.gpreg(GpReg::SsBase) as u32,
            is_userspace: cpu.is_userspace(),
            op_size,
        }
    }

    #[inline(always)]
    pub fn pop(&mut self, mem: &Mem32, mmio: &mut impl Mmio) -> u32 {
        let sp = self.esp;
        let effective_sp = self.ss_base + (sp & self.mask);
        let val = match self.op_size {
            Db::Protected16 => mem.read_u16(effective_sp, self.is_userspace, mmio).unwrap() as u32,
            Db::Protected32 => mem.read_u32(effective_sp, self.is_userspace, mmio).unwrap(),
        };

        // Increment SP
        let new_sp: u32 = sp.wrapping_add(self.op_size.num_bytes() as u32);
        self.esp = (sp & !self.mask) | (new_sp & self.mask);

        debug!("Pop: 0x{val:08X} @ 0x{effective_sp:06X}");

        val
    }

    #[inline(always)]
    pub fn push(&mut self, mem: &Mem32, mmio: &mut impl Mmio, val: u32) {
        // Decrement SP
        let sp = self.esp;
        let new_sp: u32 = sp.wrapping_sub(self.op_size.num_bytes() as u32);
        self.esp = (sp & !self.mask) | (new_sp & self.mask);

        let effective_sp = self.ss_base + (self.esp & self.mask);

        debug!("Push: 0x{val:08X} @ 0x{effective_sp:06X}");

        match self.op_size {
            Db::Protected16 => mem.write::<u16>(effective_sp, self.is_userspace, val as u16, mmio).unwrap(),
            Db::Protected32 => mem.write::<u32>(effective_sp, self.is_userspace, val, mmio).unwrap(),
        }
    }

    #[inline(always)]
    pub fn commit(self, cpu: &mut State) {
        cpu.set_gpreg(GpReg::Sp, self.esp as u64);
    }
}

pub struct MmioExecutionContext<'tag> {
    pub hw: Hw,
    pub icache: InstructionCache<'tag>,
}

impl MmioExecutionContext<'_> {
    pub fn update(&mut self) -> bool {
        self.hw.update(&mut self.icache)
    }
}

// pub type TraceType = Box<dyn Read + Send + 'static>;
pub type TraceType = std::io::BufReader<std::fs::File>;

pub struct ExecutionContext<'mem, 'tag, A> {
    pub mmio_ctx: MmioExecutionContext<'tag>,
    pub memory: &'mem Mem32,
    pub fast_memory: FastMem32<&'mem Mem32>,
    pub trace: Option<TraceEntryReader<TraceType>>,
    pub protected_mode: bool,
    pub k: u64,
    pub jit_k: u64,
    pub result: PackedExecResult,
    _phantom: PhantomData<A>,
    pub num_port_outs: u64,
    pub num_port_ins: u64,
    pub num_descriptors_read: u64,
}

pub struct DescriptorReadResult {
    pub ok: bool,
    pub base: u64,
    pub limit: u64,
    pub access_rights: u64,
}

impl<'mem, 'tag> ExecutionContext<'mem, 'tag, Intel386> {
    pub fn new(hw: Hw, mem: &'mem Mem32, trace: Option<TraceEntryReader<TraceType>>, icache: InstructionCache<'tag>) -> Self {
        Self {
            mmio_ctx: MmioExecutionContext {
                hw,
                icache,
            },
            memory: mem,
            fast_memory: FastMem32::new(mem),
            trace,
            protected_mode: false,
            k: 0,
            jit_k: 0,
            result: PackedExecResult::default(),
            num_port_ins: 0,
            num_port_outs: 0,
            num_descriptors_read: 0,
            _phantom: PhantomData,
        }
    }

    #[inline(always)]
    pub fn begin_stack_transaction(&self, ss_size: Db, op_size: Db, cpu: &State) -> StackTransaction {
        StackTransaction::new(ss_size, op_size, cpu)
    }

    pub fn load_descriptor(
        &mut self, cpu: &State, selector: SegmentSelector, check_permissions: bool, mark_accessed: bool,
    ) -> Result<Option<Descriptor>, Exception> {
        if selector.is_null() {
            debug!("Tried loading NULL selector: {selector:X?}");
            return Ok(None)
        }

        let (table_addr, table_limit) = if selector.is_local() {
            (cpu.gpreg(GpReg::LdtBase) as u32, cpu.gpreg(GpReg::LdtLimit) as u32)
        } else {
            (cpu.gpreg(GpReg::GdtBase) as u32, cpu.gpreg(GpReg::GdtLimit) as u32)
        };

        let offset = selector.segment_index().as_u32() * 8;
        let descriptor_addr = table_addr + offset;

        debug!(
            "Reading descriptor from 0x{descriptor_addr:X} (0x{table_addr:X} + 0x{:X} * 8)",
            selector.segment_index()
        );

        if offset + 7 > table_limit {
            warn!("Segment selector out of range: {selector:X?}");
            return Ok(None)
        }

        let descriptor_val = self.memory.read_u64(descriptor_addr, false, &mut self.mmio_ctx)?;
        let mut descriptor = Descriptor::from(descriptor_val);

        debug!("Descriptor: {descriptor:X?}");
        if check_permissions
            && !matches!(descriptor.access_byte().data(), DescriptorInfo::CodeOrData(seg) if seg.executable() && seg.direction_or_conforming())
        {
            let rpl = selector.rpl().as_u8();
            let dpl = descriptor.access_byte().dpl().as_u8();
            let cpl = cpu.gpreg(GpReg::Cpl) as u8;

            // If the descriptor is not visible at the current CPL and selector RPL, fault
            if cpl > dpl || rpl > dpl {
                debug!("Segment is not a conforming code segment and cpl={cpl}, rpl={rpl}, dpl={dpl}");
                return Ok(None)
            }
        }

        if mark_accessed
            && let DescriptorInfo::CodeOrData(mut info) = descriptor.access_byte().data()
            && !info.accessed()
        {
            info.set_accessed(true);
            let mut b = descriptor.access_byte();
            b.set_info(DescriptorInfo::CodeOrData(info));

            descriptor.set_access_byte(b);

            let val = (u64::from(descriptor) >> 40) as u8;
            debug!(
                "Marking descriptor {descriptor:?} as accessed by writing {val:02X} to 0x{:X}",
                descriptor_addr + 5
            );
            info!(
                "Marked descriptor at 0x{descriptor_addr:X} as accessed by writing to byte 5: 0x{:02X}",
                val
            );
            self.memory.write::<u8>(descriptor_addr + 5, false, val, &mut self.mmio_ctx)?;
        }

        Ok(Some(descriptor))
    }

    pub fn read_descriptor(
        &mut self, cpu: &mut State, force: bool, mark_accessed: bool, selector_val: u16,
    ) -> Result<DescriptorReadResult, Exception> {
        self.num_descriptors_read += 1;
        if (self.protected_mode && !cpu.flag(Intel386Flag::Vm)) || force {
            let selector = SegmentSelector::from(selector_val);
            if selector.is_null() {
                // A null descriptor is valid, but should reference an empty area
                let descriptor = Descriptor::new(
                    0,
                    u24::new(0),
                    SegmentAccessByte::new(u4::new(0), true, u2::new(0), true),
                    u4::new(0),
                    DescriptorFlags::new(u1::new(0), false, Db::Protected32, crate::system::Granularity::Byte),
                    0,
                );
                Ok(DescriptorReadResult {
                    ok: true,
                    base: 0,
                    limit: 0,
                    access_rights: u64::from(CachedDescriptorAccessRights::from(descriptor)),
                })
            } else if let Some(descriptor) = self.load_descriptor(cpu, selector, true, mark_accessed)? {
                Ok(DescriptorReadResult {
                    ok: true,
                    base: descriptor.base() as u64,
                    limit: descriptor.effective_limit_taking_direction_into_account() as u64,
                    access_rights: u64::from(CachedDescriptorAccessRights::from(descriptor)),
                })
            } else {
                Ok(DescriptorReadResult {
                    ok: false,
                    base: 0,
                    limit: 0,
                    access_rights: 0,
                })
            }
        } else {
            let desc = Descriptor::from_real_mode_selector(selector_val);
            Ok(DescriptorReadResult {
                ok: true,
                base: desc.base() as u64,
                limit: desc.effective_limit_taking_direction_into_account() as u64,
                access_rights: u64::from(CachedDescriptorAccessRights::from(desc)) | 1,
            })
        }
    }

    pub fn port_in(&mut self, cpu: &State, port: u16, len: u8) -> Result<u64, Exception> {
        self.num_port_ins += 1;

        const IGNORED_PORTS: &[u16] = &[
            // PIT
            0x40,   // IDE status register, where we instantly mark completion but Bochs has a timer.
            0x1F7,  // CGA status register (timing-based)
            0x3DA,  // PMTMR (timer)
            0xB008, // CMOS
            0x71,
        ];
        if !self.check_io_permission(cpu, port, len as u32) {
            return Err(Exception::GeneralProtectionFault(0))
        }

        let mut result = match len {
            1 => self.mmio_ctx.hw.read_port_u8(port, &mut self.mmio_ctx.icache) as u32,
            2 => self.mmio_ctx.hw.read_port_u16(port, &mut self.mmio_ctx.icache) as u32,
            4 => self.mmio_ctx.hw.read_port_u32(port, &mut self.mmio_ctx.icache),
            _ => unreachable!(),
        };
        let mask = bitmask_u64(len as u32 * 8);

        if let Some(t) = self.trace.as_mut()
            && let Some(entry) = t.next(false)
        {
            if let TraceEntry::In(t) = entry {
                assert_eq!(t.len, len);
                let c = t.port;
                assert_eq!(c, port);
                let c = t.value;

                if c & mask as u32 != result {
                    if IGNORED_PORTS.contains(&port) {
                        trace!(
                            "correcting timing discrepancy in {len}-byte read from port 0x{port:X}: found 0x{result:X}, expected 0x{c:X} (k={})",
                            DisplayK(self.k)
                        );
                    } else {
                        warn!(
                            "incorrect value reading {len} bytes from port 0x{port:X}: found 0x{result:X}, expected 0x{c:X} (k={})",
                            DisplayK(self.k)
                        );
                    }

                    result = c;
                }
            } else {
                panic!("Expected TraceEntry::In, found: {entry:#X?}")
            }
        }

        Ok((result as u64) & mask)
    }

    pub fn port_out(&mut self, cpu: &State, port: u16, len: u8, val: u32) -> Result<(), Exception> {
        self.num_port_outs += 1;

        if !self.check_io_permission(cpu, port, len as u32) {
            return Err(Exception::GeneralProtectionFault(0))
        }

        if let Some(t) = self.trace.as_mut()
            && let Some(entry) = t.next(false)
        {
            if let TraceEntry::Out(t) = entry {
                let c = t.port;
                assert_eq!(c, port);
                let c = t.value;
                assert_eq!(c, val);
                assert_eq!(t.len, len);
            } else {
                panic!("Expected TraceEntry::Out, found: {entry:#X?}")
            }
        }

        match len {
            1 => self.mmio_ctx.hw.write_port_u8(port, val as u8, &mut self.mmio_ctx.icache),
            2 => self.mmio_ctx.hw.write_port_u16(port, val as u16, &mut self.mmio_ctx.icache),
            4 => self.mmio_ctx.hw.write_port_u32(port, val, &mut self.mmio_ctx.icache),
            _ => unreachable!(),
        }

        Ok(())
    }

    fn tss(&mut self, cpu: &State) -> Tss<'_, MmioExecutionContext<'tag>> {
        let addr = cpu.gpreg(GpReg::TrBase) as u32;
        Tss::new(addr, self.memory, &mut self.mmio_ctx)
    }

    fn check_io_permission(&mut self, cpu: &State, port: u16, size: u32) -> bool {
        if self.protected_mode && ((cpu.flag(Intel386Flag::Vm)) || cpu.gpreg(GpReg::Cpl) as u8 > cpu.gpreg(GpReg::Iopl) as u8) {
            let tss_addr = cpu.gpreg(GpReg::TrBase) as u32;
            let mut tss = self.tss(cpu);
            let iopb_offset = tss.iopb() as u64 + port as u64 / 8;

            if iopb_offset > cpu.gpreg(GpReg::TrLimit) {
                // No IOPB -- assume no permission
                false
            } else {
                let iopb_addr = tss_addr + iopb_offset as u32;
                let byte1 = self.memory.read::<u8>(iopb_addr, false, &mut self.mmio_ctx).unwrap();
                let byte2 = self.memory.read::<u8>(iopb_addr + 1, false, &mut self.mmio_ctx).unwrap();
                let word = byte1 as u16 | ((byte2 as u16) << 8);
                let mask = bitmask_u64(size) as u16;

                debug!("Checking permission bits in 0x{word:04X} for port 0x{port:X}");

                (word >> (port % 8)) & mask == 0
            }
        } else {
            true
        }
    }

    pub fn software_interrupt_is_redirected(&mut self, cpu: &State, n: u8) -> bool {
        assert!(self.protected_mode);
        let tss_addr = cpu.gpreg(GpReg::TrBase) as u32;
        let mut tss = self.tss(cpu);
        let redirection_bitmap_offset = tss.iopb() as u64 - 32;
        if redirection_bitmap_offset > cpu.gpreg(GpReg::TrLimit) {
            // No IOPB -- assume no permission
            false
        } else {
            let bitmap_byte_addr = tss_addr + redirection_bitmap_offset as u32 + (n / 8) as u32;
            let b = self.memory.read::<u8>(bitmap_byte_addr, false, &mut self.mmio_ctx).unwrap();

            debug!("Checking redirection bitmap for INT {n:X}");

            (b >> (n % 8)) & 1 == 0
        }
    }

    pub fn set_protected_mode(&mut self, enable: bool) {
        self.protected_mode = enable;
    }
}

impl Mmio for MmioExecutionContext<'_> {
    fn read_mem<D: sem86_arch::mem::MemoryData>(&mut self, id: sem86_arch::mem::MmioId, address: u32) -> D {
        self.hw.mmio(&mut self.icache).read_mem(id, address)
    }

    fn write_mem<D: sem86_arch::mem::MemoryData>(&mut self, id: sem86_arch::mem::MmioId, address: u32, val: D) {
        self.hw.mmio(&mut self.icache).write_mem(id, address, val);
    }

    fn notify_memory_dirty(&mut self, phys_frame_index: PhysFrameIndex, mem: &Mem32) {
        self.hw.mmio(&mut self.icache).notify_memory_dirty(phys_frame_index, mem);
    }

    fn advise_memory_dirty(&mut self, addr: sem86_arch::addr::PhysAddr, len: u8) -> MarkDirtyAdvice {
        self.icache.advise_mark_dirty(addr, len)
    }
}
