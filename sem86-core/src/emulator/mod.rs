use std::cmp::Reverse;
use std::io::Write;
use std::iter::repeat_n;
use std::mem::{MaybeUninit, offset_of};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arbitrary_int::Number;
use arrayvec::ArrayVec;
use generativity::Guard;
use hooks::StopAt;
use itertools::Itertools;
use liblisa::Instruction;
use liblisa::arch::CpuState;
use liblisa::encoding::UnsizedParLoc;
use liblisa::value::AsValue;
use log::{debug, error, info, log_enabled, warn};
use num_traits::{FromPrimitive, ToPrimitive};
use sem86_arch::addr::{LinAddr, LinPageIndex};
use sem86_arch::cpuid::eax01::EdxFeatures;
use sem86_arch::exceptions::{Exception, ExceptionClass, Interrupt};
use sem86_arch::mem::{Mem32, Mmio, PageWalkResult};

use crate::arch::intel386::{
    GpReg, HANDLER_CPUID, HANDLER_CS_UPDATED, HANDLER_HALT, HANDLER_IF_UPDATED, HANDLER_INT, HANDLER_INVALIDATE_PAGE,
    HANDLER_IRET, HANDLER_RDMSR, HANDLER_SS_UPDATED, HANDLER_WRITE_CR, HANDLER_WRMSR, HandlerId, Intel386, Intel386Flag, Reg,
    State,
};
use crate::codegen::backends::UninstantiatedBackendFn;
// #[cfg(not(target_os = "android"))]
use crate::codegen::backends::inkwell::{InkwellBackend, InkwellContext};
use crate::codegen::see::SingleEncodingExecution;
use crate::decoder::{EncodingLookup, PackedInstrSem};
use crate::emulator::exec::{ExecutionContext, TraceType};
use crate::emulator::hooks::{CheckTrace, ExecutionHook, MeasureSingleEncodingExecution, Printer, TrackPath};
use crate::emulator::perf::PerformanceMonitor;
use crate::emulator::snapshot::EmulatorSnapshot;
use crate::hw::Hw;
use crate::hw::intr::{EarlyIntr, Intr, IntrHandle};
use crate::icache::entry::CacheEntryId;
use crate::icache::exec::{EncodingInfo, Executable};
use crate::icache::{ContextFlags, CurrentState, InstructionCache, LookupProgress};
use crate::il::part_values::PartValues;
use crate::il::{EfficientSystemState, ExecResult};
use crate::system::{
    CachedDescriptorAccessRights, Cr0, Cr4, Db, Descriptor, DescriptorInfo, GateDescriptor, GateType, SegmentSelector, Tss,
};
use crate::time::SynchronousClock;
use crate::tracefile::{TraceEntry, TraceEntryReader};
use crate::util::miniprofiler::{EncodeU64, Profiler};
use crate::util::packing::BitPacker;
use crate::util::ringbuf::FixedRingbuf;
use crate::{DisplayK, SegmentSizes};

pub mod exec;
pub mod hooks;
pub mod perf;
pub mod snapshot;
pub mod stat;

#[derive(Copy, Clone, Debug, bitcode::Encode, bitcode::Decode)]
pub enum EmulatorState {
    Other,
    DecodeEncoding,
    CacheLookupFirst(LookupProgress),
    CacheLookupNext(LookupProgress),
    ExecuteBlock,
    ExecuteSingleEncoding,
    ExecuteDispatch,
    InspectResult,
    Tick,
    TickAndUpdate,
    EnterDecode,
    InvokeHandler(HandlerId),
    CheckTrace,
    EnterInterrupt,
    PerformanceMeasurements,
    RunLoop,
    Halt,
    ExecuteJittedBlock,
    MakeChains,
}

impl EncodeU64 for EmulatorState {
    fn encode(&self) -> u64 {
        match self {
            EmulatorState::Other => 0,
            EmulatorState::DecodeEncoding => 1,
            EmulatorState::CacheLookupFirst(lookup_progress) => 2 | (lookup_progress.encode() << 8),
            EmulatorState::CacheLookupNext(lookup_progress) => 3 | (lookup_progress.encode() << 8),
            EmulatorState::ExecuteBlock => 4,
            EmulatorState::ExecuteSingleEncoding => 5,
            EmulatorState::InspectResult => 6,
            EmulatorState::Tick => 7,
            EmulatorState::EnterDecode => 8,
            EmulatorState::InvokeHandler(handler_id) => 9 | (handler_id.to_u64().unwrap() << 8),
            EmulatorState::CheckTrace => 10,
            EmulatorState::EnterInterrupt => 11,
            EmulatorState::PerformanceMeasurements => 12,
            EmulatorState::RunLoop => 13,
            EmulatorState::Halt => 14,
            EmulatorState::ExecuteJittedBlock => 15,
            EmulatorState::TickAndUpdate => 16,
            EmulatorState::ExecuteDispatch => 17,
            EmulatorState::MakeChains => 18,
        }
    }

    fn decode(val: u64) -> Self {
        match val & 0xff {
            0 => EmulatorState::Other,
            1 => EmulatorState::DecodeEncoding,
            2 => EmulatorState::CacheLookupFirst(LookupProgress::decode(val >> 8)),
            3 => EmulatorState::CacheLookupNext(LookupProgress::decode(val >> 8)),
            4 => EmulatorState::ExecuteBlock,
            5 => EmulatorState::ExecuteSingleEncoding,
            6 => EmulatorState::InspectResult,
            7 => EmulatorState::Tick,
            8 => EmulatorState::EnterDecode,
            9 => EmulatorState::InvokeHandler(HandlerId::from_u64(val >> 8).unwrap()),
            10 => EmulatorState::CheckTrace,
            11 => EmulatorState::EnterInterrupt,
            12 => EmulatorState::PerformanceMeasurements,
            13 => EmulatorState::RunLoop,
            14 => EmulatorState::Halt,
            15 => EmulatorState::ExecuteJittedBlock,
            16 => EmulatorState::TickAndUpdate,
            17 => EmulatorState::ExecuteDispatch,
            18 => EmulatorState::MakeChains,
            _ => unreachable!(),
        }
    }
}

pub type DefaultJitBackend = InkwellBackend<'static>;

pub struct EmulatorContext<'mem, 'tag> {
    inner: Pin<Box<EmulatorContextInner<'mem, 'tag>>>,
}

pub struct EmulatorContextInner<'mem, 'tag> {
    emulator: Emulator<'mem, 'tag>,
    num_chains_executed: usize,
    profiler: Profiler<EmulatorState>,
    emulator_entry_count: u64,
    semantics: Arc<PackedInstrSem>,
    verify_icache_consistency: bool,
    pagejit_enabled: bool,
    break_on_int_fe: bool,
    trace_mem_at: Option<u32>,
    trace_pmem_at: Option<u32>,
    synchronous_clock: Option<SynchronousClock>,
}

pub struct Emulator<'mem, 'tag> {
    pub(crate) ctx: ExecutionContext<'mem, 'tag, Intel386>,
    pub(crate) cpu: State,
    trace_limit: u64,
    paging_enabled: bool,
    op_size: Db,
    next_expected_interrupt: Option<u8>,
    print_at: Vec<u64>,
    path: FixedRingbuf<32, (u64, Instruction)>,
    is_halted: bool,
    execution_counts: Vec<u32>,
    execution_duration: Vec<u64>,
    segment_sizes: SegmentSizes,
    generic_debugging_limit: u64,
    trace_triggered_interrupt: Option<u8>,
    measure_single_encoding_execution: bool,
    profiling_enabled: bool,
    num_cr3_changes: u64,
    num_cr3_reloads: u64,
    num_interrupts_entered: u64,
    skip_trace_differences_before: u64,

    /// Only pub(crate) so code generation can compute the offset.
    pub(crate) intr: Intr,
    interrupts_enabled: bool,
}

// TODO: Use bitpacking magic
// fn pack_bits(x: u64) -> u8 {
//     let mask: u64 = 0x0101_0101_0101_0101; // mask for bits 0,8,16,...,56
//     let y = x & mask;

//     // multiply by 0x0102040810204081 to bring each selected bit into a single byte
//     let packed = ((y.wrapping_mul(0x0102_0408_1020_4080)) >> 56) as u8;

//     packed
// }

// fn unpack_bits(n: u8) -> u64 {
//     (0..8).map(|b| ((n >> b) as u64 & 1) << (b * 8)).reduce(|a, b| a | b).unwrap()
// }

// fn main() {
//     for n in 0..=255 {
//         let bits = unpack_bits(n);
//         println!("0x{bits:016X} = {:08b}", pack_bits(bits));
//         assert_eq!(bits, unpack_bits(pack_bits(bits)))
//     }
// }

const BYTE0_PACKER: BitPacker = BitPacker::new({
    use Intel386Flag::*;
    let mut m = [None; 8];
    m[Cf as usize] = Some(0);
    m[Pf as usize] = Some(2);
    m[Af as usize] = Some(4);
    m[Zf as usize] = Some(6);
    m[Sf as usize] = Some(7);
    m
});

const BYTE1_PACKER: BitPacker = BitPacker::new({
    use Intel386Flag::*;
    let mut m = [None; 8];
    m[Tf as usize - 5] = Some(0);
    m[If as usize - 5] = Some(1);
    m[Df as usize - 5] = Some(2);
    m[Of as usize - 5] = Some(3);
    m[Nt as usize - 5] = Some(6);
    m
});

const BYTE2_PACKER: BitPacker = BitPacker::new({
    use Intel386Flag::*;
    let mut m = [None; 8];
    m[Rf as usize - 8] = Some(0);
    m[Vm as usize - 8] = Some(1);
    m[Ac as usize - 8] = Some(2);
    m[Vif as usize - 8] = Some(3);
    m[Vip as usize - 8] = Some(4);
    m[Id as usize - 8] = Some(5);
    m
});

pub fn pack_flags(cpu: &State) -> u32 {
    let f1 = cpu.gpreg(GpReg::Flags1);
    let f2 = cpu.gpreg(GpReg::Flags2);
    // contains the upper 3 flags in f1, and the lower 5 flags in f2.
    let f12_offset_5 = (f1 >> (5 * 8)) | (f2 << (3 * 8));
    let byte0 = BYTE0_PACKER.pack(f1);
    let byte1 = BYTE1_PACKER.pack(f12_offset_5);
    let byte2 = BYTE2_PACKER.pack(f2);

    0x0002 | byte0 as u32 | ((byte1 as u32) << 8) | ((byte2 as u32) << 16) | ((cpu.gpreg(GpReg::Iopl) as u32 & 3) << 12)
}

pub fn unpack_flags(cpu: &mut State, flags: u32, unpack_upper: bool, return_from_vm: bool) {
    let f1 = BYTE0_PACKER.unpack(flags as u8);
    let f12_offset_5 = BYTE1_PACKER.unpack((flags >> 8) as u8);
    let f2 = BYTE2_PACKER.unpack((flags >> 16) as u8);

    let flags1 = f1 | (f12_offset_5 << (5 * 8));
    let flags2 = f2 | (f12_offset_5 >> (3 * 8));
    cpu.set_gpreg(GpReg::Flags1, flags1);
    // cpu.set_flag(Intel386Flag::Tf, flags & (1 << 8) != 0);
    // cpu.set_flag(Intel386Flag::Nt, flags & (1 << 14) != 0);

    let flag_mask = {
        let mut mask = 0;
        mask |= 1 << ((Intel386Flag::Tf as u32 - 8) * 8);
        mask |= 1 << ((Intel386Flag::Nt as u32 - 8) * 8);

        if unpack_upper {
            mask |= 1 << ((Intel386Flag::Rf as u32 - 8) * 8);
            mask |= 1 << ((Intel386Flag::Ac as u32 - 8) * 8);
            mask |= 1 << ((Intel386Flag::Id as u32 - 8) * 8);

            if !return_from_vm {
                mask |= 1 << ((Intel386Flag::Vm as u32 - 8) * 8);
                mask |= 1 << ((Intel386Flag::Vif as u32 - 8) * 8);
                mask |= 1 << ((Intel386Flag::Vip as u32 - 8) * 8);
            }
        }

        mask
    };

    let old_flags2 = cpu.gpreg(GpReg::Flags2);
    cpu.set_gpreg(GpReg::Flags2, (old_flags2 & !flag_mask) | (flags2 & flag_mask));

    if !return_from_vm {
        cpu.set_gpreg(GpReg::Iopl, (flags >> 12) as u64 & 3);
    }

    // if unpack_upper {
    //     cpu.set_flag(Intel386Flag::Rf, flags & (1 << 16) != 0);
    //     cpu.set_flag(Intel386Flag::Ac, flags & (1 << 18) != 0);

    //     if !return_from_vm {
    //         cpu.set_flag(Intel386Flag::Vm, flags & (1 << 17) != 0);
    //         cpu.set_flag(Intel386Flag::Vif, flags & (1 << 19) != 0);
    //         cpu.set_flag(Intel386Flag::Vip, flags & (1 << 20) != 0);
    //     }

    //     cpu.set_flag(Intel386Flag::Id, flags & (1 << 21) != 0);
    // }
}

macro_rules! expand_bool {
    ($val:expr => |$const:ident| $block:block) => {{
        if $val {
            #[allow(unused)]
            const $const: bool = true;
            $block
        } else {
            #[allow(unused)]
            const $const: bool = false;
            $block
        }
    }};
}

macro_rules! expand_bools {
    ([ $val:expr => $const:ident $(, $($nextval:expr => $nextconst:ident),*)? $(,)* ] => $block:block) => {{
        expand_bool!($val => |$const| {
            expand_bools!([ $($($nextval => $nextconst),*)? ] => { $block })
        })
    }};
    ([ $(,)* ] => { $block:block }) => {{
        $block
    }};
}

impl CurrentState for (&State, &Mem32) {
    #[inline(always)]
    fn cs_base(&self) -> u32 {
        self.0.gpreg(GpReg::CsBase) as u32
    }

    #[inline(always)]
    fn ip(&self) -> u32 {
        self.0.gpreg(GpReg::Ip) as u32
    }

    #[inline(always)]
    fn memory(&self) -> &Mem32 {
        self.1
    }
}

