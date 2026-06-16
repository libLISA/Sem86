use bilge::prelude::*;
use bitcode::{Decode, Encode};
use serde::{Deserialize, Serialize};

#[bitsize(8)]
#[derive(
    Copy,
    Clone,
    DebugBits,
    FromBits,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    Encode,
    Decode,
    mem_dbg::MemSize,
)]
pub struct PageFaultCode {
    /// When true, page accessed was present.
    /// False otherwise.
    present: bool,

    /// When true, page fault occurred during a write.
    /// When false, page fault occurred during a read.
    write: bool,

    /// When true, page fault occurred while CPL=3.
    user: bool,

    /// When true, page directory or table entries contained '1's in reserved bits.
    /// Only set when PSE or PAE is enabled in CR4.
    invalid_reservd_bits: bool,

    /// When true, page fault occurred during an instruction fetch.
    instruction_fetch: bool,

    /// When true, page fault was caused by a protection key violation.
    protection_key_violation: bool,

    /// When true, page fault was caused by a shadow stack access.
    shadow_stack_access: bool,

    /// When true, fault was caused by an SGX violation. (Software Guard Extensions)
    sgx_violation: bool,
}

impl PageFaultCode {
    pub fn from_normal_access(present: bool, write: bool, user: bool, invalid_reserved_bits: bool) -> Self {
        Self::new(present, write, user, invalid_reserved_bits, false, false, false, false)
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Encode, Decode, mem_dbg::MemSize)]
pub enum Exception {
    DivisionError,
    Debug,
    NonMaskableInterrupt,
    Breakpoint,
    Overflow,
    BoundRangeExceeded,
    InvalidOpcode,
    DeviceNotAvailable,
    DoubleFault(u16),
    CoprocessorSegmentOverrun,
    InvalidTss(u16),
    SegmentNotPresent(u16),
    StackSegmentationFault(u16),
    GeneralProtectionFault(u16),
    PageFault { code: PageFaultCode, address: u32 },
    FloatingPointException,
    AlignmentCheck(u16),
    MachineCheck,
    SimdFloatingPointException,
    VirtualizationException,
    ControlProtectionException(u16),
    HypervisorInjectionException,
    VmmCommunicationException(u16),
    SecurityException,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ExceptionClass {
    Trap,
    Fault,
    Abort,
}

impl Exception {
    #[inline(always)]
    pub fn from_vector_and_params(vector: u8, code: u32, address: u32) -> Self {
        use Exception::*;

        match vector {
            0x0 => DivisionError,
            0x1 => Debug,
            0x2 => NonMaskableInterrupt,
            0x3 => Breakpoint,
            0x4 => Overflow,
            0x5 => BoundRangeExceeded,
            0x6 => InvalidOpcode,
            0x7 => DeviceNotAvailable,
            0x8 => DoubleFault(code.try_into().unwrap()),
            0x9 => CoprocessorSegmentOverrun,
            0xA => InvalidTss(code.try_into().unwrap()),
            0xB => SegmentNotPresent(code.try_into().unwrap()),
            0xC => StackSegmentationFault(code.try_into().unwrap()),
            0xD => GeneralProtectionFault(code.try_into().unwrap()),
            0xE => PageFault {
                code: PageFaultCode::from(code as u8),
                address,
            },
            0x10 => FloatingPointException,
            0x11 => AlignmentCheck(code.try_into().unwrap()),
            0x12 => MachineCheck,
            0x13 => SimdFloatingPointException,
            0x14 => VirtualizationException,
            0x15 => ControlProtectionException(code.try_into().unwrap()),
            0x1c => HypervisorInjectionException,
            0x1d => VmmCommunicationException(code.try_into().unwrap()),
            0x1e => SecurityException,
            _ => unreachable!(),
        }
    }

    #[inline(always)]
    pub fn as_u8(&self) -> u8 {
        use Exception::*;

        match self {
            DivisionError => 0x0,
            Debug => 0x1,
            NonMaskableInterrupt => 0x2,
            Breakpoint => 0x3,
            Overflow => 0x4,
            BoundRangeExceeded => 0x5,
            InvalidOpcode => 0x6,
            DeviceNotAvailable => 0x7,
            DoubleFault(_) => 0x8,
            CoprocessorSegmentOverrun => 0x9,
            InvalidTss(_) => 0xA,
            SegmentNotPresent(_) => 0xB,
            StackSegmentationFault(_) => 0xC,
            GeneralProtectionFault(_) => 0xD,
            PageFault {
                ..
            } => 0xE,
            FloatingPointException => 0x10,
            AlignmentCheck(_) => 0x11,
            MachineCheck => 0x12,
            SimdFloatingPointException => 0x13,
            VirtualizationException => 0x14,
            ControlProtectionException(_) => 0x15,
            HypervisorInjectionException => 0x1c,
            VmmCommunicationException(_) => 0x1d,
            SecurityException => 0x1e,
        }
    }

