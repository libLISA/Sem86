use bilge::prelude::*;
use bitcode::{Decode, Encode};
use sem86_arch::mem::{Mem32, Mmio};
use serde::{Deserialize, Serialize};

#[bitsize(32)]
#[derive(Copy, Clone, DebugBits, FromBits)]
pub struct Cr0 {
    pub protected_mode: bool,
    pub monitor_coprocessor: bool,
    pub fpu_emulation: bool,
    pub task_switched: bool,
    pub extension_type: bool,

    /// Numeric Error (NE) bit.
    /// When true, exceptions are generated for errors.
    /// When false, the CPU sets the ERROR# pin which software must poll/wait for.
    pub generate_x87_exceptions: bool,

    reserved: u10,
    pub write_protect: bool,
    reserved: u1,
    pub alignment_mask: bool,
    reserved: u10,
    pub not_write_through: bool,
    pub cache_disable: bool,
    pub paging: bool,
}

impl Cr0 {
    pub fn as_u32(&self) -> u32 {
        self.value
    }
}

#[bitsize(32)]
#[derive(Copy, Clone, DebugBits, FromBits)]
pub struct Cr4 {
    pub virtual_8086_mode_extensions: bool,
    pub protected_mode_virtual_interrupts: bool,
    pub time_stamp_disabled: bool,

    /// When true, accessing DR4 and DR5 causes an InvalidOpcode exception.
    /// When false, DR4 = DR6 and DR5 = DR7.
    pub debugging_extensions: bool,
    pub page_size_extension: bool,
    pub physical_address_extension: bool,
    pub machine_check_exception: bool,
    pub page_global_enabled: bool,
    pub performance_monitoring_counters_enabled: bool,
    pub os_fxsr: bool,
    pub os_xmm_exceptions: bool,
    pub user_mode_instruction_prevention: bool,
    pub linear_57_bit_addresses: bool,
    pub virtual_machine_extensions_enabled: bool,
    pub safer_mode_extensions_enabled: bool,
    reserved: u1,
    pub fsgsbase_enabled: bool,
    pub pcid_enabled: bool,
    pub xsave_enabled: bool,
    reserved: u1,
    pub supervisor_mode_execution_protection_enabled: bool,
    pub supervisor_mode_access_prevention_enabled: bool,
    pub protection_key_enabled: bool,
    pub control_flow_enforcement_technology_enabled: bool,
    pub protection_keys_for_supervisor_mode_pages_enabled: bool,
    reserved: u7,
}

impl Cr4 {
    pub fn as_u32(&self) -> u32 {
        self.value
    }
}

#[bitsize(4)]
#[derive(Copy, Clone, DebugBits, FromBits)]
pub struct CodeOrData {
    pub accessed: bool,
    pub rw: bool,
    /// 0 = expand up, 1 = expand down.
    pub direction_or_conforming: bool,
    pub executable: bool,
}

#[bitsize(4)]
#[derive(Copy, Clone, Debug, FromBits)]
#[repr(u8)]
pub enum DescriptorType {
    AvailableTss16 = 0x1,
    Ldt = 0x2,
    BusyTss16 = 0x3,
    CallGate16,
    TaskGate,
    InterruptGate16,
    TrapGate16,
    AvailableTss32 = 0x9,
    BusyTss32 = 0xB,
    CallGate32 = 0xC,
    InterruptGate32 = 0xE,
    TrapGate32 = 0xF,

    #[fallback]
    Reserved(u4),
}

#[derive(Copy, Clone, Debug)]
pub enum DescriptorInfo {
    CodeOrData(CodeOrData),
    SystemSegment(DescriptorType),
}

impl From<DescriptorInfo> for u4 {
    fn from(value: DescriptorInfo) -> Self {
        match value {
            DescriptorInfo::CodeOrData(code_or_data) => code_or_data.into(),
            DescriptorInfo::SystemSegment(descriptor_type) => descriptor_type.into(),
        }
    }
}

#[bitsize(8)]
#[derive(Copy, Clone, DebugBits, FromBits)]
pub struct SegmentAccessByte {
    low: u4,
    is_code_or_data: bool,
    pub dpl: u2,
    pub present: bool,
}