impl<'mem, 'tag> EmulatorContext<'mem, 'tag> {
    pub fn new(
        memory: &'mem Mem32, semantics: Arc<PackedInstrSem>, mut cpu: State, hw_builder: impl FnOnce(IntrHandle) -> Hw,
        guard: Guard<'tag>,
    ) -> Self {
        cpu.set_gpreg(GpReg::Cr0, 0x6000_0010);
        cpu.set_gpreg(GpReg::GdtLimit, 0xffff);
        cpu.set_gpreg(GpReg::IdtLimit, 0xffff);
        cpu.set_gpreg(GpReg::Dr6, 0xFFFF0FF0);
        cpu.set_gpreg(GpReg::Dr7, 0x00000400);

        for sreg in [GpReg::Cs, GpReg::Ds, GpReg::Ss, GpReg::Es, GpReg::Fs, GpReg::Gs] {
            // TODO: Just load the entire selectors by using `PreparedSegment` or equivalent.
            let desc = Descriptor::from_real_mode_selector(cpu.gpreg(sreg) as u16);
            let (base, limit, ar) = sreg.related_segment_regs();
            cpu.set_gpreg(base, desc.base() as u64);
            cpu.set_gpreg(limit, desc.effective_limit_taking_direction_into_account() as u64);
            cpu.set_gpreg(ar, u64::from(CachedDescriptorAccessRights::from(desc)) | 1);
        }

        let backend = InkwellBackend::new(Box::leak(Box::new(InkwellContext::new())));

        // Here, we construct a pointer to the INTR atomic before constructing the rest of the structure.
        // The INTR value is checked very often, dozens of millions of times per second.
        // This means that a memory indirection is very costly.
        // To avoid this, we will do unsafe magic to make sure we can place the INTR and still hand out references to it.
        // Effectively, we're constructing a self-referential struct, which is why unsafe is needed.
        // The Pin ensures that the memory is never moved.
        let mut uninit = Pin::new(Box::new(MaybeUninit::<EmulatorContextInner>::uninit()));
        let ptr = uninit.as_ptr();

        // SAFETY: Byte offset is within EmulatorContextInner allocation.
        let intr_ptr = unsafe { ptr.byte_add(offset_of!(EmulatorContextInner, emulator.intr)) as *const Intr };

        // SAFETY: This is the exact location where Intr will be placed inside the EmulatorContextInnter struct.
        let early_intr = unsafe { EarlyIntr::from_ptr(intr_ptr) };

        let intr_handle = unsafe { early_intr.handle() };
        let hw = hw_builder(intr_handle.clone());

        // SAFETY: The mut pointer is only used for writing, which is safe when the memory is uninitialized.
        unsafe {
            uninit.as_mut_ptr().write(EmulatorContextInner {
                emulator: Emulator {
                    cpu,
                    paging_enabled: false,
                    op_size: Db::Protected16,
                    next_expected_interrupt: None,
                    trace_triggered_interrupt: None,
                    trace_limit: 0,
                    print_at: Vec::new(),
                    path: FixedRingbuf::new_with(|| (0, Instruction::new(&[]))),
                    is_halted: false,
                    execution_counts: vec![0; semantics.len()],
                    execution_duration: vec![0; semantics.len()],
                    ctx: ExecutionContext::new(
                        hw,
                        memory,
                        None,
                        InstructionCache::new(
                            guard,
                            semantics.clone(),
                            SingleEncodingExecution::new(backend, semantics.len()),
                        ),
                    ),
                    segment_sizes: SegmentSizes::Cs16Ss16,
                    generic_debugging_limit: u64::MAX,
                    measure_single_encoding_execution: false,
                    profiling_enabled: false,
                    num_cr3_changes: 0,
                    num_cr3_reloads: 0,
                    num_interrupts_entered: 0,
                    skip_trace_differences_before: 0,
                    intr: early_intr.build(),
                    interrupts_enabled: true,
                },
                num_chains_executed: 0,
                profiler: Profiler::new(&EmulatorState::Other),
                emulator_entry_count: 0,
                semantics,
                verify_icache_consistency: false,
                pagejit_enabled: true,
                break_on_int_fe: false,
                trace_mem_at: None,
                trace_pmem_at: None,
                synchronous_clock: None,
                // verifier: Some(Verifier::new()),
            });
        }

        // SAFETY: We initialized this memory above, so it is all valid.
        // SAFETY: MaybeUninit is guaranteed to have the same size, alignment, and ABI as T, so this transmute is safe.
        let mut inner = unsafe {
            std::mem::transmute::<
                Pin<Box<MaybeUninit<EmulatorContextInner<'mem, 'tag>>>>,
                Pin<Box<EmulatorContextInner<'mem, 'tag>>>,
            >(uninit)
        };

        assert_eq!(inner.emulator.intr.count.as_ptr(), intr_handle.count_ptr());

        inner.emulator.hw_mut().start_clock();

        Self {
            inner,
        }
    }

    pub fn set_semantics(&mut self, semantics: Arc<PackedInstrSem>) {
        let backend = InkwellBackend::new(Box::leak(Box::new(InkwellContext::new())));

        self.inner.semantics = semantics.clone();
        self.inner.emulator.execution_counts = vec![0; semantics.len()];
        self.inner.emulator.execution_duration = vec![0; semantics.len()];

        let see = SingleEncodingExecution::new(backend, semantics.len());
        self.inner.emulator.ctx.mmio_ctx.icache.set_semantics(semantics, see);
        self.inner.emulator.ctx.memory.clean_all_phys_frames();
        self.inner.emulator.ctx.memory.invalidate_all_pages();
    }

    pub fn snapshot(&mut self) -> EmulatorSnapshot {
        self.inner.emulator.snapshot()
    }

    pub fn restore(&mut self, snapshot: EmulatorSnapshot) {
        self.inner.emulator.restore(snapshot);
    }

    pub fn set_measure_single_encoding_execution(&mut self, measure: bool) {
        self.inner.emulator.measure_single_encoding_execution = measure;
    }

    pub fn set_generic_debugging_limit(&mut self, num: u64) {
        self.inner.emulator.generic_debugging_limit = num;
    }

    pub fn set_profiling(&mut self, enabled: bool) {
        self.inner.emulator.profiling_enabled = enabled;
    }

    pub fn k(&self) -> u64 {
        self.inner.emulator.ctx.k
    }

    pub fn disable_interrupts(&mut self) {
        self.inner.emulator.interrupts_enabled = false;
    }

    pub fn disable_es1370(&mut self) {
        self.inner.emulator.hw_mut().disable_es1370();
    }

    pub fn run(&mut self, num: Option<u64>) {
        self.inner.run(num)
    }

    pub fn set_trace(&mut self, trace: TraceEntryReader<TraceType>, limit: u64) {
        self.inner.emulator.ctx.trace = Some(trace);
        self.inner.emulator.trace_limit = limit;
    }

    pub fn set_print_at(&mut self, print_at: Vec<u64>) {
        self.inner.emulator.print_at = print_at;

        // The lowest numbers should be at the end of the array
        self.inner.emulator.print_at.sort_by_key(|n| u64::MAX - n);
    }

    pub fn set_verify_icache_consistency(&mut self, enable: bool) {
        self.inner.verify_icache_consistency = enable;
    }

    pub fn set_pagejit_enabled(&mut self, enable: bool) {
        self.inner.pagejit_enabled = enable;
    }

    pub fn emulator(&mut self) -> &mut Emulator<'mem, 'tag> {
        &mut self.inner.emulator
    }

    pub fn pause(&mut self) {
        self.inner.emulator.hw_mut().pause();
    }

    pub fn set_break_on_int_fe(&mut self, enable: bool) {
        self.inner.break_on_int_fe = enable;
    }

    pub fn set_skip_trace_differences_before(&mut self, n: u64) {
        self.inner.emulator.skip_trace_differences_before = n;
    }

    pub fn set_trace_mem_at(&mut self, addr: u32) {
        self.inner.trace_mem_at = Some(addr);
    }

    pub fn set_trace_pmem_at(&mut self, addr: u32) {
        self.inner.trace_pmem_at = Some(addr);
    }

    pub fn reset_k(&mut self) {
        self.inner.emulator.ctx.k = 0;
        self.inner.emulator.ctx.jit_k = 0;
    }

    pub fn set_synchronous_clock(&mut self, clock: SynchronousClock) {
        self.inner.synchronous_clock = Some(clock);
    }
}

impl<'mem, 'tag> EmulatorContextInner<'mem, 'tag> {
    pub fn most_executed_encodings(&self) -> impl Iterator<Item = (usize, u32)> {
        self.emulator
            .execution_counts
            .iter()
            .copied()
            .enumerate()
            .sorted_by_key(|(_, val)| Reverse(*val))
    }

    pub fn encodings_most_time_taken(&self) -> impl Iterator<Item = (usize, u64)> {
        self.emulator
            .execution_duration
            .iter()
            .copied()
            .enumerate()
            .sorted_by_key(|(_, val)| Reverse(*val))
    }

    pub fn run(&mut self, num: Option<u64>) {
        // Disable trace if we have already passed the limit
        if self.emulator.ctx.k > self.emulator.trace_limit {
            self.emulator.ctx.trace = None;
        }

        trait Decide {
            type Output;
            fn get(self) -> Self::Output;
        }

        struct CondIf<const VAL: bool, True, False>(True, False);

        impl<T, True: FnOnce() -> T, False> Decide for CondIf<true, True, False> {
            type Output = T;

            fn get(self) -> Self::Output {
                (self.0)()
            }
        }

        impl<T, True, False: FnOnce() -> T> Decide for CondIf<false, True, False> {
            type Output = T;

            fn get(self) -> Self::Output {
                (self.1)()
            }
        }

        let mut exit = false;
        #[allow(unused_mut)] // may warn because of disabled features
        let mut clock = self.synchronous_clock.take();
        let mut perfmon = PerformanceMonitor::new(self.emulator.ctx.k);

        while !exit {
            exit = expand_bools!([
                self.emulator.ctx.trace.is_some() => CHECK_TRACE,
                !self.emulator.print_at.is_empty() => HAS_PRINT_AT,
                self.emulator.measure_single_encoding_execution => MEASURE_SEE,
                self.emulator.profiling_enabled => PROFILE,
                // self.verify_icache_consistency => VERIFY_ICACHE_CONSISTENCY,
                self.trace_mem_at.is_some() => HAS_MEM_TRACE,
                self.trace_pmem_at.is_some() => HAS_PMEM_TRACE,
                num.is_some() => STOP_AT_EXACT,
                clock.is_some() => SYNCHRONOUS_CLOCK,
            ] => {
                #[cfg(feature = "synchronous-clock")]
                let clock = clock.as_mut();
                #[allow(clippy::let_unit_value)]
                let mut hook = {
                    let hook = ();
                    // let hook = PrintEveryInstr;
                    let hook = (Decide::get(CondIf::<STOP_AT_EXACT, _, _>(|| StopAt::new(num.unwrap()), || ())), hook);

                    #[cfg(feature = "synchronous-clock")]
                    let hook = (Decide::get(CondIf::<SYNCHRONOUS_CLOCK, _, _>(move || hooks::SyncClock::new(clock.unwrap()), || ())), hook);
                    let hook = (Decide::get(CondIf::<{CHECK_TRACE || HAS_PRINT_AT}, _, _>(|| TrackPath, || ())), hook);
                    let hook = (Decide::get(CondIf::<CHECK_TRACE, _, _>(CheckTrace::new, || ())), hook);
                    let hook = (Decide::get(CondIf::<HAS_PRINT_AT, _, _>(|| Printer::new(self), || ())), hook);
                    #[cfg(feature = "profiler")]
                    let hook = (Decide::get(CondIf::<PROFILE, _, _>(|| hooks::EnableProfiler, || ())), hook);
                    #[cfg(feature = "mem-trace")]
                    let hook = (Decide::get(CondIf::<HAS_MEM_TRACE, _, _>(|| hooks::MemTrace::new_lin(self.trace_mem_at.unwrap()), || ())), hook);
                    #[cfg(feature = "mem-trace")]
                    let hook = (Decide::get(CondIf::<HAS_PMEM_TRACE, _, _>(|| hooks::MemTrace::new_phys(self.trace_pmem_at.unwrap()), || ())), hook);
                    // // // let hook = (DirtyTracker { num_dirty_before: 0 }, hook);
                    // // let hook = (Decide::get(CondIf::<VERIFY_ICACHE_CONSISTENCY, _, _>(CheckInstructionCacheConsistency, ())), hook);

                    (Decide::get(CondIf::<MEASURE_SEE, _, _>(MeasureSingleEncodingExecution::new, || ())), hook)
                };

                self.run_inner::<PROFILE, CHECK_TRACE>(num, &mut hook, &mut perfmon)
            });
        }

        self.synchronous_clock = clock;
    }

