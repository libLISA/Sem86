use std::time::Duration;

use arrayvec::ArrayVec;
use itertools::Itertools;
use liblisa::Instruction;
use liblisa::arch::CpuState;
use log::{debug, error, info, trace};
use sem86_arch::addr::LinAddr;
use sem86_arch::exceptions::Interrupt;

use crate::DisplayK;
use crate::arch::intel386::GpReg;
use crate::emulator::{EmulatorContextInner, EmulatorState};
use crate::icache::entry::CacheEntryId;
use crate::time::SynchronousClock;
use crate::util::miniprofiler::Profiler;

pub trait ExecutionHook {
    #[inline(always)]
    fn before_execute<'tag>(&mut self, _ec: &mut EmulatorContextInner<'_, 'tag>, _id: CacheEntryId<'tag>) {}

    #[inline(always)]
    fn after_execute<'tag>(
        &mut self, _ec: &mut EmulatorContextInner<'_, 'tag>, _id: CacheEntryId<'tag>,
    ) -> Result<bool, Interrupt> {
        Ok(true)
    }

    #[inline(always)]
    fn before_encoding_execution<'tag>(&mut self, _ec: &mut EmulatorContextInner<'_, 'tag>, _id: CacheEntryId<'tag>) {}

    #[inline(always)]
    fn after_encoding_execution<'tag>(&mut self, _ec: &mut EmulatorContextInner<'_, 'tag>, _id: CacheEntryId<'tag>) {}

    /// Set to false if the hook needs events after every instruction.
    /// `at` and the before_*/after_* function calls may be skipped if this returns true.
    #[inline(always)]
    fn allow_multiple_instr_single_function(&self) -> bool {
        false
    }

    #[inline(always)]
    fn can_be_discarded<'tag>(&self, _ec: &mut EmulatorContextInner<'_, 'tag>) -> bool {
        false
    }

    #[inline(always)]
    fn can_halt<'tag>(&self, _ec: &mut EmulatorContextInner<'_, 'tag>) -> bool {
        true
    }

    #[inline(always)]
    fn can_return_blocks(&self) -> bool {
        true
    }

    #[inline(always)]
    fn can_accept_interrupts<'tag>(&self, _ec: &mut EmulatorContextInner<'_, 'tag>) -> bool {
        true
    }

    #[inline(always)]
    fn at(&self, _profiler: &mut Profiler<EmulatorState>, _state: EmulatorState) {}

    #[inline(always)]
    fn halt(&mut self) -> bool {
        false
    }
}

impl ExecutionHook for () {
    fn allow_multiple_instr_single_function(&self) -> bool {
        true
    }
}

impl<T1: ExecutionHook, T2: ExecutionHook> ExecutionHook for (T1, T2) {
    #[inline(always)]
    fn before_execute<'tag>(&mut self, ec: &mut EmulatorContextInner<'_, 'tag>, id: CacheEntryId<'tag>) {
        self.0.before_execute(ec, id);
        self.1.before_execute(ec, id);
    }

    #[inline(always)]
    fn after_execute<'tag>(
        &mut self, ec: &mut EmulatorContextInner<'_, 'tag>, id: CacheEntryId<'tag>,
    ) -> Result<bool, Interrupt> {
        Ok(self.0.after_execute(ec, id)? && self.1.after_execute(ec, id)?)
    }

    #[inline(always)]
    fn can_halt<'tag>(&self, ec: &mut EmulatorContextInner<'_, 'tag>) -> bool {
        self.0.can_halt(ec) && self.1.can_halt(ec)
    }

    fn can_be_discarded<'tag>(&self, ec: &mut EmulatorContextInner<'_, 'tag>) -> bool {
        self.0.can_be_discarded(ec) || self.1.can_be_discarded(ec)
    }

    #[inline(always)]
    fn can_return_blocks(&self) -> bool {
        self.0.can_return_blocks() && self.1.can_return_blocks()
    }

    #[inline(always)]
    fn can_accept_interrupts<'tag>(&self, ec: &mut EmulatorContextInner<'_, 'tag>) -> bool {
        self.0.can_accept_interrupts(ec) && self.1.can_accept_interrupts(ec)
    }

    #[inline(always)]
    fn at(&self, profiler: &mut Profiler<EmulatorState>, state: EmulatorState) {
        self.0.at(profiler, state);
        self.1.at(profiler, state);
    }

    #[inline(always)]
    fn before_encoding_execution<'tag>(&mut self, ec: &mut EmulatorContextInner<'_, 'tag>, id: CacheEntryId<'tag>) {
        self.0.before_encoding_execution(ec, id);
        self.1.before_encoding_execution(ec, id);
    }

    #[inline(always)]
    fn after_encoding_execution<'tag>(&mut self, ec: &mut EmulatorContextInner<'_, 'tag>, id: CacheEntryId<'tag>) {
        self.0.after_encoding_execution(ec, id);
        self.1.after_encoding_execution(ec, id);
    }

    #[inline(always)]
    fn allow_multiple_instr_single_function(&self) -> bool {
        self.0.allow_multiple_instr_single_function() && self.1.allow_multiple_instr_single_function()
    }

    #[inline(always)]
    fn halt(&mut self) -> bool {
        self.0.halt() || self.1.halt()
    }
}