    #[inline(always)]
    pub fn with_code_from_u32(&self, code: u32) -> Self {
        use Exception::*;

        match *self {
            DoubleFault(_) => DoubleFault(code.try_into().unwrap()),
            InvalidTss(_) => InvalidTss(code.try_into().unwrap()),
            SegmentNotPresent(_) => SegmentNotPresent(code.try_into().unwrap()),
            StackSegmentationFault(_) => StackSegmentationFault(code.try_into().unwrap()),
            GeneralProtectionFault(_) => GeneralProtectionFault(code.try_into().unwrap()),
            PageFault {
                address, ..
            } => PageFault {
                code: PageFaultCode::from(code as u8),
                address,
            },
            AlignmentCheck(_) => AlignmentCheck(code.try_into().unwrap()),
            ControlProtectionException(_) => ControlProtectionException(code.try_into().unwrap()),
            VmmCommunicationException(_) => VmmCommunicationException(code.try_into().unwrap()),
            BoundRangeExceeded
            | InvalidOpcode
            | DeviceNotAvailable
            | CoprocessorSegmentOverrun
            | FloatingPointException
            | MachineCheck
            | SimdFloatingPointException
            | VirtualizationException
            | HypervisorInjectionException
            | SecurityException
            | DivisionError
            | Debug
            | NonMaskableInterrupt
            | Breakpoint
            | Overflow => *self,
        }
    }

    #[inline(always)]
    pub fn code_as_u32(&self) -> Option<u32> {
        use Exception::*;

        match *self {
            DoubleFault(n) => Some(n.into()),
            InvalidTss(n) => Some(n.into()),
            SegmentNotPresent(n) => Some(n.into()),
            StackSegmentationFault(n) => Some(n.into()),
            GeneralProtectionFault(n) => Some(n.into()),
            PageFault {
                code, ..
            } => Some(u8::from(code) as u32),
            AlignmentCheck(n) => Some(n.into()),
            ControlProtectionException(n) => Some(n.into()),
            VmmCommunicationException(n) => Some(n.into()),
            BoundRangeExceeded
            | InvalidOpcode
            | DeviceNotAvailable
            | CoprocessorSegmentOverrun
            | FloatingPointException
            | MachineCheck
            | SimdFloatingPointException
            | VirtualizationException
            | HypervisorInjectionException
            | SecurityException
            | DivisionError
            | Debug
            | NonMaskableInterrupt
            | Breakpoint
            | Overflow => None,
        }
    }

    #[inline(always)]
    pub fn address(&self) -> Option<u32> {
        if let Self::PageFault {
            address, ..
        } = *self
        {
            Some(address)
        } else {
            None
        }
    }

    #[inline(always)]
    pub fn class(&self) -> ExceptionClass {
        use Exception::*;

        match *self {
            MachineCheck => ExceptionClass::Abort,
            Breakpoint | Overflow => ExceptionClass::Trap,
            _ => ExceptionClass::Fault,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Interrupt {
    Exception(Exception),
    HardwareInterrupt(u8),
    SoftwareInterrupt { vector: u8, pc_increment: u8 },
}
impl Interrupt {
    #[inline(always)]
    pub fn vector(&self) -> u8 {
        match *self {
            Interrupt::Exception(exception) => exception.as_u8(),
            Interrupt::HardwareInterrupt(n) => n,
            Interrupt::SoftwareInterrupt {
                vector, ..
            } => vector,
        }
    }

    #[inline(always)]
    pub fn code(&self) -> Option<u32> {
        match *self {
            Interrupt::Exception(exception) => exception.code_as_u32(),
            Interrupt::HardwareInterrupt(_) => None,
            Interrupt::SoftwareInterrupt {
                ..
            } => None,
        }
    }

    #[inline(always)]
    pub fn pc_increment(&self) -> u32 {
        match *self {
            Interrupt::SoftwareInterrupt {
                pc_increment, ..
            } => pc_increment as u32,
            _ => 0,
        }
    }
}

impl From<Exception> for Interrupt {
    #[inline(always)]
    fn from(exception: Exception) -> Self {
        Interrupt::Exception(exception)
    }
}