    /// Returns true when execution should be terminated, false when hooks need to be updated but execution can continue.
    fn run_inner<const PROFILE: bool, const CHECK_TRACE: bool>(
        &mut self, num: Option<u64>, hooks: &mut impl ExecutionHook, perfmon: &mut PerformanceMonitor,
    ) -> bool {
        self.emulator.is_halted = false;
        let mut num_halted = 0u64;
        let mut halt_time = Duration::ZERO;
        let mut halt_start = Instant::now();

        assert!(!hooks.can_be_discarded(self));

        self.emulator
            .ctx
            .mmio_ctx
            .icache
            .set_page_jit_enabled(hooks.allow_multiple_instr_single_function() && self.pagejit_enabled);

        // TODO: add can_discard() function to hooks that returns true when a hook has become unneeded. In that case, break to outer and enter the correct path again.

        let mut is_entry = true;
        while self.emulator.ctx.k < num.unwrap_or(u64::MAX) {
            self.emulator.ctx.mmio_ctx.icache.periodic_work();

            if !self.emulator.is_halted {
                self.profiler.at::<PROFILE>(&EmulatorState::RunLoop);
                let result = self.enter_emulation(hooks, is_entry);

                match result {
                    Ok(_) => is_entry = false,
                    Err(int) => {
                        self.profiler.at::<PROFILE>(&EmulatorState::EnterInterrupt);
                        is_entry = true;
                        if matches!(int, Interrupt::Exception(Exception::InvalidOpcode)) {
                            debug!("Encountered InvalidOpcode exception at k={}", DisplayK(self.emulator.ctx.k));
                        }

                        if let Interrupt::SoftwareInterrupt {
                            vector, ..
                        } = int
                            && vector == 0xfe
                            && self.break_on_int_fe
                        {
                            return true
                        }

                        if let Interrupt::Exception(Exception::PageFault {
                            address, ..
                        }) = int
                            && address == 0
                        {
                            warn!(
                                "Page fault at NULL (k={})\n{}",
                                DisplayK(self.emulator.ctx.k),
                                self.emulator.cpu
                            );
                            let id = {
                                let cs_base = self.emulator.cpu.gpreg(GpReg::CsBase);
                                let ip = self.emulator.cpu.gpreg(GpReg::Ip);

                                let context_flags = self.build_context_flags();
                                self.emulator
                                    .ctx
                                    .mmio_ctx
                                    .icache
                                    .lookup_first(
                                        cs_base as u32,
                                        ip as u32,
                                        context_flags,
                                        self.emulator.ctx.memory,
                                        |p| hooks.at(&mut self.profiler, EmulatorState::CacheLookupFirst(p)),
                                        is_entry,
                                    )
                                    .unwrap()
                            };

                            let info = self.emulator.ctx.mmio_ctx.icache.encoding_info(id);
                            let cs_base = self.emulator.cpu.gpreg(GpReg::CsBase);
                            let ip = self.emulator.cpu.gpreg(GpReg::Ip);
                            let pc = cs_base.wrapping_add(ip);
                            let instr = self.emulator.ctx.mmio_ctx.icache.resolve_instr_for(
                                id,
                                LinAddr::new(pc as u32).into(),
                                self.emulator.ctx.memory,
                            );
                            warn!(
                                "CPU was executing 0x{:X}:0x{:X} ({} instrs executed)",
                                self.emulator.cpu.gpreg(GpReg::CsBase),
                                self.emulator.cpu.gpreg(GpReg::Ip),
                                self.emulator.ctx.k
                            );
                            warn!("Executing: {instr:X}\n{}", info.display_instance(self.semantics.as_ref()));

                            let mut page_contents = [0; 4096];
                            self.emulator
                                .ctx
                                .memory
                                .read_slice(pc as u32 & !0xfff, &mut page_contents, false, &mut ())
                                .ok();
                            warn!("Page contents: {:02X}", page_contents.iter().format(""));
                            warn!("Path: {:X?}", self.emulator.path);
                        }

                        if log_enabled!(log::Level::Debug) {
                            debug!("About to enter {int:X?}");
                            if let Ok(id) = {
                                let cs_base = self.emulator.cpu.gpreg(GpReg::CsBase);
                                let ip = self.emulator.cpu.gpreg(GpReg::Ip);

                                let context_flags = self.build_context_flags();
                                self.emulator.ctx.mmio_ctx.icache.lookup_first(
                                    cs_base as u32,
                                    ip as u32,
                                    context_flags,
                                    self.emulator.ctx.memory,
                                    |p| hooks.at(&mut self.profiler, EmulatorState::CacheLookupFirst(p)),
                                    is_entry,
                                )
                            } {
                                let info = self.emulator.ctx.mmio_ctx.icache.encoding_info(id);
                                let cs_base = self.emulator.cpu.gpreg(GpReg::CsBase);
                                let ip = self.emulator.cpu.gpreg(GpReg::Ip);
                                let pc = cs_base.wrapping_add(ip);
                                let instr = self.emulator.ctx.mmio_ctx.icache.resolve_instr_for(
                                    id,
                                    LinAddr::new(pc as u32).into(),
                                    self.emulator.ctx.memory,
                                );
                                debug!(
                                    "CPU was executing 0x{:X}:0x{:X} ({} instrs executed)",
                                    self.emulator.cpu.gpreg(GpReg::CsBase),
                                    self.emulator.cpu.gpreg(GpReg::Ip),
                                    self.emulator.ctx.k
                                );
                                debug!("Executing: {instr:X}\n{}", info.display_instance(self.semantics.as_ref()));

                                let mut page_contents = [0; 4096];
                                self.emulator
                                    .ctx
                                    .memory
                                    .read_slice(pc as u32 & !0xfff, &mut page_contents, false, &mut ())
                                    .ok();
                                debug!("Page contents: {:02X}", page_contents.iter().format(""));
                                debug!("Path: {:X?}", self.emulator.path);
                            }
                        }

                        match self.emulator.enter_interrupt(int) {
                            Ok(_) => {
                                if CHECK_TRACE {
                                    if let Interrupt::SoftwareInterrupt {
                                        vector, ..
                                    } = int
                                    {
                                        info!(target: extend_path_with!("int"), "execute_one_instr terminated with software interrupt 0x{vector:X}, checking trace now");
                                        let entry = EncodingInfo {
                                            instr_len: 2,
                                            part_values: PartValues::ALL_ZERO,
                                            encoding_index: 0,
                                        };
                                        if let Err(e) = self.emulator.check_trace(
                                            None,
                                            Instruction::new(&[0xCD, vector]),
                                            &entry,
                                            &mut ArrayVec::new(),
                                            false,
                                            &*self.semantics,
                                        ) {
                                            self.emulator
                                                .enter_interrupt(e)
                                                .expect("trace interrupts should not trigger exceptions");
                                        }
                                    } else if let Interrupt::Exception(Exception::DeviceNotAvailable) = int {
                                        // Bochs will *sometimes* execute the entire WAIT instruction, THEN generate an error.
                                        // TODO: Detect whether Bochs is expecting a WAIT as the next instruction, and only then do an extra trace check.
                                        info!(
                                            "TODO: execute_one_instr terminated with #DeviceNotAvailable, should maybe do an extra trace check"
                                        );
                                        // if let Err(e) = self.emulator.check_trace(Instruction::new(&[ 0x9B ]), &mut ArrayVec::new(), &MiniSem {
                                        //     name: String::new(),
                                        //     addresses: MemoryAccesses {
                                        //         memory: Vec::new(),
                                        //         use_trap_flag: false,
                                        //     },
                                        //     commands: il::Commands::Ops(Vec::new()),
                                        // }, false) {
                                        //     self.emulator.enter_interrupt(e).expect("trace interrupts should not trigger exceptions");
                                        // }
                                    }
                                }
                            },
                            Err(e) => {
                                // TODO: double and triple faults
                                self.emulator.enter_interrupt(e).expect("TODO: handle double fault");
                            },
                        };
                    },
                };

                if self.emulator.is_halted {
                    halt_start = Instant::now();
                }
            } else {
                self.profiler.at::<PROFILE>(&EmulatorState::Halt);
                num_halted += 1;
                if num_halted.is_multiple_of(1 << 20) {
                    info!("HALT: {num_halted}");
                }

                if self.emulator.ctx.trace.is_some() {
                    self.emulator.is_halted = false;
                } else {
                    self.profiler.at::<PROFILE>(&EmulatorState::Halt);
                    if !self.emulator.interrupts_enabled {
                        panic!("HALT executed while interrupts are disabled");
                    }

                    while !self.emulator.interrupt_pending() {
                        if !hooks.halt() {
                            std::thread::sleep(Duration::from_millis(1));
                        }

                        if self.emulator.ctx.mmio_ctx.update() {
                            return true
                        }
                    }
                }
            }

            self.profiler.at::<PROFILE>(&EmulatorState::TickAndUpdate);
            if self.emulator.ctx.mmio_ctx.update() {
                return true
            }
            self.emulator.ctx.mmio_ctx.icache.receive_compiled_chains();

            if hooks.can_be_discarded(self) {
                return false
            }

            if self.emulator.interrupt_pending()
                && self.emulator.cpu().flag(Intel386Flag::If)
                && self.emulator.ctx.trace.is_none()
                && self.emulator.interrupts_enabled
            {
                self.profiler.at::<PROFILE>(&EmulatorState::EnterInterrupt);
                self.emulator.ctx.mmio_ctx.hw.clear_periodic_intr();
                if let Some(next_interrupt) = self.emulator.hw().check_interrupt() {
                    // TODO: Because of race conditions we can'tc assume INTR is atomically asserted together with a pending interrupt.
                    // assert!(interrupt_was_pending, "INTR should be raised when an interrupt is pending (INT 0x{next_interrupt:X}, offsets = {:02X?})", self.emulator.hw().vector_offsets());
                    info!("Servicing pending interrupt: 0x{next_interrupt:X}");
                    if self.emulator.is_halted {
                        halt_time += halt_start.elapsed();
                    }
                    self.emulator.is_halted = false;

                    self.emulator
                        .enter_interrupt(Interrupt::HardwareInterrupt(next_interrupt))
                        .expect("TODO: Handle exceptions in run()");
                }
            }

            self.profiler.at::<PROFILE>(&EmulatorState::PerformanceMeasurements);
            if perfmon.should_print() {
                if self.emulator.is_halted {
                    halt_time += halt_start.elapsed();
                    halt_start = Instant::now();
                };

                perfmon.update(halt_time, self);

                halt_time = Duration::ZERO;
            }
        }

        true
    }

    #[inline(always)]
    fn build_context_flags(&self) -> ContextFlags {
        ContextFlags::build(
            // Written via CR0, which invokes a handler
            // Protected mode ifs written via CR0, which invokes a handler.
            // VM is set via an IRET, which also invokes a handler.
            self.emulator.ctx.protected_mode && !self.emulator.cpu.flag(Intel386Flag::Vm),
            // Returns whether CPL != 0, which is only set upon interrupt entry/irets
            self.emulator.cpu.is_userspace(),
            // Can only change when CS changes, which invokes a handler.
            self.emulator.segment_sizes,
        )
    }

    /// `is_entry` indicates whether the current CS:IP is an entry point of an interrupt.
    #[inline(never)]
    fn enter_emulation(&mut self, hooks: &mut impl ExecutionHook, mut is_entry: bool) -> Result<bool, Interrupt> {
        hooks.at(&mut self.profiler, EmulatorState::EnterDecode);
        self.emulator_entry_count += 1;

        let can_accept_interrupts = self.emulator.cpu().flag(Intel386Flag::If) && hooks.can_accept_interrupts(self);
        if self.emulator.interrupt_pending() && can_accept_interrupts && self.emulator.interrupts_enabled {
            return Ok(false)
        }

        let mut context_flags = self.build_context_flags();
        'outer: loop {
            let mut id = {
                let cs_base = self.emulator.cpu.gpreg(GpReg::CsBase);
                let ip = self.emulator.cpu.gpreg(GpReg::Ip);

                self.emulator.ctx.mmio_ctx.icache.lookup_first(
                    cs_base as u32,
                    ip as u32,
                    context_flags,
                    self.emulator.ctx.memory,
                    |p| hooks.at(&mut self.profiler, EmulatorState::CacheLookupFirst(p)),
                    is_entry,
                )?
            };

            is_entry = false;

            // In this main loop we repeatedly execute individual instructions or compiled blocks of instructions.
            // The icache determines how and when blocks are compiled.
            // We use `InstructionCache::find_next` at the end of the loop.
            // This ensures that the icache is aware of the sequence in which instructions are executed.
            // The icache uses this to collect instructions when a block is compiled,
            // when it is impossible to derive the next instruction pointer from the semantics.
            // (this happens for example when executing a return, because we don't know the contents of the stack)
            loop {
                hooks.at(&mut self.profiler, EmulatorState::EnterDecode);
                hooks.before_execute(self, id);

                #[cfg(debug_assertions)]
                {
                    let cs_base = self.emulator.cpu.gpreg(GpReg::CsBase);
                    let ip = self.emulator.cpu.gpreg(GpReg::Ip);
                    let pc = cs_base.wrapping_add(ip) as u32;

                    if self.emulator.paging_enabled {
                        let result = self.emulator.ctx.memory.page_walk(pc, false);
                        assert!(matches!(result, PageWalkResult::PhysAddr { .. }));

                        let instr_len = self.emulator.ctx.mmio_ctx.icache.encoding_info(id).instr_len;
                        if (pc & 0xfff) + instr_len as u32 > 4096 {
                            let result = self
                                .emulator
                                .ctx
                                .memory
                                .page_walk(pc.wrapping_add(instr_len as u32 - 1), false);
                            assert!(matches!(result, PageWalkResult::PhysAddr { .. }));
                        }
                    }

                    let correct_id = {
                        self.emulator.ctx.mmio_ctx.icache.lookup_first(
                            cs_base as u32,
                            ip as u32,
                            context_flags,
                            self.emulator.ctx.memory,
                            |p| hooks.at(&mut self.profiler, EmulatorState::CacheLookupFirst(p)),
                            false,
                        )?
                    };

                    debug_assert_eq!(
                        id, correct_id,
                        "current id is {id:?}, but should be {correct_id:?} at 0x{cs_base:X}:0x{ip:X}\n\n{}",
                        self.emulator.cpu
                    );
                }

                hooks.at(&mut self.profiler, EmulatorState::EnterDecode);
                let result = match self.emulator.ctx.mmio_ctx.icache.executable_entry(id) {
                    Executable::JittedPage {
                        page,
                    } => {
                        hooks.at(&mut self.profiler, EmulatorState::ExecuteBlock);

                        let f = page.function();
                        self.emulator.ctx.jit_k = self.emulator.ctx.jit_k.wrapping_sub(self.emulator.ctx.k);

                        let (result, exit_token) = f.dispatch(&mut self.emulator);

                        self.emulator.ctx.jit_k = self.emulator.ctx.jit_k.wrapping_add(self.emulator.ctx.k);
                        self.num_chains_executed += 1;

                        // TODO: This doesn't check if the chain is still the same chain that we started with.
                        let Executable::JittedPage {
                            page,
                        } = self.emulator.ctx.mmio_ctx.icache.executable_entry(id)
                        else {
                            if !result.can_continue_execution() {
                                self.inspect_result(true, hooks, None)?;
                                context_flags = self.build_context_flags();
                                if self.emulator.is_halted && hooks.can_halt(self) {
                                    return Ok(true)
                                }
                            }

                            break
                        };
                        id = page.resolve_exit_token(exit_token);

                        result
                    },
                    Executable::Single {
                        part_values,
                        instr_len,
                        execute,
                    } => {
                        // TODO: Turn into hook
                        // if self.emulator.ctx.mmio_ctx.icache.frame_is_jitted(id) && self.emulator.ctx.mmio_ctx.icache.entry_phys_addr(id).frame_offset() + instr_len as u16 <= 0x1000 {
                        //     println!("No JITed entry available at 0x{:X}:0x{:X} (phys={})", self.emulator.cpu.gpreg(GpReg::CsBase), self.emulator.cpu.gpreg(GpReg::Ip), self.emulator.ctx.mmio_ctx.icache.entry_phys_addr(id));
                        // }
                        // There should be as little code as possible between here and execute_one_instr.
                        // In particular, there should be no memory writes.
                        // This will allow the compiler to generate a call [..] for the function execution.
                        // If there are memory writes it needs to load the value into a register here already.
                        hooks.at(&mut self.profiler, EmulatorState::ExecuteSingleEncoding);

                        hooks.before_encoding_execution(self, id);

                        let result = {
                            execute.execute_uninstantiated(&mut self.emulator, instr_len, part_values, |_| ())
                            // let info = self.emulator.ctx.mmio_ctx.icache.encoding_info(id);
                            // let instr = self.emulator.ctx.mmio_ctx.icache.resolve_instr_for(id, self.emulator.ctx.memory);
                            // self.emulator.execute_instr_interpreter(false, instr, &info, &*self.semantics)
                        };

                        hooks.after_encoding_execution(self, id);

                        result
                    },
                };

                debug_assert!(result.can_continue_execution() || !result.jump_taken());

                // TODO: The RF flag can only be set by IRETD, so we should execute the first instruction after an IRETD separately so we do not have this extra memory write in the happy path.
                // TODO: Don't clear this if the instruction was IRETD, JMP, CALL, or INT n that causes a task switch.
                self.emulator.cpu.set_flag(Intel386Flag::Rf, false);

                if !result.can_continue_execution() {
                    self.inspect_result(true, hooks, Some(id))?;
                    // TODO: Make sure IRET doesn't mark something as entry point
                    // TODO: Special handling of instruction after IRET, to ensure correct RF behavior.
                    // TODO: If there are any instructions that will update the context flags without triggering a handler, we should fix that.
                    context_flags = self.build_context_flags();
                    if self.emulator.is_halted && hooks.can_halt(self) {
                        return Ok(true)
                    }
                }

                if !hooks.after_execute(self, id)? {
                    println!("Exiting execution loop at k={}", DisplayK(self.emulator.ctx.k));
                    break 'outer
                }

                // TODO: We might have to invoke check_trace here, before returning, in case of the WAIT instruction
                hooks.at(&mut self.profiler, EmulatorState::EnterDecode);
                // If can_continue_execution is false, we had to inspect the result.
                // Given the fact that this function is still running, the result was a handler.
                // This means that we will always return after the previous instruction, thus making the next instruction an entry point.
                id = self.emulator.ctx.mmio_ctx.icache.lookup_next_from_entry(
                    id,
                    (&self.emulator.cpu, self.emulator.ctx.memory),
                    context_flags,
                    |p| hooks.at(&mut self.profiler, EmulatorState::CacheLookupNext(p)),
                    !result.can_continue_execution(),
                    result.jump_taken(),
                )?;

                // Since interrupts are enabled most of the time, it makes more sense to check for interrupts first.
                // This way we only incur the cost of checking IF when an interrupt is actually pending and IF=0.
                // This is only the case briefly in interrupt handlers.
                let can_accept_interrupts = self.emulator.cpu().flag(Intel386Flag::If) && hooks.can_accept_interrupts(self);
                if self.emulator.interrupt_pending() && can_accept_interrupts && self.emulator.interrupts_enabled {
                    return Ok(false)
                }
            }
        }