impl ExecutionHook for Box<dyn ExecutionHook> {
    #[inline(always)]
    fn before_execute<'tag>(&mut self, ec: &mut EmulatorContextInner<'_, 'tag>, id: CacheEntryId<'tag>) {
        (**self).before_execute(ec, id);
    }

    #[inline(always)]
    fn after_execute<'tag>(
        &mut self, ec: &mut EmulatorContextInner<'_, 'tag>, id: CacheEntryId<'tag>,
    ) -> Result<bool, Interrupt> {
        (**self).after_execute(ec, id)
    }

    #[inline(always)]
    fn can_halt<'tag>(&self, ec: &mut EmulatorContextInner<'_, 'tag>) -> bool {
        (**self).can_halt(ec)
    }

    fn can_be_discarded<'tag>(&self, ec: &mut EmulatorContextInner<'_, 'tag>) -> bool {
        (**self).can_be_discarded(ec)
    }

    #[inline(always)]
    fn can_return_blocks(&self) -> bool {
        (**self).can_return_blocks()
    }

    #[inline(always)]
    fn can_accept_interrupts<'tag>(&self, ec: &mut EmulatorContextInner<'_, 'tag>) -> bool {
        (**self).can_accept_interrupts(ec)
    }

    #[inline(always)]
    fn at(&self, profiler: &mut Profiler<EmulatorState>, state: EmulatorState) {
        (**self).at(profiler, state);
    }

    #[inline(always)]
    fn before_encoding_execution<'tag>(&mut self, ec: &mut EmulatorContextInner<'_, 'tag>, id: CacheEntryId<'tag>) {
        (**self).before_encoding_execution(ec, id);
    }

    #[inline(always)]
    fn after_encoding_execution<'tag>(&mut self, ec: &mut EmulatorContextInner<'_, 'tag>, id: CacheEntryId<'tag>) {
        (**self).after_encoding_execution(ec, id);
    }

    #[inline(always)]
    fn allow_multiple_instr_single_function(&self) -> bool {
        (**self).allow_multiple_instr_single_function()
    }

    #[inline(always)]
    fn halt(&mut self) -> bool {
        (**self).halt()
    }
}

pub struct DirtyTracker {
    num_dirty_before: u64,
}

impl ExecutionHook for DirtyTracker {
    fn before_execute<'tag>(&mut self, ec: &mut EmulatorContextInner<'_, 'tag>, _id: CacheEntryId<'tag>) {
        self.num_dirty_before = ec.emulator.ctx.memory.phys_frames_marked_dirty()
    }

    fn after_execute<'tag>(
        &mut self, ec: &mut EmulatorContextInner<'_, 'tag>, id: CacheEntryId<'tag>,
    ) -> Result<bool, Interrupt> {
        if self.num_dirty_before != ec.emulator.ctx.memory.phys_frames_marked_dirty() {
            let icache_entry = ec.emulator.ctx.mmio_ctx.icache.encoding_info(id);
            let cs_base = ec.emulator.cpu.gpreg(GpReg::CsBase);
            let ip = ec.emulator.cpu.gpreg(GpReg::Ip);
            let pc = cs_base.wrapping_add(ip);
            let instr =
                ec.emulator
                    .ctx
                    .mmio_ctx
                    .icache
                    .resolve_instr_for(id, LinAddr::new(pc as u32).into(), ec.emulator.ctx.memory);
            println!(
                "Frame marked dirty after: {instr:X} {}\n\nCPU State after:\n{}",
                icache_entry.display_instance(ec.semantics.as_ref()),
                ec.emulator.cpu
            )
        }

        Ok(true)
    }
}