impl SegmentAccessByte {
    pub fn data(&self) -> DescriptorInfo {
        if self.is_code_or_data() {
            DescriptorInfo::CodeOrData(CodeOrData::from(self.low()))
        } else {
            DescriptorInfo::SystemSegment(DescriptorType::from(self.low()))
        }
    }

    /// Returns true if the descriptor describes a data segment with the `direction` flag set.
    pub fn expands_down(&self) -> bool {
        matches!(self.data(), DescriptorInfo::CodeOrData(info) if !info.executable() && info.direction_or_conforming())
    }

    pub fn set_info(&mut self, info: DescriptorInfo) {
        self.set_low(match info {
            DescriptorInfo::CodeOrData(code_or_data) => u4::from(code_or_data),
            DescriptorInfo::SystemSegment(descriptor_type) => u4::from(descriptor_type),
        })
    }
}

#[bitsize(1)]
#[derive(Copy, Clone, Debug, FromBits)]
pub enum Granularity {
    Byte,
    Page,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
pub enum ExpandedDb {
    Protected16,
    Protected32,
}

impl From<Db> for ExpandedDb {
    fn from(value: Db) -> Self {
        match value {
            Db::Protected16 => ExpandedDb::Protected16,
            Db::Protected32 => ExpandedDb::Protected32,
        }
    }
}

impl From<ExpandedDb> for Db {
    fn from(value: ExpandedDb) -> Self {
        match value {
            ExpandedDb::Protected16 => Db::Protected16,
            ExpandedDb::Protected32 => Db::Protected32,
        }
    }
}

#[bitsize(1)]
#[derive(Copy, Clone, Debug, FromBits, PartialEq, Eq, Hash)]
pub enum Db {
    Protected16,
    Protected32,
}

impl Db {
    pub fn num_bytes(&self) -> usize {
        match self {
            Db::Protected16 => 2,
            Db::Protected32 => 4,
        }
    }
}

#[bitsize(16)]
#[derive(Copy, Clone, DebugBits, FromBits)]
pub struct SegmentSelector {
    pub rpl: u2,
    pub is_local: bool,
    pub segment_index: u13,
}

impl SegmentSelector {
    pub fn is_null(&self) -> bool {
        self.segment_index().as_u16() == 0
    }
}

#[bitsize(4)]
#[derive(Copy, Clone, DebugBits, FromBits)]
pub struct DescriptorFlags {
    pub avl: u1,
    pub long_mode_code: bool,
    pub size: Db,
    pub granularity: Granularity,
}

#[bitsize(64)]
#[derive(Copy, Clone, DebugBits, FromBits)]
pub struct Descriptor {
    limit_low: u16,
    base_low: u24,
    pub access_byte: SegmentAccessByte,
    limit_high: u4,
    pub flags: DescriptorFlags,
    base_high: u8,
}

impl Descriptor {
    pub fn flat_data(size: Db) -> Self {
        let info = DescriptorInfo::CodeOrData(CodeOrData::new(false, true, false, false));
        let access_byte = SegmentAccessByte::new(info.into(), true, u2::new(0), true);
        let flags = DescriptorFlags::new(u1::new(0), false, size, Granularity::Page);
        Self::new(0xffff, u24::new(0), access_byte, u4::new(0xf), flags, 0)
    }

    pub fn from_real_mode_selector(selector: u16) -> Self {
        // TODO: RW should be true
        let info = DescriptorInfo::CodeOrData(CodeOrData::new(false, false, false, false));
        let access_byte = SegmentAccessByte::new(info.into(), true, u2::new(0), true);
        let flags = DescriptorFlags::new(u1::new(0), false, Db::Protected16, Granularity::Byte);
        Descriptor::new(0xffff, u24::new(selector as u32 * 16), access_byte, u4::new(0), flags, 0)
    }

    pub fn offset(&self) -> u32 {
        self.limit_low().as_u32() | ((self.value >> 32) as u32 & 0xffff_0000)
    }

    pub fn base(&self) -> u32 {
        self.base_low().as_u32() | (self.base_high().as_u32() << 24)
    }

    fn limit(&self) -> u32 {
        self.limit_low().as_u32() | (self.limit_high().as_u32() << 16)
    }

