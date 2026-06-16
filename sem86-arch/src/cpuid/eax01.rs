use bilge::prelude::*;

#[bitsize(32)]
pub struct EdxFeatures {
    /// Floating-Point Unit On-Chip. The processor contains an x87 FPU.
    fpu: bool,
    /// Virtual 8086 Mode Enhancements.
    vme: bool,
    /// Debugging Extensions. Support for I/O breakpoints.
    de: bool,
    /// Page Size Extension. Large 4 MByte pages are supported.
    pse: bool,
    /// Time Stamp Counter. The RDTSC instruction is supported.
    tsc: bool,
    /// Model Specific Registers RDMSR and WRMSR Instructions are supported.
    msr: bool,
    /// Physical Address Extension. Physical addresses >32 bits are supported.
    pae: bool,
    /// Machine Check Exception. Exception 18 is defined for machine checks.
    mce: bool,
    /// CMPXCHG8B Instruction. Compare-and-exchange 8-byte instruction.
    cx8: bool,
    /// APIC On-Chip. The processor contains an Advanced Programmable Interrupt Controller.
    apic: bool,
    /// Reserved.
    reserved: bool,
    /// SYSENTER and SYSEXIT Instructions.
    sep: bool,
    /// Memory Type Range Registers are supported.
    mtrr: bool,
    /// Page Global Bit. Enables global TLB entries.
    pge: bool,
    /// Machine Check Architecture. Machine Check Architecture is supported.
    mca: bool,
    /// Conditional Move Instructions (CMOV) supported.
    cmov: bool,
    /// Page Attribute Table. Augments MTRRs with 4KB granularity.
    pat: bool,
    /// 36-Bit Page Size Extension. Supports >4 GB physical memory with 32-bit paging.
    pse36: bool,
    /// Processor Serial Number is supported and enabled.
    psn: bool,
    /// CLFLUSH Instruction is supported.
    clflush: bool,
    /// Reserved.
    reserved: bool,
    /// Debug Store. BTS and PEBS facilities are supported.
    ds: bool,
    /// Thermal Monitor and software-controlled clock modulation supported.
    acpi: bool,
    /// Intel MMX Technology supported.
    mmx: bool,
    /// FXSAVE and FXRSTOR Instructions are supported.
    fxsr: bool,
    /// SSE supported.
    sse: bool,
    /// SSE2 supported.
    sse2: bool,
    /// Self Snoop. Conflicting memory type management.
    ss: bool,
    /// Hyper-Threading Technology (HTT). Indicates multiple logical processors.
    htt: bool,
    /// Thermal Monitor. Implements automatic thermal control circuitry (TCC).
    tm: bool,
    /// Reserved.
    reserved: bool,
    /// Pending Break Enable. FERR#/PBE# signaling supported during stop-clock state.
    pbe: bool,
}

impl EdxFeatures {
    pub fn as_u32(&self) -> u32 {
        self.value
    }
}