pub struct TrackPath;

impl ExecutionHook for TrackPath {
    fn before_execute<'tag>(&mut self, ec: &mut EmulatorContextInner<'_, 'tag>, id: CacheEntryId<'tag>) {
        let cs_base = ec.emulator.cpu.gpreg(GpReg::CsBase);
        let ip = ec.emulator.cpu.gpreg(GpReg::Ip);
        let pc = cs_base.wrapping_add(ip);
        let instr = ec
            .emulator
            .ctx
            .mmio_ctx
            .icache
            .resolve_instr_for(id, LinAddr::new(pc as u32).into(), ec.emulator.ctx.memory);
        ec.emulator.path.push_with(|| (pc, instr));
    }
}

pub struct CheckTrace {
    pc: u64,
}

impl Default for CheckTrace {
    fn default() -> Self {
        Self::new()
    }
}

impl CheckTrace {
    pub fn new() -> Self {
        Self {
            pc: 0,
        }
    }
}

impl ExecutionHook for CheckTrace {
    fn before_execute<'tag>(&mut self, ec: &mut EmulatorContextInner<'_, 'tag>, _id: CacheEntryId<'tag>) {
        let cs_base = ec.emulator.cpu.gpreg(GpReg::CsBase);
        let ip = ec.emulator.cpu.gpreg(GpReg::Ip);
        self.pc = cs_base.wrapping_add(ip);
    }

    fn after_execute<'tag>(
        &mut self, ec: &mut EmulatorContextInner<'_, 'tag>, id: CacheEntryId<'tag>,
    ) -> Result<bool, Interrupt> {
        // TODO: ec.profiler.at::<{P::PROFILE}>(&EmulatorState::CheckTrace);
        let expected_pagefault = if let Some(next) = ec.emulator.next_expected_interrupt.take() {
            let instr = ec.emulator.ctx.mmio_ctx.icache.resolve_instr_for(
                id,
                LinAddr::new(self.pc as u32).into(),
                ec.emulator.ctx.memory,
            );
            error!(
                "Did not generate interrupt after executing {instr:X}: 0x{next:X} (vector offsets: {:X?}, redirection entries: {:X?}) state {}",
                ec.emulator.ctx.mmio_ctx.hw.vector_offsets(),
                (0..24)
                    .map(|n| ec.emulator.ctx.mmio_ctx.hw.redirection_entry_vector(n))
                    .format(", "),
                ec.emulator.cpu
            );
            next == 0xE
        } else {
            false
        };

        if ec.emulator.ctx.trace.is_some() {
            let icache_entry = ec.emulator.ctx.mmio_ctx.icache.encoding_info(id);
            let mut addrs_written = ArrayVec::<_, 64>::new();
            let instr = ec.emulator.ctx.mmio_ctx.icache.resolve_instr_for(
                id,
                LinAddr::new(self.pc as u32).into(),
                ec.emulator.ctx.memory,
            );
            ec.emulator.check_trace(
                Some(id),
                instr,
                &icache_entry,
                &mut addrs_written,
                expected_pagefault,
                ec.semantics.as_ref(),
            )?;
        }

        Ok(true)
    }

    fn can_be_discarded<'tag>(&self, ec: &mut EmulatorContextInner<'_, 'tag>) -> bool {
        ec.emulator.ctx.trace.is_none()
    }

    fn can_halt<'tag>(&self, ec: &mut EmulatorContextInner<'_, 'tag>) -> bool {
        ec.emulator.ctx.trace.is_none()
    }

    fn can_return_blocks(&self) -> bool {
        false
    }

    fn can_accept_interrupts<'tag>(&self, ec: &mut EmulatorContextInner<'_, 'tag>) -> bool {
        ec.emulator.ctx.trace.is_none()
    }
}