        Ok(false)
    }

    fn inspect_result(
        &mut self, skip_print: bool, hooks: &mut impl ExecutionHook, last_executed_id: Option<CacheEntryId<'tag>>,
    ) -> Result<(), Interrupt> {
        hooks.at(&mut self.profiler, EmulatorState::InspectResult);
        let result = self.emulator.ctx.result.unpack();
        match &result {
            Ok(ExecResult::Ok) => (),
            Ok(ExecResult::InvokeHandler {
                id,
                args,
            }) => {
                hooks.at(&mut self.profiler, EmulatorState::InvokeHandler(*id));
                if !skip_print {
                    // debug!("Invoking handler {id:?} with args {args:X?} after executing cached block {block:X?}");
                }

                self.emulator.execute_handler(skip_print, *id, args)?;
            },
            Err(e) => {
                if let Some(id) = last_executed_id {
                    let info = self.emulator.ctx.mmio_ctx.icache.encoding_info(id);
                    debug!(target: extend_path_with!("int"), "Exception {e:X?} (k={}) after executing encoding:\n{}", DisplayK(self.emulator.ctx.k), info.display_instance(&*self.semantics));

                    if self.semantics.get(info.encoding_index).semantics.is_rep {
                        // TODO: Check if CX is non-zero
                        self.emulator.cpu.set_flag(Intel386Flag::Rf, true);
                    }
                }

                return Err((*e).into())
            },
        }

        Ok(())
    }
}

#[must_use]
struct PreparedSegment {
    seg: GpReg,
    selector: SegmentSelector,
    descriptor: Descriptor,
    protected_mode: bool,
}

fn compute_effective_cpl(cpl: u8) -> u64 {
    cpl as u64 | (((cpl != 0) as u64) << 8)
}

impl PreparedSegment {
    #[inline(always)]
    pub fn commit(self, em: &mut Emulator<'_, '_>) {
        let (seg_base, seg_limit, seg_ar) = self.seg.related_segment_regs();
        em.cpu.set_gpreg(self.seg, u16::from(self.selector) as u64);

        let base = self.descriptor.base();
        let limit = self.descriptor.effective_limit_taking_direction_into_account();
        let ar = u64::from(CachedDescriptorAccessRights::from(self.descriptor)) | (!self.protected_mode) as u64;
        em.cpu.set_gpreg(seg_base, base as u64);
        em.cpu.set_gpreg(seg_limit, limit as u64);
        em.cpu.set_gpreg(seg_ar, ar);

        if self.seg == GpReg::Cs {
            if self.protected_mode {
                em.op_size = self.descriptor.flags().size();
                em.cpu
                    .set_gpreg(GpReg::Cpl, compute_effective_cpl(self.selector.rpl().as_u8()));
            } else {
                // TODO: If VM, this should force CPL to 3. However it seems that my current implementation left the CPL untouched, which we will also continue to do until we run into something that breaks.
            }
        }

        if self.seg == GpReg::Ss || self.seg == GpReg::Cs {
            em.update_segment_sizes();
        }
    }
}

impl<'mem, 'tag> Emulator<'mem, 'tag> {
    fn update_segment_sizes(&mut self) {
        self.segment_sizes = match (self.read_cs_size(), self.read_ss_size()) {
            (Db::Protected16, Db::Protected16) => SegmentSizes::Cs16Ss16,
            (Db::Protected16, Db::Protected32) => SegmentSizes::Cs16Ss32,
            (Db::Protected32, Db::Protected16) => SegmentSizes::Cs32Ss16,
            (Db::Protected32, Db::Protected32) => SegmentSizes::Cs32Ss32,
        };
    }