    fn effective_limit(&self) -> u32 {
        let n = self.limit();
        match self.flags().granularity() {
            Granularity::Byte => n,
            Granularity::Page => (n << 12) | 0xfff,
        }
    }

    pub fn effective_limit_taking_direction_into_account(&self) -> u32 {
        if self.access_byte().expands_down() {
            // TODO: Should this use u16::MAX when in a 16-bit mode?
            u32::MAX - self.effective_limit()
        } else {
            self.effective_limit()
        }
    }

    pub fn segment_selector(&self) -> u16 {
        self.base_low().as_u32() as u16
    }
}

#[bitsize(64)]
#[derive(Copy, Clone, DebugBits, FromBits)]
pub struct CachedDescriptorAccessRights {
    // TODO: Properly model the lowest bit being set here as meaning "set in real mode".
    reserved: u8,
    pub segment_type: u4,
    pub is_code_or_data: bool,
    pub dpl: u2,
    pub present: bool,
    reserved: u4,
    pub flags: DescriptorFlags,
    reserved: u8,
    pub effective_start: u32,
}

impl From<Descriptor> for CachedDescriptorAccessRights {
    fn from(desc: Descriptor) -> Self {
        let base = desc.base();
        let effective_start = base.wrapping_add(if desc.access_byte().expands_down() {
            // Need 1 extra because the limit check for down-expanding segments is exclusive,
            // while the limit check for up-expanding segments is inclusive.
            desc.effective_limit().wrapping_add(1)
        } else {
            0
        });

        Self::from(((desc.value >> 32) as u32 & 0x00ff_ff00) as u64 | ((effective_start as u64) << 32))
    }
}

#[bitsize(4)]
#[derive(Copy, Clone, Debug, FromBits, PartialEq, Eq)]
#[repr(u8)]
pub enum GateType {
    TaskGate = 0x5,
    InterruptGate16 = 0x6,
    TrapGate16 = 0x7,
    InterruptGate32 = 0xe,
    TrapGate32 = 0xf,

    #[fallback]
    Invalid(u4),
}

impl GateType {
    pub fn is_interrupt_gate(&self) -> bool {
        matches!(self, GateType::InterruptGate16 | GateType::InterruptGate32)
    }

    pub fn is_trap_gate(&self) -> bool {
        matches!(self, GateType::TrapGate16 | GateType::TrapGate32)
    }
}

#[bitsize(64)]
#[derive(Copy, Clone, DebugBits, FromBits)]
pub struct GateDescriptor {
    offset_low: u16,
    pub segment_selector: u16,
    reserved: u8,
    pub gate_type: GateType,
    reserved: u1,
    pub dpl: u2,
    pub present: bool,
    offset_high: u16,
}

impl GateDescriptor {
    pub fn offset(&self) -> u32 {
        self.offset_low() as u32 | ((self.offset_high() as u32) << 16)
    }
}

pub struct Tss<'m, M> {
    addr: u32,
    mem: &'m Mem32,
    mmio: &'m mut M,
}

impl<'m, M: Mmio> Tss<'m, M> {
    pub fn new(addr: u32, mem: &'m Mem32, mmio: &'m mut M) -> Self {
        Self {
            addr,
            mem,
            mmio,
        }
    }

    fn u16(&mut self, offset: u32) -> u16 {
        self.mem.read_u16(self.addr + offset, false, self.mmio).unwrap()
    }

    fn u32(&mut self, offset: u32) -> u32 {
        self.mem.read_u32(self.addr + offset, false, self.mmio).unwrap()
    }

    pub fn link(&mut self) -> u16 {
        self.u16(0x00)
    }

    pub fn esp0(&mut self) -> u32 {
        self.u32(0x04)
    }
    pub fn ss0(&mut self) -> u16 {
        self.u16(0x08)
    }

    pub fn esp1(&mut self) -> u32 {
        self.u32(0x04)
    }
    pub fn ss1(&mut self) -> u16 {
        self.u16(0x08)
    }

    pub fn esp2(&mut self) -> u32 {
        self.u32(0x04)
    }
    pub fn ss2(&mut self) -> u16 {
        self.u16(0x08)
    }

    pub fn iopb(&mut self) -> u16 {
        self.u16(0x66)
    }
}