pub struct Printer {
    print_at: u64,
    skip_print: bool,
}

impl Printer {
    pub fn new(ec: &mut EmulatorContextInner<'_, '_>) -> Self {
        let print_at = *ec.emulator.print_at.last().unwrap_or(&u64::MAX);
        if ec.emulator.ctx.k > print_at {
            ec.emulator.print_at.pop().unwrap();
            info!("Moving to next print-at entry");
        }

        let skip_print = ec.emulator.ctx.k < print_at.saturating_sub(10_000);
        Self {
            print_at,
            skip_print,
        }
    }
}

impl ExecutionHook for Printer {
    fn before_execute<'tag>(&mut self, ec: &mut EmulatorContextInner<'_, 'tag>, id: CacheEntryId<'tag>) {
        if !self.skip_print {
            let info = ec.emulator.ctx.mmio_ctx.icache.encoding_info(id);
            let cs_base = ec.emulator.cpu.gpreg(GpReg::CsBase);
            let ip = ec.emulator.cpu.gpreg(GpReg::Ip);
            let pc = cs_base.wrapping_add(ip);
            let instr =
                ec.emulator
                    .ctx
                    .mmio_ctx
                    .icache
                    .resolve_instr_for(id, LinAddr::new(pc as u32).into(), ec.emulator.ctx.memory);
            debug!(
                "### CPU now executing 0x{:X}:0x{:X} ({} instrs executed) ###",
                ec.emulator.cpu.gpreg(GpReg::CsBase),
                ec.emulator.cpu.gpreg(GpReg::Ip),
                ec.emulator.ctx.k
            );
            debug!("Executing: {instr:X}\n{}", info.display_instance(ec.semantics.as_ref()));
            trace!("CPU: {}", ec.emulator.cpu);
        } else {
            let print_at = *ec.emulator.print_at.last().unwrap_or(&u64::MAX);
            if ec.emulator.ctx.k > print_at {
                ec.emulator.print_at.pop().unwrap();
                info!("Moving to next print-at entry");
            }

            self.skip_print = ec.emulator.ctx.k < print_at.saturating_sub(10_000);
        }
    }

    fn after_execute<'tag>(
        &mut self, ec: &mut EmulatorContextInner<'_, 'tag>, _id: CacheEntryId<'tag>,
    ) -> Result<bool, Interrupt> {
        Ok(!(!self.skip_print && ec.emulator.ctx.k > self.print_at.saturating_add(5_000)))
    }

    fn can_return_blocks(&self) -> bool {
        self.skip_print
    }
}

pub struct PrintEveryInstr;

impl PrintEveryInstr {
    pub fn new(_ec: &mut EmulatorContextInner<'_, '_>) -> Self {
        Self
    }
}

impl ExecutionHook for PrintEveryInstr {
    fn before_execute<'tag>(&mut self, ec: &mut EmulatorContextInner<'_, 'tag>, id: CacheEntryId<'tag>) {
        let cs_base = ec.emulator.cpu.gpreg(GpReg::CsBase);
        let ip = ec.emulator.cpu.gpreg(GpReg::Ip);
        let pc = cs_base.wrapping_add(ip);
        let instr = ec
            .emulator
            .ctx
            .mmio_ctx
            .icache
            .resolve_instr_for(id, LinAddr::new(pc as u32).into(), ec.emulator.ctx.memory);
        println!(
            "0x{:X}:0x{:X}: {instr:X}",
            ec.emulator.cpu.gpreg(GpReg::CsBase),
            ec.emulator.cpu.gpreg(GpReg::Ip)
        );
    }

    fn can_return_blocks(&self) -> bool {
        false
    }
}

/// TODO: Move profiler out of EmulatorContext so we can directly reference it here and remove the `profiler` param.
pub struct EnableProfiler;

impl ExecutionHook for EnableProfiler {
    fn at(&self, profiler: &mut Profiler<EmulatorState>, state: EmulatorState) {
        profiler.at::<true>(&state);
    }

    fn allow_multiple_instr_single_function(&self) -> bool {
        true
    }
}

pub struct MeasureSingleEncodingExecution {
    start: u64,
}