    #[allow(unused)]
    fn execute_instr_interpreter<'e>(
        &mut self, skip_print: bool, instr: Instruction, icache_entry: &EncodingInfo, encodings: &impl EncodingLookup,
    ) -> bool {
        // We make a backup of the current CPU state, then modify `self.cpu`.
        // This allows us to skip copying the new state into `self.cpu` in the happy path.
        // We now only need to restore the backup if the instruction triggers an exception.
        let cpu_backup = self.cpu.clone();
        let encoding = encodings.get(icache_entry.encoding_index);
        self.ctx.k += 1;

        self.ctx.result = (try {
            let mut s = EfficientSystemState::<Intel386> {
                cpu: &mut self.cpu,
                instr,
                mem: ArrayVec::new(),
                part_values: icache_entry.part_values,
                part_packing: encoding.semantics.part_packing,
                parts: encoding.parts,
            };

            // TODO: Do not use instance, but execute from encoding with parts directly.
            let memory_areas = {
                let mut v = ArrayVec::<_, 64>::new();
                v.extend(encoding.semantics.extract_memory_areas(self.ctx.protected_mode, &s));
                v
            };

            for (area, access) in memory_areas.iter().zip(encoding.semantics.addresses.iter()) {
                for &input in access.inputs.iter() {
                    let input = s.resolve_loc(input);
                    if let UnsizedParLoc::Reg(seg_base) = input.loc {
                        let (seg_selector, seg_ar, seg_limit) = match seg_base {
                            Reg::Gp(GpReg::DsBase) => (GpReg::Ds, GpReg::DsAr, GpReg::DsLimit),
                            Reg::Gp(GpReg::EsBase) => (GpReg::Es, GpReg::EsAr, GpReg::EsLimit),
                            Reg::Gp(GpReg::SsBase) => (GpReg::Ss, GpReg::SsAr, GpReg::SsLimit),
                            Reg::Gp(GpReg::FsBase) => (GpReg::Fs, GpReg::FsAr, GpReg::FsLimit),
                            Reg::Gp(GpReg::GsBase) => (GpReg::Gs, GpReg::GsAr, GpReg::GsLimit),
                            _ => continue,
                        };

                        let selector = SegmentSelector::from(s.cpu.gpreg(seg_selector) as u16);
                        if selector.is_null() && (self.ctx.protected_mode && !s.cpu.flag(Intel386Flag::Vm))  {
                            warn!("tried to use NULL segment {seg_selector:?} in {:X}:\n{}\n{}", instr, icache_entry.display_instance(encodings), s.cpu);
                            Err(Exception::GeneralProtectionFault(0))?;
                        }

                        // Check segment limits
                        // TODO: Move these checks to the accesses in `MiniSemRef::execute`, so that string functions don't need workarounds
                        let ar = CachedDescriptorAccessRights::from(s.cpu.gpreg(seg_ar));
                        let addr = area.start_addr().as_u64() as u32;

                        let effective_offset = addr.wrapping_sub(ar.effective_start());

                        let limit = s.cpu.gpreg(seg_limit) as u32;
                        let in_range = effective_offset <= limit;

                        if !in_range {
                            let base = s.cpu.reg(seg_base).unwrap_num() as u32;
                            warn!("Segment {seg_selector:?} used with offset 0x{effective_offset:X}, which is out-of-range [0x{base:X}..0x{limit:X}]. In {:X}:\n{}\n{}", instr, icache_entry.display_instance(encodings), s.cpu);
                            Err(Exception::GeneralProtectionFault(0))?;
                        }
                    }
                }
            }

            s.mem.extend(repeat_n(0, encoding.semantics.addresses.len()));

            let exec_result = encoding.semantics.execute(&mut s, &mut self.ctx, (&memory_areas), !skip_print)
                .inspect_err(|e| info!(target: extend_path_with!("int"), "Interpreter exception {e:X?} (k={}) after executing {:X}:\n{}\n{s:?}", DisplayK(self.ctx.k), instr, icache_entry.display_instance(encodings)))?;

            exec_result
        }).into();

        // If an interrupt occurred, roll back the CPU state
        if self.ctx.result.is_exception() {
            self.cpu = cpu_backup;
        }

        todo!("self.ctx.result == Ok(ExecResult::Ok)")
    }

    // TODO: The ENTER instruction should not need a handler
    fn execute_handler(&mut self, skip_print: bool, id: HandlerId, args: &[u32]) -> Result<(), Interrupt> {
        if !skip_print {
            debug!(
                "Invoking handler (pc: 0x{:X}:0x{:X}): {id:?} with args {args:X?}",
                self.cpu.gpreg(GpReg::CsBase),
                self.cpu.gpreg(GpReg::Ip)
            );
        }

        match id {
            HANDLER_INT => {
                let n = args[0];
                let enter_interrupt = true;
                if self.ctx.protected_mode && self.cpu.flag(Intel386Flag::Vm) {
                    let cr4 = Cr4::from(self.cpu.gpreg(GpReg::Cr4) as u32);
                    let check_iopl = if cr4.virtual_8086_mode_extensions() {
                        if self.ctx.software_interrupt_is_redirected(&self.cpu, n as u8) {
                            todo!("Redirect interrupt");
                            // enter_interrupt = false;
                            // false
                        } else {
                            true
                        }
                    } else {
                        true
                    };

                    if check_iopl && self.cpu.gpreg(GpReg::Iopl) < 3 {
                        return Err(Exception::GeneralProtectionFault(((n as u16) << 3) | 2).into())
                    }
                }

                if enter_interrupt {
                    return Err(Interrupt::SoftwareInterrupt {
                        vector: n.try_into().unwrap(),
                        pc_increment: u8::try_from(args[1]).unwrap(),
                    })
                }
            },
            HANDLER_IRET => {
                self.iret(match args[0] {
                    2 => Db::Protected16,
                    4 => Db::Protected32,
                    _ => unreachable!(),
                })?;
            },
            HANDLER_HALT => {
                self.is_halted = true;
            },
            HANDLER_WRITE_CR => {
                let n = args[0];
                let new_val = args[1];
                match n {
                    0 => self.update_cr0(Cr0::from(new_val)),
                    2 => self.cpu.set_gpreg(GpReg::Cr2, new_val as u64),
                    3 => {
                        self.update_cr3(new_val);
                        self.ctx.mmio_ctx.icache.notify_all_page_mappings_updated(self.ctx.memory);
                    },
                    4 => self.update_cr4(Cr4::from(new_val)),
                    _ => todo!("write Cr{n} = {new_val:X}"),
                }
            },
            // We don't need to do anything here, we just want to know when this happens so we trigger interrupts if needed.
            // TODO: Remove this completely
            HANDLER_IF_UPDATED => (),
            // We don't need to do anything here, we just need the handler to be invoked to ensure the icache will lookup the correct encoding.
            // TODO: Is there a way to remove this?
            HANDLER_SS_UPDATED => {
                self.update_segment_sizes();
            },
            // TODO: Is there a way to move this into the semantics and remove this? -- this is executed very often, and because it's a handler it breaks chains etc.
            HANDLER_CS_UPDATED => {
                if self.cpu.flag(Intel386Flag::Vm) {
                    self.op_size = Db::Protected16;
                    self.cpu.set_gpreg(GpReg::Cpl, compute_effective_cpl(3));
                } else if self.ctx.protected_mode {
                    let ar = CachedDescriptorAccessRights::from(self.cpu.gpreg(GpReg::CsAr));
                    let selector = SegmentSelector::from(self.cpu.gpreg(GpReg::Cs) as u16);

                    self.op_size = ar.flags().size();
                    self.cpu.set_gpreg(GpReg::Cpl, compute_effective_cpl(selector.rpl().as_u8()));
                } else {
                    self.op_size = Db::Protected16;
                    self.cpu.set_gpreg(GpReg::Cpl, compute_effective_cpl(0));
                }

                self.update_segment_sizes();
            },
            HANDLER_CPUID => {
                let eax = args[0];
                let ecx = args[1];

                info!(target: concat!(module_path!(), "::cpuid"), "Reading CPUID eax=0x{eax:X}, ecx=0x{ecx:X}");

                // TODO: Proper CPUID implementation
                let (a, b, c, d) = match (eax, ecx) {
                    (0x0, _) => (Some(0x1), Some(0x756E6547), Some(0x6C65746E), Some(0x49656E69)),
                    (0x1 | 0x8000_0000, _) => {
                        // 0x000201bd
                        let cpu_features = EdxFeatures::new(
                            // 0xbd
                            true, false, true, true, true, true, true, true, // 0x01
                            true, false, // TODO: self.hw().apic_is_enabled(),
                            false, false, false, // TODO: Set this to true so we have less work to do on CR3 reloads
                            true, true, // 0x02
                            false, true, false, false, false, false, false, // 0x00
                            false, false, false, false, false, false, false,
                        );

                        (Some(0x634), Some(0), Some(0), Some(cpu_features.as_u32() as u64))
                    },
                    // (0x80000000, _) => (Some(0x80000008), Some(0), Some(0), Some(0)),
                    (0x80000008, _) => (Some(0x3028), Some(0x200), Some(0), Some(0)),
                    (0xC27C6DF0, 0x2FF) => (Some(0x40), Some(0x40), Some(0x3), Some(0x20)),
                    (eax, ecx) => {
                        error!("TODO: CPUID 0x{eax:X}:0x{ecx:X}");
                        (None, None, None, None)
                    },
                };

                if let Some(a) = a {
                    self.cpu.set_gpreg(GpReg::Ax, a);
                }

                if let Some(b) = b {
                    self.cpu.set_gpreg(GpReg::Bx, b);
                }

                if let Some(c) = c {
                    self.cpu.set_gpreg(GpReg::Cx, c);
                }

                if let Some(d) = d {
                    self.cpu.set_gpreg(GpReg::Dx, d);
                }
            },
            HANDLER_RDMSR => {
                let val = self.ctx.mmio_ctx.hw.read_msr(&self.cpu, args[0], self.ctx.k);
                let (d, a) = ((val >> 32) as u32, val as u32);

                self.cpu.set_gpreg(GpReg::Dx, d as u64);
                self.cpu.set_gpreg(GpReg::Ax, a as u64);
            },
            HANDLER_WRMSR => {
                let msr = self.cpu.gpreg(GpReg::Cx);
                let val = (self.cpu.gpreg(GpReg::Dx) << 32) | (self.cpu.gpreg(GpReg::Ax) as u32 as u64);

                self.ctx.mmio_ctx.hw.write_msr(&mut self.cpu, msr as u32, val);
            },
            HANDLER_INVALIDATE_PAGE => {
                self.ctx.memory.invalidate_page(args[0]);
                self.ctx
                    .mmio_ctx
                    .icache
                    .notify_page_mapping_updated(LinPageIndex::from(LinAddr::new(args[0])), self.ctx.memory);
            },
        }

        Ok(())
    }

    pub fn iret(&mut self, op_size: Db) -> Result<(), Interrupt> {
        let k = self.ctx.k;

        self.op_size = op_size;
        let vm = self.cpu.flag(Intel386Flag::Vm);
        let unpack_upper = self.ctx.protected_mode && matches!(op_size, Db::Protected32) && !vm;
        let ss_size = self.read_ss_size();
        let mut stack = self.ctx.begin_stack_transaction(ss_size, self.op_size, &self.cpu);
        let ip = stack.pop(self.ctx.memory, &mut self.ctx.mmio_ctx);
        let cs = stack.pop(self.ctx.memory, &mut self.ctx.mmio_ctx) as u16;
        let flags = stack.pop(self.ctx.memory, &mut self.ctx.mmio_ctx);
        let cs_selector = SegmentSelector::from(cs);
        let mut sp = None;
        let mut cpl = None;

        unpack_flags(&mut self.cpu, flags, unpack_upper, vm);

        info!(target: extend_path_with!("int"), "IRET from {:X}:0x{:X} to 0x{cs:X}:0x{ip:X} (operand size: {op_size:?}, selector: {cs_selector:X?}, vm={vm}, next vm={}, k={})", self.cpu.gpreg(GpReg::CsBase), self.cpu.gpreg(GpReg::Ip), self.cpu.flag(Intel386Flag::Vm), DisplayK(self.ctx.k));

        let mut segments_to_commit = ArrayVec::<_, 8>::new();
        if self.ctx.protected_mode {
            if !vm && self.cpu.flag(Intel386Flag::Vm) {
                let esp = stack.pop(self.ctx.memory, &mut self.ctx.mmio_ctx) as u16;
                let ss = stack.pop(self.ctx.memory, &mut self.ctx.mmio_ctx) as u16;
                let es = stack.pop(self.ctx.memory, &mut self.ctx.mmio_ctx) as u16;
                let ds = stack.pop(self.ctx.memory, &mut self.ctx.mmio_ctx) as u16;
                let fs = stack.pop(self.ctx.memory, &mut self.ctx.mmio_ctx) as u16;
                let gs = stack.pop(self.ctx.memory, &mut self.ctx.mmio_ctx) as u16;

                // TODO: Don't modify until we're sure we're not going to fault
                sp = Some(esp as u64);
                cpl = Some(3);

                segments_to_commit.extend([
                    self.prepare_segment(GpReg::Es, es)?,
                    self.prepare_segment(GpReg::Ds, ds)?,
                    self.prepare_segment(GpReg::Fs, fs)?,
                    self.prepare_segment(GpReg::Gs, gs)?,
                    self.prepare_segment(GpReg::Ss, ss)?,
                ]);

                info!(target: extend_path_with!("int"), "Entering Virtual 8086 Mode via IRET (k={})", DisplayK(k));
            } else if !self.cpu.flag(Intel386Flag::Vm) {
                if cs_selector.is_null() {
                    debug!(target: extend_path_with!("int"), "CS selector is null");
                    return Err(Exception::GeneralProtectionFault(0).into())
                }

                let cpl = self.cpu.gpreg(GpReg::Cpl) as u8;
                if cs_selector.rpl().as_u8() < cpl {
                    debug!(target: extend_path_with!("int"), "rpl(CS) < cpl");
                    return Err(Exception::GeneralProtectionFault(cs & 0xfffc).into())
                }

                // TODO: Move this into a generic "check if CS descriptor is valid function"
                let cs_descriptor = self.load_descriptor(cs_selector)?; // TODO: Do this only once
                let DescriptorInfo::CodeOrData(cs_info) = cs_descriptor.access_byte().data() else {
                    debug!(target: extend_path_with!("int"), "CS access byte is not CodeOrData: {:?}", cs_descriptor.access_byte());
                    return Err(Exception::GeneralProtectionFault(cs & 0xfffc).into())
                };

                if !cs_info.executable() {
                    debug!(target: extend_path_with!("int"), "CS 0x{cs:X} is not executable");
                    return Err(Exception::GeneralProtectionFault(cs & 0xfffc).into())
                }

                if cs_info.direction_or_conforming() {
                    if cs_descriptor.access_byte().dpl() > cs_selector.rpl() {
                        debug!(target: extend_path_with!("int"), "CS dpl > rpl is not executable");
                        return Err(Exception::GeneralProtectionFault(cs & 0xfffc).into())
                    }
                } else {
                    if cs_descriptor.access_byte().dpl() != cs_selector.rpl() {
                        debug!(target: extend_path_with!("int"), "CS dpl != rpl is not executable");
                        return Err(Exception::GeneralProtectionFault(cs & 0xfffc).into())
                    }

                    // TODO: In some other cases (not iret) we also need to check this: requested RPL > requested CPL -> then #GP.
                }

                if cs_selector.rpl().as_u8() > cpl {
                    // Return to outer privilege level
                    let esp = stack.pop(self.ctx.memory, &mut self.ctx.mmio_ctx);
                    let ss = stack.pop(self.ctx.memory, &mut self.ctx.mmio_ctx) as u16;

                    let ss_selector = SegmentSelector::from(ss);
                    if ss_selector.is_null() {
                        debug!(target: extend_path_with!("int"), "SS selector is null");
                        return Err(Exception::GeneralProtectionFault(0).into())
                    }

                    let ss_descriptor = self.load_descriptor(ss_selector)?; // TODO: Do this only once
                    let sp_mask = match ss_descriptor.flags().size() {
                        Db::Protected16 => 0xffff,
                        Db::Protected32 => 0xffff_ffff,
                    };

                    if ss_selector.rpl() != cs_selector.rpl() {
                        debug!(target: extend_path_with!("int"), "rpl(CS) != rpl(SS)");
                        return Err(Exception::GeneralProtectionFault(ss & 0xfffc).into())
                    }

                    // TODO: Should base ESP mask on SS descriptor, not CS descriptor
                    sp = Some((esp as u64) & sp_mask | (self.cpu.gpreg(GpReg::Sp) & !sp_mask));
                    segments_to_commit.push(self.prepare_segment(GpReg::Ss, ss)?);

                    for (_seg, _seg_base) in [
                        (GpReg::Es, GpReg::EsBase),
                        (GpReg::Fs, GpReg::FsBase),
                        (GpReg::Gs, GpReg::GsBase),
                        (GpReg::Ds, GpReg::DsBase),
                    ] {
                        // TODO: If descriptor DPL < CPL and descriptor is data or code, then set to NULL selector.
                    }
                }
            }
        }

        self.prepare_segment(GpReg::Cs, cs)?.commit(self);
        for seg in segments_to_commit {
            seg.commit(self);
        }

        stack.commit(&mut self.cpu);
        self.cpu.set_gpreg(GpReg::Ip, ip as u64);

        if let Some(cpl) = cpl {
            self.cpu.set_gpreg(GpReg::Cpl, compute_effective_cpl(cpl));
        }

        if let Some(sp) = sp {
            self.cpu.set_gpreg(GpReg::Sp, sp);
        }

        Ok(())
    }

    fn update_cr4(&mut self, cr4: Cr4) {
        warn!("CR4 = 0x{:X} = {cr4:X?}", cr4.as_u32());
        self.cpu.set_gpreg(GpReg::Cr4, cr4.as_u32() as u64);

        if cr4.page_global_enabled() {
            error!("TODO: Implement PGE");
        }

        self.ctx.memory.set_page_size_extension(cr4.page_size_extension());
        self.ctx
            .memory
            .enable_physical_address_extension(cr4.physical_address_extension());
    }

    fn update_cr3(&mut self, new_val: u32) {
        info!("Writing CR3");
        if self.cpu.gpreg(GpReg::Cr3) == new_val as u64 {
            self.num_cr3_reloads += 1;
        } else {
            self.num_cr3_changes += 1;
        }

        self.ctx.memory.set_page_directory_base(new_val);
        self.cpu.set_gpreg(GpReg::Cr3, new_val as u64);
    }

    fn update_cr0(&mut self, cr0: Cr0) {
        info!(
            "CR0 = 0x{:X} = {cr0:?} @ IP = 0x{:X}",
            cr0.as_u32(),
            self.cpu.gpreg(GpReg::Ip)
        );
        self.paging_enabled = cr0.paging();
        self.ctx.protected_mode = cr0.protected_mode();
        self.ctx.memory.set_system_write_protect(cr0.write_protect());
        self.ctx.memory.enable_paging(self.paging_enabled);
        self.cpu.set_gpreg(GpReg::Cr0, u32::from(cr0) as u64);
    }

    fn check_trace(
        &mut self, id: Option<CacheEntryId<'tag>>, instr: Instruction, entry: &EncodingInfo,
        addrs_written: &mut ArrayVec<u32, 64>, expected_pagefault: bool, encodings: &impl EncodingLookup,
    ) -> Result<(), Interrupt> {
        if let Some(trace) = self.ctx.trace.as_mut() {
            if self.cpu.gpreg(GpReg::Ip) == 0x35067004 {
                let info = entry;
                warn!(
                    "CPU executed 0x{:X}:0x{:X} ({} instrs executed)",
                    self.cpu.gpreg(GpReg::CsBase),
                    self.cpu.gpreg(GpReg::Ip),
                    self.ctx.k
                );
                warn!("Executing: {instr:X}\n{}", info.display_instance(encodings));
            }

            if instr == Instruction::new(&[0x9b]) {
                debug!("Checked instruction {instr:X}");
            }

            if self.trace_limit == self.ctx.k {
                warn!("Disengaging after next instruction");
            }

            if self.trace_limit < self.ctx.k {
                self.ctx.trace = None;
                warn!(
                    "Disengaging trace, next expected interrupt: {:X?}",
                    self.next_expected_interrupt
                );
                self.ctx.mmio_ctx.hw.trace_disengaged();

                return Ok(());
            }

            let k = self.ctx.k;
            let external_ints = &[
                self.ctx.mmio_ctx.hw.vector_offsets().0,     // Timer
                self.ctx.mmio_ctx.hw.vector_offsets().0 + 1, // PPI (i.e., PS/2 keyboard)
                self.ctx.mmio_ctx.hw.vector_offsets().0 + 4, // COM1?
                // self.ctx.mmio_ctx.hw.vector_offsets().0 + 5, // LogiBM
                self.ctx.mmio_ctx.hw.vector_offsets().0 + 6, // FDC
                self.ctx.mmio_ctx.hw.vector_offsets().0 + 7, // LPT1?
                self.ctx.mmio_ctx.hw.vector_offsets().1,     // TODO: CMOS alarm
                self.ctx.mmio_ctx.hw.vector_offsets().1 + 1, // NE2K (w98)
                self.ctx.mmio_ctx.hw.vector_offsets().1 + 3, // ES1370
                self.ctx.mmio_ctx.hw.vector_offsets().1 + 4, // PPI AUX (i.e., PS/2 mouse)
                self.ctx.mmio_ctx.hw.vector_offsets().1 + 6, // Primary IDE
                self.ctx.mmio_ctx.hw.vector_offsets().1 + 7, // Secondary IDE
                // TODO: Replace this with INTs from redirection entries
                self.ctx.mmio_ctx.hw.redirection_entry_vector(0),
                self.ctx.mmio_ctx.hw.redirection_entry_vector(1),
                self.ctx.mmio_ctx.hw.redirection_entry_vector(2),
                self.ctx.mmio_ctx.hw.redirection_entry_vector(3),
                self.ctx.mmio_ctx.hw.redirection_entry_vector(4),
                self.ctx.mmio_ctx.hw.redirection_entry_vector(5),
                self.ctx.mmio_ctx.hw.redirection_entry_vector(6),
                self.ctx.mmio_ctx.hw.redirection_entry_vector(7),
                self.ctx.mmio_ctx.hw.redirection_entry_vector(8),
                self.ctx.mmio_ctx.hw.redirection_entry_vector(9),
                self.ctx.mmio_ctx.hw.redirection_entry_vector(10),
                self.ctx.mmio_ctx.hw.redirection_entry_vector(11),
                self.ctx.mmio_ctx.hw.redirection_entry_vector(12),
                self.ctx.mmio_ctx.hw.redirection_entry_vector(13),
                self.ctx.mmio_ctx.hw.redirection_entry_vector(14),
                self.ctx.mmio_ctx.hw.redirection_entry_vector(15),
                self.ctx.mmio_ctx.hw.redirection_entry_vector(16),
                self.ctx.mmio_ctx.hw.redirection_entry_vector(17),
                self.ctx.mmio_ctx.hw.redirection_entry_vector(18),
                self.ctx.mmio_ctx.hw.redirection_entry_vector(19),
                self.ctx.mmio_ctx.hw.redirection_entry_vector(20),
                self.ctx.mmio_ctx.hw.redirection_entry_vector(21),
                self.ctx.mmio_ctx.hw.redirection_entry_vector(22),
                self.ctx.mmio_ctx.hw.redirection_entry_vector(23),
                0x3d,
                0x62,
                0x41,
                0xD1,
                0x93,
                0xA3,
                0xB3,
                0xC1, // ??
            ];

            let mut expected_addr_writes = Vec::new();
            let mut checked_instr = false;

            let gb_read = trace.gb_read();
            while let Some(next) = trace.next(checked_instr) {
                match next {
                    TraceEntry::Instr(t) => {
                        assert!(!checked_instr);

                        if expected_pagefault {
                            let addr = t.cr2;
                            error!("Expected a page fault for 0x{addr:X} at IP=0x{:X}", { t.eip });

                            let w = self.ctx.memory.page_walk(addr, false);
                            error!("Page info for address 0x{addr:X} = {w:#X?}");
                        }

                        let expected_instr = Instruction::new(&t.bytes[..t.len as usize]);
                        if expected_instr != instr {
                            if expected_instr == Instruction::new(&[0x9B]) {
                                warn!(
                                    "Skipping over instruction 9B, because the Bochs trace is probably missing it due to the generated interrupt"
                                );
                                continue;
                            }

                            if let &[0xCD, v] = expected_instr.bytes()
                                && self.trace_triggered_interrupt == Some(v)
                            {
                                warn!(
                                    "Skipping over instruction {expected_instr:X}, because we just triggered INT 0x{v:02X} from the trace"
                                );
                                continue
                            }

                            let t = *t;
                            let next = (0..5).map(|_| trace.next(false).map(|x| x.into_owned())).collect::<Vec<_>>();

                            let pc = self.path.last().0;
                            let phys_addr = if self.paging_enabled {
                                match self.ctx.memory.page_walk(pc as u32, false) {
                                    PageWalkResult::Unmapped(_) => 0,
                                    PageWalkResult::PhysAddr {
                                        addr, ..
                                    } => addr,
                                }
                            } else {
                                pc
                            };

                            if phys_addr != 0 {
                                let mut bytes = [0; 16];
                                self.ctx
                                    .memory
                                    .read_physical_slice(phys_addr as u32, &mut bytes, &mut self.ctx.mmio_ctx)
                                    .unwrap();
                                println!("Resolved physical address: 0x{phys_addr:X} = {bytes:02X?}");

                                if &bytes[..instr.byte_len()] != instr.bytes() {
                                    panic!(
                                        "Instruction cache is out of date: memory contains {:02X?}, but icache returned {id:?} = {instr:X} at lin=0x{pc:X}, phys=0x{phys_addr:X}\nInstance: {}\n\nPath: {:X?}",
                                        &bytes[..instr.byte_len()],
                                        entry.display_instance(encodings),
                                        self.path
                                    );
                                }

                                let mut bytes = [0; 16];
                                self.ctx
                                    .memory
                                    .read_physical_slice(phys_addr as u32 & 0xfffff, &mut bytes, &mut self.ctx.mmio_ctx)
                                    .unwrap();
                                println!("A20 disabled: 0x{:X} = {bytes:02X?}", phys_addr & 0xfffff);
                            }

                            let is_dirty = self.ctx.memory.phys_frame_is_dirty(phys_addr);
                            let encoding_index = entry.encoding_index;

                            if t.eip != self.cpu.gpreg(GpReg::Ip) as u32 {
                                let expected_ip = t.eip;
                                let actual_ip = self.cpu.gpreg(GpReg::Ip);
                                panic!(
                                    "deviated from execution @ pc=0x{pc:X} (phys: 0x{phys_addr:X}, dirty={is_dirty}, encoding_index={encoding_index}, next_expected_interrupt={:X?}) (k={}, trace={gb_read:.3}GiB, protected_mode={}): expected {expected_instr:X} @ 0x{expected_ip:X} found {instr:X} @ 0x{actual_ip:X}:\n{}\n\n{t:#X?}\n\nInstance: {}\n\nPath: {:X?}\n\nNext trace entries: {next:#X?}",
                                    self.next_expected_interrupt,
                                    DisplayK(k),
                                    self.ctx.protected_mode,
                                    self.cpu,
                                    entry.display_instance(encodings),
                                    self.path
                                )
                            }

                            panic!(
                                "executed a different instruction @ pc=0x{pc:X} (phys: 0x{phys_addr:X}, dirty={is_dirty}, encoding_index={encoding_index}, next_expected_interrupt={:X?}) (k={}, trace={gb_read:.3}GiB, protected_mode={}): expected {expected_instr:X} found {instr:X}:\n{}\n\n{t:#X?}\n\nInstance: {}\n\nPath: {:X?}\n\nNext trace entries: {next:#X?}",
                                self.next_expected_interrupt,
                                DisplayK(k),
                                self.ctx.protected_mode,
                                self.cpu,
                                entry.display_instance(encodings),
                                self.path
                            )
                        }

                        checked_instr = true;
                        macro_rules! compare_state {
                            ({ $($name:tt)* } $gpreg:ident) => {{
                                compare_state!({ $($name)* } { self.cpu.gpreg(GpReg::$gpreg) as u32 } |v| {
                                    self.cpu.set_gpreg(GpReg::$gpreg, v as u64);
                                })
                            }};
                            ({ $($name:tt)* } if { $show_if:expr } $gpreg:ident) => {{
                                compare_state!({ $($name)* } { self.cpu.gpreg(GpReg::$gpreg) as u32 } if { $show_if } |v| {
                                    self.cpu.set_gpreg(GpReg::$gpreg, v as u64);
                                })
                            }};
                            ({ $($name:tt)* } { $e:expr } precision=$p:expr; if { $show_if:expr } $correct:expr) => {{
                                let v = $($name)*;

                                if u128::from($e) != u128::from(v) {
                                    if self.ctx.k >= self.skip_trace_differences_before && $show_if && u128::from($e) >> $p != u128::from(v) >> $p {
                                        error!("{} not equal after executing {instr:X} (k={}, trace={gb_read:.3}GiB):\n\nEmulator state: {}\nExpected state: {t:#X?}\n\nInstruction executed: {}\n\npath taken: {:X?}", stringify!($($name)*), DisplayK(k), self.cpu, entry.display_instance(encodings), self.path);
                                    }

                                    $correct(v);
                                };
                            }};
                            ({ $($name:tt)* } { $e:expr } if { $show_if:expr } $correct:expr) => {{
                                let v = $($name)*;

                                if u128::from($e) != u128::from(v) {
                                    if self.ctx.k >= self.skip_trace_differences_before && $show_if {
                                        error!("{} not equal after executing {instr:X}: found: 0x{:X}, expected: 0x{:X} (k={}, trace={gb_read:.3}GiB):\n\nEmulator state: {}\nExpected state: {t:#X?}\n\nInstruction executed: {}\n\npath taken: {:X?}", stringify!($($name)*), u128::from($e), u128::from(v), DisplayK(k), self.cpu, entry.display_instance(encodings), self.path);
                                    } else {
                                        warn!(target: extend_path_with!("trace::skip"), "Skipped difference in {} after executing {instr:X}, found: 0x{:X}, expected: 0x{:X}", stringify!($($name)*), u128::from($e), u128::from(v))
                                    }

                                    $correct(v);
                                };
                            }};
                            ({ $($name:tt)* } { $e:expr } $correct:expr) => {{
                                compare_state!( { $($name)* } { $e } if { true } $correct );
                            }}
                        }

                        let instr_is_rdtsc = instr == Instruction::new(&[0x0F, 0x31]);

                        if instr_is_rdtsc {
                            self.cpu.set_gpreg(GpReg::Ax, t.eax as u64);
                            self.cpu.set_gpreg(GpReg::Dx, t.edx as u64);
                        } else {
                            // Ignore differences in FNSTW_ax -- there are always some, we just want to know if they eventually cause a meaningful difference.
                            if instr != Instruction::new(&[0xDF, 0xE0]) && instr != Instruction::new(&[0x9B]) {
                                compare_state!({ t.eax } if { !instr_is_rdtsc && instr != Instruction::new(&[ 0x9E ]) } Ax);
                            }
                            compare_state!({ t.edx } if { !instr_is_rdtsc } Dx);
                        }

                        compare_state!({ t.ebx } Bx);
                        compare_state!({ t.ecx } Cx);
                        compare_state!({ t.esi } Si);
                        compare_state!({ t.edi } Di);
                        compare_state!({ t.esp } Sp);
                        compare_state!({ t.ebp } Bp);
                        compare_state!({ t.eip } Ip);

                        compare_state!({ t.cs.cached_base } CsBase);
                        compare_state!({ t.ds.cached_base } DsBase);
                        compare_state!({ t.es.cached_base } EsBase);
                        compare_state!({ t.ss.cached_base } SsBase);
                        compare_state!({ t.fs.cached_base } FsBase);
                        compare_state!({ t.gs.cached_base } GsBase);

                        compare_state!({ t.cs.value } Cs);
                        compare_state!({ t.ds.value } Ds);
                        compare_state!({ t.es.value } Es);
                        compare_state!({ t.ss.value } Ss);
                        compare_state!({ t.fs.value } Fs);
                        compare_state!({ t.gs.value } Gs);

                        // TODO: Can we somehow compute the effective limit?
                        // compare_state!({ t.cs.limit } CsLimit);
                        // compare_state!({ t.ds.limit } DsLimit);
                        // compare_state!({ t.es.limit } EsLimit);
                        // compare_state!({ t.ss.limit } SsLimit);
                        // compare_state!({ t.fs.limit } FsLimit);
                        // compare_state!({ t.gs.limit } GsLimit);

                        compare_state!({ t.gdtr.base } GdtBase);
                        compare_state!({ t.gdtr.limit } GdtLimit);
                        compare_state!({ t.ldtr.cached_base } LdtBase);
                        compare_state!({ t.idtr.base } IdtBase);
                        compare_state!({ t.idtr.limit } IdtLimit);
                        compare_state!({ t.tr.cached_base } TrBase);
                        compare_state!({ t.tr.value } Tr);

                        compare_state!({ t.eflags & (1 << 17) != 0 } { self.cpu.flag(Intel386Flag::Vm) } |v| {
                            self.cpu.set_flag(Intel386Flag::Vm, v);
                        });
                        compare_state!({ t.eflags & (1 << 9) != 0 } { self.cpu.flag(Intel386Flag::If) } |v| {
                            self.cpu.set_flag(Intel386Flag::If, v);
                        });
                        // TODO: These don't seem to be up-to-date
                        // compare_state!({ t.eflags & (1 << 2) != 0 } { self.cpu.flag(Intel386Flag::Pf) } |v| {
                        //     self.cpu.set_flag(Intel386Flag::Pf, v);
                        // });
                        // compare_state!({ t.eflags & (1 << 7) != 0 } { self.cpu.flag(Intel386Flag::Sf) } |v| {
                        //     self.cpu.set_flag(Intel386Flag::Sf, v);
                        // });
                        // compare_state!({ t.eflags & (1 << 6) != 0 } { self.cpu.flag(Intel386Flag::Zf) } |v| {
                        //     self.cpu.set_flag(Intel386Flag::Zf, v);
                        // });
                        // compare_state!({ t.eflags & (1 << 11) != 0 } { self.cpu.flag(Intel386Flag::Of) } |v| {
                        //     self.cpu.set_flag(Intel386Flag::Of, v);
                        // });
                        // compare_state!({ t.eflags & (1 << 0) != 0 } { self.cpu.flag(Intel386Flag::Cf) } |v| {
                        //     self.cpu.set_flag(Intel386Flag::Cf, v);
                        // });
                        // TODO: RF
                        // compare_state!({ t.eflags & (1 << 16) != 0 } { self.cpu.flag(Intel386Flag::Rf) } |v| {
                        //     self.cpu.set_flag(Intel386Flag::Rf, v);
                        // });

                        compare_state!({ (t.eflags >> 12) as u64 & 3 } { self.cpu.gpreg(GpReg::Iopl) } |v| {
                            self.cpu.set_gpreg(GpReg::Iopl, v);
                        });

                        compare_state!({ t.cr3 as u64 } { self.cpu.gpreg(GpReg::Cr3) } |_| {
                            panic!("CR3 not equal after executing {instr:X}: expected {:#X?}\n\n{}", t, self.cpu);
                        });

                        let show_fp_differences = true;
                        let precision = if [
                            Instruction::new(&[0xD9, 0xF0]), // F2XM1
                            Instruction::new(&[0xD9, 0xF1]), // FYL2X
                            Instruction::new(&[0xD9, 0xF2]), // FPTAN
                            Instruction::new(&[0xD9, 0xF3]), // FPATAN
                            Instruction::new(&[0xD9, 0xFE]), // FSIN
                            Instruction::new(&[0xD9, 0xFF]), // FCOS
                        ]
                        .contains(&instr)
                        {
                            23
                        } else {
                            0
                        };
                        compare_state!({ { t.mm[0] }.as_u128() } { self.cpu.x87.mm[0] } precision=precision; if { show_fp_differences } |v| { self.cpu.x87.mm[0] = v; });
                        compare_state!({ { t.mm[1] }.as_u128() } { self.cpu.x87.mm[1] } precision=precision; if { show_fp_differences } |v| { self.cpu.x87.mm[1] = v; });
                        compare_state!({ { t.mm[2] }.as_u128() } { self.cpu.x87.mm[2] } precision=precision; if { show_fp_differences } |v| { self.cpu.x87.mm[2] = v; });
                        compare_state!({ { t.mm[3] }.as_u128() } { self.cpu.x87.mm[3] } precision=precision; if { show_fp_differences } |v| { self.cpu.x87.mm[3] = v; });
                        compare_state!({ { t.mm[4] }.as_u128() } { self.cpu.x87.mm[4] } precision=precision; if { show_fp_differences } |v| { self.cpu.x87.mm[4] = v; });
                        compare_state!({ { t.mm[5] }.as_u128() } { self.cpu.x87.mm[5] } precision=precision; if { show_fp_differences } |v| { self.cpu.x87.mm[5] = v; });
                        compare_state!({ { t.mm[6] }.as_u128() } { self.cpu.x87.mm[6] } precision=precision; if { show_fp_differences } |v| { self.cpu.x87.mm[6] = v; });
                        compare_state!({ { t.mm[7] }.as_u128() } { self.cpu.x87.mm[7] } precision=precision; if { show_fp_differences } |v| { self.cpu.x87.mm[7] = v; });
                        compare_state!({ (t.fsw >> 11) & 7 } { self.cpu.x87.top } if { show_fp_differences } |v| { self.cpu.x87.top = v as u8; });

                        compare_state!({ t.fsw & 1 } { self.cpu.x87.exception_flags as u8 } if { show_fp_differences } |v| {
                            self.cpu.x87.exception_flags &= !0xff;
                            self.cpu.x87.exception_flags |= v as u64 ;
                        });
                        // compare_state!({ (t.fsw >> 1) & 1 } { (self.cpu.x87.exception_flags >> 8) as u8 } if { show_fp_differences } |v| {
                        //     self.cpu.x87.exception_flags &= !(0xff << 8);
                        //     self.cpu.x87.exception_flags |= (v as u64) << 8;
                        // });
                        // compare_state!({ (t.fsw >> 2) & 1 } { (self.cpu.x87.exception_flags >> 16) as u8 } if { show_fp_differences } |v| {
                        //     self.cpu.x87.exception_flags &= !(0xff << 16);
                        //     self.cpu.x87.exception_flags |= (v as u64) << 16;
                        // });
                        // compare_state!({ (t.fsw >> 3) & 1 } { (self.cpu.x87.exception_flags >> 24) as u8 } if { show_fp_differences } |v| {
                        //     self.cpu.x87.exception_flags &= !(0xff << 24);
                        //     self.cpu.x87.exception_flags |= (v as u64) << 24;
                        // });
                        // compare_state!({ (t.fsw >> 4) & 1 } { (self.cpu.x87.exception_flags >> 32) as u8 } if { show_fp_differences } |v| {
                        //     self.cpu.x87.exception_flags &= !(0xff << 32);
                        //     self.cpu.x87.exception_flags |= (v as u64) << 32;
                        // });
                        // compare_state!({ (t.fsw >> 5) & 1 } { (self.cpu.x87.exception_flags >> 40) as u8 } if { show_fp_differences } |v| {
                        //     self.cpu.x87.exception_flags &= !(0xff << 40);
                        //     self.cpu.x87.exception_flags |= (v as u64) << 40;
                        // });
                        // compare_state!({ (t.fsw >> 6) & 1 } { (self.cpu.x87.exception_flags >> 48) as u8 } if { show_fp_differences } |v| {
                        //     self.cpu.x87.exception_flags &= !(0xff << 48);
                        //     self.cpu.x87.exception_flags |= (v as u64) << 48;
                        // });

                        // compare_state!({ (t.fsw >> 8) & 1 } { (self.cpu.x87.condition_codes >> 0) as u8 } if { show_fp_differences } |v| {
                        //     self.cpu.x87.condition_codes &= !(0xff << 0);
                        //     self.cpu.x87.condition_codes |= (v as u32) << 0;
                        // });
                        // // TODO: Use show_fp_differences
                        // compare_state!({ (t.fsw >> 9) & 1 } { (self.cpu.x87.condition_codes >> 8) as u8 } if { false } |v| {
                        //     self.cpu.x87.condition_codes &= !(0xff << 8);
                        //     self.cpu.x87.condition_codes |= (v as u32) << 8;
                        // });
                        // compare_state!({ (t.fsw >> 10) & 1 } { (self.cpu.x87.condition_codes >> 16) as u8 } if { show_fp_differences } |v| {
                        //     self.cpu.x87.condition_codes &= !(0xff << 16);
                        //     self.cpu.x87.condition_codes |= (v as u32) << 16;
                        // });
                        // compare_state!({ (t.fsw >> 14) & 1 } { (self.cpu.x87.condition_codes >> 24) as u8 } if { show_fp_differences } |v| {
                        //     self.cpu.x87.condition_codes &= !(0xff << 24);
                        //     self.cpu.x87.condition_codes |= (v as u32) << 24;
                        // });
                    },
                    TraceEntry::MemAssert(t) => {
                        let addr = t.paddr;
                        let laddr = t.laddr;
                        let expected = &t.data[..t.len as usize];

                        let mut buf = [0; 16];
                        let actual = &mut buf[..t.len as usize];
                        self.ctx
                            .memory
                            .read_physical_slice(addr, actual, &mut self.ctx.mmio_ctx)
                            .unwrap();

                        let mut lbuf = [0; 16];
                        let lactual = &mut lbuf[..t.len as usize];
                        if self.paging_enabled {
                            if let Err(e) = self.ctx.memory.read_slice(laddr, lactual, false, &mut self.ctx.mmio_ctx) {
                                error!("Error trying to read from 0x{laddr:X}: {e:#X?}")
                            }
                        } else {
                            lactual.copy_from_slice(actual);
                        }

                        // TODO: addrs_written is now always empty, so this does nothing
                        addrs_written.retain(|x| *x < laddr || *x >= laddr + t.len as u32);
                        expected_addr_writes.push(laddr..laddr + t.len as u32);

                        if expected != actual
                            && addr & !0xfff != 0xfee00000
                            && addr & !0xfff != 0xfec00000
                            && addr & !0x1ffff != 0x000c0000
                        {
                            if self.ctx.k >= self.skip_trace_differences_before {
                                if actual != lactual {
                                    error!("memory is mapped incorrectly phys=0x{addr:X} is not mapped at lin=0x{laddr:X}");
                                }

                                let is_flags_or_pte_accessed =
                                    expected.len() == 4 && expected[1..] == actual[1..] && expected[0] & 0xdd == actual[0] & 0xdd;

                                let e = encodings.get(entry.encoding_index);
                                let is_fnsave_fop = e.semantics.name == "FNSAVE" && {
                                    let mem4_addr = entry.instance(encodings).addresses[4]
                                        .compute_address_from_cpu_state(&self.cpu, 0)
                                        .as_u64();
                                    mem4_addr == laddr as u64
                                };

                                if is_fnsave_fop {
                                    warn!(
                                        "Ignoring difference in FNSAVE FOP field at phys=0x{addr:X}/lin=0x{laddr:X}: expected {expected:02X?}, found {actual:02X?}"
                                    );
                                } else if is_flags_or_pte_accessed {
                                    warn!(
                                        "Probable error code/PTE flag difference at phys=0x{addr:X}/lin=0x{laddr:X}: expected {expected:02X?}, found {actual:02X?}"
                                    );
                                } else {
                                    error!(
                                        "memory difference (k={}, trace={gb_read:.3}GiB) after executing {instr:X} (op_size={:?}) {}\nAt address phys=0x{addr:X}, lin=0x{laddr:X}: expected {expected:02X?}, found {actual:02X?} (lfound: {lactual:02X?})\n\nInstance: {}\n\nPath: {:X?}",
                                        DisplayK(k),
                                        self.op_size,
                                        self.cpu,
                                        entry.display_instance(encodings),
                                        self.path
                                    );
                                }
                            }

                            self.ctx
                                .memory
                                .write_physical_slice(addr, expected, &mut self.ctx.mmio_ctx)
                                .unwrap();
                        }
                    },
                    TraceEntry::Int(t) => {
                        self.next_expected_interrupt = Some(t.vector);

                        if external_ints.contains(&t.vector) {
                            checked_instr = true;
                        }
                    },
                    TraceEntry::In(t) | TraceEntry::Out(t) => {
                        if checked_instr {
                            unreachable!()
                        } else {
                            panic!(
                                "Unused port I/O {t:X?} (k={}, trace={gb_read:.3}GiB) after executing {instr:X} {}",
                                DisplayK(k),
                                self.cpu
                            )
                        }
                    },
                }
            }

            if !checked_instr {
                self.ctx.trace = None;
                warn!("Disengaging trace -- no instruction was read");
                self.ctx.mmio_ctx.hw.trace_disengaged();

                return Ok(());
            }

            addrs_written.retain(|x| !(0xa0000..0xc0000).contains(x));
            if !addrs_written.is_empty() {
                if self.paging_enabled {
                    addrs_written.retain(|addr| {
                        let phys_addr = if self.paging_enabled {
                            match self.ctx.memory.page_walk(*addr, false) {
                                PageWalkResult::Unmapped(_) => 0,
                                PageWalkResult::PhysAddr {
                                    addr, ..
                                } => addr,
                            }
                        } else {
                            0
                        };

                        // println!("0x{addr:X} -> 0x{phys_addr:X}");
                        !(0xa0000..0xc0000).contains(&phys_addr)
                    });
                }

                if !addrs_written.is_empty() {
                    let next = (0..5).map(|_| trace.next(false).map(|x| x.into_owned())).collect::<Vec<_>>();

                    let gb_read = trace.gb_read();
                    panic!(
                        "Incorrect memory written (k={}, trace={gb_read:.3}GiB, protected_mode={}, size={:?}): {addrs_written:X?} expected {expected_addr_writes:X?} by {instr:X} executed on:\n{}\n\n{}\n\nNext trace entries: {next:#X?}",
                        DisplayK(k),
                        self.ctx.protected_mode,
                        self.op_size,
                        self.cpu,
                        entry.display_instance(encodings)
                    )
                }
            }

            self.trace_triggered_interrupt = None;
            if let Some(vector) = self.next_expected_interrupt {
                if external_ints.contains(&vector) {
                    let gb_read = trace.gb_read();
                    let redirection_entry = self.hw().ioapic().find_redirection_entry_from_vector(vector);
                    info!(
                        "Triggering interrupt {vector:X?} from trace (k={}, trace={gb_read:.3}GiB, current offsets: {:X?}, associated redirection entry: {redirection_entry:X?})",
                        DisplayK(k),
                        self.ctx.mmio_ctx.hw.vector_offsets()
                    );

                    self.trace_triggered_interrupt = Some(vector);

                    let vector = vector as u64;
                    assert!(checked_instr);

                    if encodings.get(entry.encoding_index).semantics.is_rep {
                        // TODO: Check if CX is non-zero
                        self.cpu.set_flag(Intel386Flag::Rf, true);
                    }

                    return Err(Interrupt::HardwareInterrupt(vector.try_into().unwrap()))
                } else {
                    info!(target: extend_path_with!("int"), "Pending next interrupt: {vector:02X}");
                }
            }
        }

        Ok(())
    }

    #[inline(always)]
    fn read_ss_size(&mut self) -> Db {
        CachedDescriptorAccessRights::from(self.cpu.gpreg(GpReg::SsAr)).flags().size()
    }

    #[inline(always)]
    fn read_cs_size(&mut self) -> Db {
        CachedDescriptorAccessRights::from(self.cpu.gpreg(GpReg::CsAr)).flags().size()
    }

    #[inline(always)]
    fn prepare_segment(&mut self, seg: GpReg, selector_val: u16) -> Result<PreparedSegment, Exception> {
        if self.ctx.protected_mode && !self.cpu.flag(Intel386Flag::Vm) {
            let selector = SegmentSelector::from(selector_val);

            debug!(target: extend_path_with!("seg"), "Looking up segment base for {seg:?} = {selector:?} (0x{selector_val:04X}) in protected mode (VM={})", self.cpu.flag(Intel386Flag::Vm));
            let descriptor = self.load_descriptor(selector)?;
            debug!(target: extend_path_with!("seg"), "Descriptor: {descriptor:X?}");

            if selector.segment_index().as_u16() != 0 && !descriptor.access_byte().present() {
                return Err(Exception::SegmentNotPresent(selector.segment_index().as_u16()));
            }

            if let DescriptorInfo::CodeOrData(info) = descriptor.access_byte().data()
                && !info.accessed()
            {
                Self::mark_descriptor_accessed(&self.cpu, self.ctx.memory, &mut self.ctx.mmio_ctx, selector, descriptor)?;
            }

            Ok(PreparedSegment {
                seg,
                selector,
                protected_mode: true,
                descriptor,
            })
        } else {
            Ok(PreparedSegment {
                seg,
                selector: SegmentSelector::from(selector_val),
                protected_mode: false,
                descriptor: Descriptor::from_real_mode_selector(selector_val),
            })
        }
    }

    fn load_descriptor(&mut self, selector: SegmentSelector) -> Result<Descriptor, Exception> {
        let (table_addr, table_limit) = if selector.is_local() {
            (self.cpu.gpreg(GpReg::LdtBase) as u32, self.cpu.gpreg(GpReg::LdtLimit) as u32)
        } else {
            (self.cpu.gpreg(GpReg::GdtBase) as u32, self.cpu.gpreg(GpReg::GdtLimit) as u32)
        };

        let offset = selector.segment_index().as_u32() * 8;
        let descriptor_addr = table_addr + offset;

        debug!(target: extend_path_with!("seg"), "Reading from 0x{descriptor_addr:X} (0x{table_addr:X} + 0x{:X} * 8)", selector.segment_index());

        if offset + 7 > table_limit {
            warn!(target: extend_path_with!("seg"), "Segment selector out of range");
            return Err(Exception::SegmentNotPresent(selector.segment_index().as_u16()));
        }

        let Ok(descriptor_val) = self.ctx.memory.read::<u64>(descriptor_addr, false, &mut self.ctx.mmio_ctx) else {
            panic!("Reading segment descriptor table failed in: {}", self.cpu);
        };

        Ok(Descriptor::from(descriptor_val))
    }

    fn mark_descriptor_accessed(
        cpu: &State, mem: &Mem32, mmio: &mut impl Mmio, selector: SegmentSelector, descriptor: Descriptor,
    ) -> Result<(), Exception> {
        let mut access_byte = descriptor.access_byte();
        if let DescriptorInfo::CodeOrData(mut info) = access_byte.data()
            && !info.accessed()
        {
            info.set_accessed(true);
            access_byte.set_info(DescriptorInfo::CodeOrData(info));
            let (table_addr, table_limit) = if selector.is_local() {
                (cpu.gpreg(GpReg::LdtBase) as u32, cpu.gpreg(GpReg::LdtLimit) as u32)
            } else {
                (cpu.gpreg(GpReg::GdtBase) as u32, cpu.gpreg(GpReg::GdtLimit) as u32)
            };

            let offset = selector.segment_index().as_u32() * 8;
            let descriptor_addr = table_addr + offset;

            debug!(target: extend_path_with!("seg"), "Reading from 0x{descriptor_addr:X} (0x{table_addr:X} + 0x{:X} * 8)", selector.segment_index());

            if offset + 7 > table_limit {
                warn!(target: extend_path_with!("seg"), "Segment selector out of range");
                return Err(Exception::SegmentNotPresent(selector.segment_index().as_u16()));
            }

            let val = u8::from(access_byte);
            let Ok(_) = mem.write::<u8>(descriptor_addr + 5, false, val, mmio) else {
                panic!("Write segment descriptor table failed in: {}", cpu);
            };

            debug!(target: extend_path_with!("seg"), "Marked descriptor at 0x{descriptor_addr:X} as accessed by writing to byte 5: 0x{:02X}", val);
        }

        Ok(())
    }

    pub fn enter_interrupt(&mut self, interrupt: impl Into<Interrupt>) -> Result<(), Exception> {
        self.num_interrupts_entered += 1;
        // TODO: Raise GP if n is too big.
        // TODO: Raise SS if stack is not big enough
        // TODO: We should not be modifying state until we are sure we're not going to throw an exception

        const LOG_TARGET: &str = extend_path_with!("int");
        let do_log = log_enabled!(target: LOG_TARGET, log::Level::Info);
        let interrupt = interrupt.into();
        let is_software_interrupt = matches!(interrupt, Interrupt::SoftwareInterrupt { .. });
        let n = interrupt.vector() as u64;
        let error_code = if self.ctx.protected_mode {
            interrupt.code()
        } else {
            // We don't push error codes in real mode
            None
        };

        // TODO: Also set if last executed instruction was repeated string instruction (except if the instruction has completed the last iteration)
        if let Interrupt::Exception(e) = interrupt
            && e.class() == ExceptionClass::Fault
            && e != Exception::Debug
        {
            self.cpu.set_flag(Intel386Flag::Rf, true);
        }

        if let Interrupt::Exception(Exception::PageFault {
            address, ..
        }) = interrupt
        {
            self.cpu.set_gpreg(GpReg::Cr2, address as u64);
        }

        // Bochs, for some reason, sets the RF flag on almost every interrupt, so we do too to avoid execution differences.
        let extra_flags = if matches!(interrupt, Interrupt::Exception(e) if e.class() == ExceptionClass::Fault && e != Exception::Debug)
        {
            1 << 16
        } else {
            0
        };
        let flags = pack_flags(&self.cpu) | extra_flags;
        let (idt_base, idt_byte_size, entry_size) = if self.ctx.protected_mode {
            (self.cpu.gpreg(GpReg::IdtBase), self.cpu.gpreg(GpReg::IdtLimit), 8)
        } else {
            (0, 4096, 4)
        };

        let old_ip = (self.cpu.gpreg(GpReg::Ip) as u32).wrapping_add(interrupt.pc_increment());
        let old_cs = self.cpu.gpreg(GpReg::Cs) as u32;

        let mut stack_transaction = None;

        let offset = n * entry_size + idt_base;
        assert!(
            (n + 1) * entry_size <= idt_byte_size + 1,
            "Entry #{n} of IDT out of range: offset=0x{offset:X}, cpu={}",
            self.cpu
        );

        if self.ctx.protected_mode {
            let vm = self.cpu.flag(Intel386Flag::Vm);

            let old_ss = self.cpu.gpreg(GpReg::Ss);
            let old_sp = self.cpu.gpreg(GpReg::Sp);

            let Ok(entry_val) = self.ctx.memory.read::<u64>(offset as u32, false, &mut self.ctx.mmio_ctx) else {
                let next = self
                    .ctx
                    .trace
                    .as_mut()
                    .map(|trace| (0..5).map(|_| trace.next(false).map(|x| x.into_owned())).collect::<Vec<_>>())
                    .unwrap_or_default();
                panic!(
                    "Reading IDT at 0x{offset:X} for INT 0x{n:X} failed in (k={}): {}\n\nNext trace entries: {next:#X?}",
                    DisplayK(self.ctx.k),
                    self.cpu
                );
            };
            let entry = GateDescriptor::from(entry_val);

            if do_log {
                info!(target: LOG_TARGET, "IDT entry for {interrupt:X?} 0x{entry_val:016X} = {entry:X?}");
            }

            assert!(
                entry.present(),
                "Descriptor 0x{entry_val:016X} = {entry:?} for INT 0x{n:02X} at 0x{offset:X} is not present\n{}",
                self.cpu
            );
            if entry.gate_type() == GateType::TaskGate {
                todo!("Task switching")
            }

            let dpl = entry.dpl().as_u8();
            let new_ip = entry.offset();
            let new_cs = entry.segment_selector();
            let cs_selector = SegmentSelector::from(new_cs);

            let cpl = self.cpu.gpreg(GpReg::Cpl) as u8;
            if is_software_interrupt && dpl < cpl {
                warn!(target: LOG_TARGET, "Permission level not OK for INT{n:02X} ({interrupt:?}): dpl={dpl}, cpl={cpl}, k={}", DisplayK(self.ctx.k));
                return Err(Exception::GeneralProtectionFault((n << 3) as u16 | 2))
            }

            let cs_descriptor = self.load_descriptor(cs_selector)?;

            // Already exit virtual 8086 mode, because lookups should happen in a normal protected mode context
            self.cpu.set_flag(Intel386Flag::Vm, false);

            let mut privilege_level_switch = false;
            if cs_descriptor.access_byte().dpl().as_u8() < self.cpu.gpreg(GpReg::Cpl) as u8 {
                let mut tss: Tss<'_, exec::MmioExecutionContext<'tag>> =
                    Tss::new(self.cpu.gpreg(GpReg::TrBase) as u32, self.ctx.memory, &mut self.ctx.mmio_ctx);
                let (esp, ss) = match cs_selector.rpl().as_u64() {
                    0 => (tss.esp0(), tss.ss0()),
                    1 => (tss.esp1(), tss.ss1()),
                    2 => (tss.esp2(), tss.ss2()),
                    _ => unreachable!(),
                };

                if do_log {
                    debug!(target: LOG_TARGET, "Loaded SP and SS from TSS: 0x{esp:08X}, 0x{ss:04X}");
                }

                self.cpu.set_gpreg(GpReg::Sp, esp as u64);
                self.prepare_segment(GpReg::Ss, ss)?.commit(self);

                privilege_level_switch = true;
            }

            if do_log {
                // TODO: This is somehow incredibly broken...
                let mut bytes_at_ip = [0u8; 16];
                if Cr0::from(self.cpu.gpreg(GpReg::Cr0) as u32).paging() {
                    if let PageWalkResult::PhysAddr {
                        addr, ..
                    } = self.ctx.memory.page_walk(old_ip.wrapping_add(old_cs), false)
                    {
                        self.ctx
                            .memory
                            .read_physical_slice(addr as u32, &mut bytes_at_ip, &mut self.ctx.mmio_ctx)
                            .ok();
                    }
                } else {
                    self.ctx
                        .memory
                        .read_physical_slice(old_ip.wrapping_add(old_cs), &mut bytes_at_ip, &mut self.ctx.mmio_ctx)
                        .ok();
                }

                info!(target: LOG_TARGET, "INT {n:X} = {new_cs:04X}:{new_ip:08X} -- (bytes at old IP: {:02X}, idt: 0x{idt_base:X}; 0x{idt_byte_size:X} bytes -- entry at offset 0x{offset:X} read) (k={})\n{}", bytes_at_ip.iter().format(""), DisplayK(self.ctx.k), self.cpu);
            }

            self.cpu.set_gpreg(GpReg::Ip, new_ip as u64);
            self.prepare_segment(GpReg::Cs, new_cs)?.commit(self);

            if entry.gate_type().is_interrupt_gate() {
                self.cpu.set_flag(Intel386Flag::If, false);
            }

            if entry.gate_type().is_interrupt_gate() || entry.gate_type().is_trap_gate() {
                self.cpu.set_flag(Intel386Flag::Rf, false);
            }

            if vm {
                // Exit virtual 8086 mode and push segments and old SS/ESP
                let tr = stack_transaction.get_or_insert_with(|| {
                    let ss_size = self.read_ss_size();
                    self.ctx.begin_stack_transaction(ss_size, self.op_size, &self.cpu)
                });
                tr.push(self.ctx.memory, &mut self.ctx.mmio_ctx, self.cpu.gpreg(GpReg::Gs) as u32);
                tr.push(self.ctx.memory, &mut self.ctx.mmio_ctx, self.cpu.gpreg(GpReg::Fs) as u32);
                tr.push(self.ctx.memory, &mut self.ctx.mmio_ctx, self.cpu.gpreg(GpReg::Ds) as u32);
                tr.push(self.ctx.memory, &mut self.ctx.mmio_ctx, self.cpu.gpreg(GpReg::Es) as u32);

                self.cpu.set_gpreg(GpReg::Gs, 0);
                self.cpu.set_gpreg(GpReg::Fs, 0);
                self.cpu.set_gpreg(GpReg::Ds, 0);
                self.cpu.set_gpreg(GpReg::Es, 0);

                if do_log {
                    info!(target: LOG_TARGET, "Pushed old segment registers (GS, FS, DS, ES) and far pointer to stack (SS, SP) onto new stack for VM exit: {}", self.cpu);
                }
                privilege_level_switch = true;
            }

            if privilege_level_switch {
                let tr = stack_transaction.get_or_insert_with(|| {
                    let ss_size = self.read_ss_size();
                    self.ctx.begin_stack_transaction(ss_size, self.op_size, &self.cpu)
                });

                tr.push(self.ctx.memory, &mut self.ctx.mmio_ctx, old_ss as u32);
                tr.push(self.ctx.memory, &mut self.ctx.mmio_ctx, old_sp as u32);
            }
        } else {
            let new_ip = self
                .ctx
                .memory
                .read::<u16>(offset as u32, self.cpu.is_userspace(), &mut self.ctx.mmio_ctx)
                .unwrap();
            let new_cs = self
                .ctx
                .memory
                .read::<u16>(
                    (offset + (entry_size - 2)) as u32,
                    self.cpu.is_userspace(),
                    &mut self.ctx.mmio_ctx,
                )
                .unwrap();

            if do_log {
                info!(target: LOG_TARGET, "INT {n:X} = {new_cs:04X}:{new_ip:08X} -- (idt: 0x{idt_base:X}; 0x{idt_byte_size:X} bytes -- entry at offset 0x{offset:X} read)");
                info!(target: LOG_TARGET, "INT {n:X}, Ax=0x{:04X}: {}", self.cpu.gpreg(GpReg::Ax), self.cpu);
            }

            self.cpu.set_gpreg(GpReg::Ip, new_ip as u64);
            self.cpu.set_gpreg(GpReg::Cs, new_cs as u64);
            self.prepare_segment(GpReg::Cs, new_cs)?.commit(self);

            self.cpu.set_flag(Intel386Flag::If, false);
            self.cpu.set_flag(Intel386Flag::Rf, false);
        }

        if self.ctx.trace.is_some() {
            if self.next_expected_interrupt == Some(n as u8) {
                self.next_expected_interrupt = None;
            } else {
                error!(target: LOG_TARGET, "Incorrect interrupt: expected {:X?}, found 0x{:X} -- {interrupt:#X?}", self.next_expected_interrupt, n);
            }
        }

        let tr = stack_transaction.get_or_insert_with(|| {
            let ss_size = self.read_ss_size();
            self.ctx.begin_stack_transaction(ss_size, self.op_size, &self.cpu)
        });
        tr.push(self.ctx.memory, &mut self.ctx.mmio_ctx, flags);
        tr.push(self.ctx.memory, &mut self.ctx.mmio_ctx, old_cs);
        tr.push(self.ctx.memory, &mut self.ctx.mmio_ctx, old_ip);

        if let Some(error_code) = error_code {
            tr.push(self.ctx.memory, &mut self.ctx.mmio_ctx, error_code);
        }

        stack_transaction.unwrap().commit(&mut self.cpu);

        Ok(())
    }

    pub fn hw(&self) -> &Hw {
        &self.ctx.mmio_ctx.hw
    }

    pub fn hw_mut(&mut self) -> &mut Hw {
        &mut self.ctx.mmio_ctx.hw
    }

    pub fn cpu(&self) -> &State {
        &self.cpu
    }

    pub fn intr(&self) -> &Intr {
        &self.intr
    }

    /// Returns true if an interrupt is ready to be fired, in which case [`Self::check_interrupt`] should be checked.
    /// This function is fast, and requires only one atomic load.
    /// It should at least be checked after every memory write,
    /// in order to quickly handle interrupts triggered by writing to the LAPIC's ICR.
    #[inline(always)]
    pub fn interrupt_pending(&self) -> bool {
        self.intr.any_pending()
    }

    pub fn snapshot(&mut self) -> EmulatorSnapshot {
        EmulatorSnapshot {
            cpu: self.cpu.clone(),
            op_size: self.op_size.into(),
            next_expected_interrupt: self.next_expected_interrupt,
            is_halted: self.is_halted,
            trace_triggered_interrupt: self.trace_triggered_interrupt,
            // TODO: Make snapshot of trace that also includes trace.cached_next, so we can use a cheaper seek operation to catch up instead of having to decode every entry.
            trace: self.ctx.trace.as_mut().map(|trace| trace.snapshot()),
            hw: self.ctx.mmio_ctx.hw.snapshot(),
            memory: self.ctx.memory.snapshot(),
            k: self.ctx.k,
        }
    }

    pub fn restore(&mut self, snapshot: EmulatorSnapshot) {
        println!("Restoring...");
        self.cpu = snapshot.cpu;
        self.op_size = snapshot.op_size.into();
        self.next_expected_interrupt = snapshot.next_expected_interrupt;
        self.is_halted = snapshot.is_halted;
        self.trace_triggered_interrupt = snapshot.trace_triggered_interrupt;
        self.ctx.k = snapshot.k;

        println!("Restoring hardware...");
        self.ctx.mmio_ctx.hw.restore(snapshot.hw);

        println!("Restoring memory...");
        self.ctx.memory.restore(snapshot.memory);

        println!("Restoring trace...");
        if let Some(trace_snapshot) = snapshot.trace {
            if self.ctx.k > self.trace_limit {
                self.ctx.trace = None;
            } else {
                let start = Instant::now();
                let trace = self.ctx.trace.take().unwrap();
                self.ctx.trace = Some(trace.restore(trace_snapshot, |done, total| {
                    print!(
                        "\rFast-forwarding trace ({:.1}GiB / {:.1}GiB)...\x1B[0K",
                        done as f64 / (1 << 30) as f64,
                        total as f64 / (1 << 30) as f64
                    );
                    std::io::stdout().flush().unwrap();
                }));

                println!();
                println!("Fast-forwarding took {:.1}s", start.elapsed().as_secs_f64());
            }
        }

        println!("Updating cached CPU state...");
        self.flush_cached_cpu_state();
    }

    pub fn flush_cached_cpu_state(&mut self) {
        // Sets memory.system_write_protect, memory.paging_enabled
        // Sets self.paging_enabled, ctx.protected_mode
        self.update_cr0(Cr0::from(self.cpu.gpreg(GpReg::Cr0) as u32));

        // Sets memory.page_table_addr
        self.update_cr3(self.cpu.gpreg(GpReg::Cr3) as u32);

        // Sets memory.page_size_extension
        self.update_cr4(Cr4::from(self.cpu.gpreg(GpReg::Cr4) as u32));

        // Sets self.segment_sizes
        self.update_segment_sizes();
    }

    pub fn cpu_mut(&mut self) -> &mut State {
        &mut self.cpu
    }

    pub fn result(&self) -> Result<ExecResult, Exception> {
        self.ctx.result.unpack()
    }

    pub fn ctx(&mut self) -> &mut ExecutionContext<'mem, 'tag, Intel386> {
        &mut self.ctx
    }
}

#[cfg(test)]
mod tests {
    use crate::arch::intel386::State;
    use crate::emulator::{pack_flags, unpack_flags};

    #[test]
    pub fn pack_unpack_flags_is_correct() {
        let mut cpu = State::default();
        for n in 0..(1 << 22) {
            unpack_flags(&mut cpu, n, true, false);
            let flags = pack_flags(&cpu);

            assert_eq!(2 | (n & 0x003F_7FD5), flags, "n: {n:022b}, flags: {flags:022b}\n{cpu}");
        }
    }
}