impl Default for MeasureSingleEncodingExecution {
    fn default() -> Self {
        Self::new()
    }
}

impl MeasureSingleEncodingExecution {
    pub fn new() -> Self {
        Self {
            start: 0,
        }
    }
}

impl ExecutionHook for MeasureSingleEncodingExecution {
    fn before_encoding_execution<'tag>(&mut self, ec: &mut EmulatorContextInner<'_, 'tag>, id: CacheEntryId<'tag>) {
        let info = ec.emulator.ctx.mmio_ctx.icache.encoding_info(id);
        ec.emulator.execution_counts[info.encoding_index] += 1;

        #[cfg(target_arch = "x86_64")]
        {
            self.start = unsafe { std::arch::x86_64::_rdtsc() };
        }
    }

    fn after_encoding_execution<'tag>(&mut self, ec: &mut EmulatorContextInner<'_, 'tag>, id: CacheEntryId<'tag>) {
        #[cfg(target_arch = "x86_64")]
        {
            let info = ec.emulator.ctx.mmio_ctx.icache.encoding_info(id);
            if let Some(elapsed) = unsafe { std::arch::x86_64::_rdtsc() }.checked_sub(self.start)
                && elapsed < 10_000_000_000
            {
                ec.emulator.execution_duration[info.encoding_index] += elapsed;
            }
        }
    }
}

pub struct CheckInstructionCacheConsistency;

impl ExecutionHook for CheckInstructionCacheConsistency {
    fn before_execute<'tag>(&mut self, ec: &mut EmulatorContextInner<'_, 'tag>, id: CacheEntryId<'tag>) {
        let cs_base = ec.emulator.cpu.gpreg(GpReg::CsBase);
        let ip = ec.emulator.cpu.gpreg(GpReg::Ip);
        let pc = cs_base.wrapping_add(ip);
        let instr = ec
            .emulator
            .ctx
            .mmio_ctx
            .icache
            .resolve_instr_for(id, LinAddr::new(pc as u32).into(), ec.emulator.ctx.memory);

        let mut bytes = [0; 16];
        // TODO: Read from userspace if in userspace
        match ec.emulator.ctx.memory.read_slice(
            pc as u32,
            &mut bytes[..instr.byte_len()],
            false,
            &mut ec.emulator.ctx.mmio_ctx,
        ) {
            Ok(_) => (),
            Err(e) => {
                panic!(
                    "Address at pc=0x{pc:X}, (0x{cs_base:X}:0x{ip:X}) is unreadable, but we're about to execute cache entry {id:?} which supposedly is mapped there.\n{e:X?}"
                )
            },
        };

        if let Err(e) = ec
            .emulator
            .ctx
            .mmio_ctx
            .icache
            .decoder()
            .lookup(instr, ec.emulator.segment_sizes)
        {
            let encoding_info = ec.emulator.ctx.mmio_ctx.icache.encoding_info(id);
            let phys_addr = if ec.emulator.paging_enabled {
                match ec.emulator.ctx.memory.page_walk(pc as u32, false) {
                    sem86_arch::mem::PageWalkResult::Unmapped(page_walk_error) => panic!(
                        "Address at pc=0x{pc:X}, (0x{cs_base:X}:0x{ip:X}) is unmapped, but we're about to execute cache entry {id:?} which supposedly is mapped there.\n{page_walk_error:X?}"
                    ),
                    sem86_arch::mem::PageWalkResult::PhysAddr {
                        addr, ..
                    } => addr,
                }
            } else {
                pc
            };
            panic!(
                "Executed instruction does not decode: {e:?}, instr={instr:X}, pc=0x{pc:X}, (0x{cs_base:X}:0x{ip:X}), expected encoding\n\n{}\n\nCurrent memory contents: {:02X?}\nMemory dirty: {}\nPhysical address: 0x{phys_addr:X}\nEntry: {id:?}",
                encoding_info.display_instance(ec.semantics.as_ref()),
                &bytes[..instr.byte_len()],
                ec.emulator.ctx.memory.phys_frame_is_dirty(phys_addr)
            );
        }

        let actual_instr = Instruction::new(&bytes[..instr.byte_len()]);
        if actual_instr != instr {
            let encoding_info = ec.emulator.ctx.mmio_ctx.icache.encoding_info(id);
            let phys_addr = if ec.emulator.paging_enabled {
                ec.emulator.ctx.memory.page_walk(pc as u32, false).unwrap_phys_addr()
            } else {
                pc
            };
            panic!(
                "Executed instruction does not match memory contents: executed {instr:X} but memory contains {actual_instr:X}: pc=0x{pc:X}, (0x{cs_base:X}:0x{ip:X}), expected encoding\n\n{}\n\nCurrent memory contents: {:02X?}\nMemory dirty: {}\nPhysical address: 0x{phys_addr:X}\nEntry: {id:?}",
                encoding_info.display_instance(ec.semantics.as_ref()),
                &bytes[..instr.byte_len()],
                ec.emulator.ctx.memory.phys_frame_is_dirty(phys_addr)
            );
        }
    }
}

pub struct MemTrace {
    addr: u32,
    last_k: u64,
    physical: bool,
}

impl MemTrace {
    pub fn new_lin(addr: u32) -> Self {
        Self {
            addr,
            last_k: 0,
            physical: false,
        }
    }

    pub fn new_phys(addr: u32) -> Self {
        Self {
            addr,
            last_k: 0,
            physical: true,
        }
    }
}

impl ExecutionHook for MemTrace {
    fn after_execute<'tag>(
        &mut self, ec: &mut EmulatorContextInner<'_, 'tag>, _id: CacheEntryId<'tag>,
    ) -> Result<bool, Interrupt> {
        if self.last_k + 1_000 < ec.emulator.ctx.k
            || ec.emulator.ctx.k >= ec.emulator.print_at.last().unwrap_or(&u64::MAX).saturating_sub(10_000)
        {
            self.last_k = ec.emulator.ctx.k;
            let mut buf = [0; 16];
            let addr = self.addr;

            if self.physical {
                match ec.emulator.ctx.memory.read_physical_slice(
                    addr,
                    &mut buf,
                    &mut ec.emulator.ctx.mmio_ctx.hw.mmio(&mut ec.emulator.ctx.mmio_ctx.icache),
                ) {
                    Ok(_) => info!("PMEM TRACE: 0x{addr:X} = {buf:02X?} (k={})", DisplayK(ec.emulator.ctx.k)),
                    Err(e) => info!("PMEM TRACE: 0x{addr:X} = {e:?}"),
                }
            } else {
                match ec.emulator.ctx.memory.read_slice(
                    addr,
                    &mut buf,
                    false,
                    &mut ec.emulator.ctx.mmio_ctx.hw.mmio(&mut ec.emulator.ctx.mmio_ctx.icache),
                ) {
                    Ok(_) => info!("MEM TRACE: 0x{addr:X} = {buf:02X?} (k={})", DisplayK(ec.emulator.ctx.k)),
                    Err(e) => info!("MEM TRACE: 0x{addr:X} = {e:?}"),
                }
            }
        }

        Ok(true)
    }
}

pub struct StopAt(u64);

impl ExecutionHook for StopAt {
    fn after_execute<'tag>(
        &mut self, ec: &mut EmulatorContextInner<'_, 'tag>, _id: CacheEntryId<'tag>,
    ) -> Result<bool, Interrupt> {
        Ok(ec.emulator.ctx.k < self.0)
    }
}

impl StopAt {
    pub fn new(val: u64) -> Self {
        Self(val)
    }
}

pub struct SyncClock<'r> {
    clock: &'r mut SynchronousClock,
    interval: Duration,
}

impl ExecutionHook for SyncClock<'_> {
    fn after_execute<'tag>(
        &mut self, _ec: &mut EmulatorContextInner<'_, 'tag>, _id: CacheEntryId<'tag>,
    ) -> Result<bool, Interrupt> {
        self.clock.tick_by(self.interval);
        Ok(true)
    }

    fn allow_multiple_instr_single_function(&self) -> bool {
        false
    }

    fn halt(&mut self) -> bool {
        self.clock.tick_by(self.interval);
        true
    }
}

impl<'r> SyncClock<'r> {
    pub fn new(clock: &'r mut SynchronousClock) -> Self {
        Self {
            clock,
            interval: Duration::from_nanos(100),
        }
    }
}
