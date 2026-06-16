use std::fmt::Display;
use std::mem::offset_of;

use bitcode::{Decode, Encode};
use itertools::Itertools;
use liblisa::arch::{Arch, CpuState, Flag, NumberedRegister, Register};
use liblisa::encoding::{ParLoc, UnsizedParLoc};
use liblisa::state::Size;
use liblisa::value::{MutValue, Value, ValueType};
use num_derive::{FromPrimitive, ToPrimitive};
use num_traits::{FromPrimitive, ToPrimitive};
use serde::{Deserialize, Serialize};
use strum::{EnumCount, VariantArray};

#[derive(
    Copy,
    Clone,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Deserialize,
    Serialize,
    ToPrimitive,
    FromPrimitive,
    Encode,
    Decode,
    mem_dbg::MemSize,
)]
pub enum HandlerId {
    Int,
    Iret,
    WriteCr,
    CpuId,
    ReadMsr,
    WriteMsr,
    CsUpdated,
    SsUpdated,
    IfUpdated,
    InvalidatePage,
    Halt,
}

pub const HANDLER_INT: HandlerId = HandlerId::Int;
pub const HANDLER_IRET: HandlerId = HandlerId::Iret;
pub const HANDLER_WRITE_CR: HandlerId = HandlerId::WriteCr;
pub const HANDLER_CPUID: HandlerId = HandlerId::CpuId;
pub const HANDLER_RDMSR: HandlerId = HandlerId::ReadMsr;
pub const HANDLER_WRMSR: HandlerId = HandlerId::WriteMsr;
pub const HANDLER_CS_UPDATED: HandlerId = HandlerId::CsUpdated;
pub const HANDLER_SS_UPDATED: HandlerId = HandlerId::SsUpdated;
pub const HANDLER_IF_UPDATED: HandlerId = HandlerId::IfUpdated;
pub const HANDLER_INVALIDATE_PAGE: HandlerId = HandlerId::InvalidatePage;
pub const HANDLER_HALT: HandlerId = HandlerId::Halt;

pub const FLAG_ZF: ParLoc<Intel386> = ParLoc {
    loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Flags1)),
    size: Size::one_byte(Intel386Flag::Zf as usize),
};
pub const FLAG_CF: ParLoc<Intel386> = ParLoc {
    loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Flags1)),
    size: Size::one_byte(Intel386Flag::Cf as usize),
};
pub const FLAG_SF: ParLoc<Intel386> = ParLoc {
    loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Flags1)),
    size: Size::one_byte(Intel386Flag::Sf as usize),
};
pub const FLAG_DF: ParLoc<Intel386> = ParLoc {
    loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Flags1)),
    size: Size::one_byte(Intel386Flag::Df as usize),
};
pub const FLAG_PF: ParLoc<Intel386> = ParLoc {
    loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Flags1)),
    size: Size::one_byte(Intel386Flag::Pf as usize),
};
pub const FLAG_AF: ParLoc<Intel386> = ParLoc {
    loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Flags1)),
    size: Size::one_byte(Intel386Flag::Af as usize),
};
pub const FLAG_OF: ParLoc<Intel386> = ParLoc {
    loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Flags1)),
    size: Size::one_byte(Intel386Flag::Of as usize),
};
pub const FLAG_IF: ParLoc<Intel386> = ParLoc {
    loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Flags1)),
    size: Size::one_byte(Intel386Flag::If as usize),
};
pub const FLAG_RF: ParLoc<Intel386> = ParLoc {
    loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Flags2)),
    size: Size::one_byte(Intel386Flag::Rf as usize - 8),
};
pub const FLAG_VM: ParLoc<Intel386> = ParLoc {
    loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Flags2)),
    size: Size::one_byte(Intel386Flag::Vm as usize - 8),
};
pub const FLAG_TF: ParLoc<Intel386> = ParLoc {
    loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Flags2)),
    size: Size::one_byte(Intel386Flag::Tf as usize - 8),
};
pub const FLAG_NT: ParLoc<Intel386> = ParLoc {
    loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Flags2)),
    size: Size::one_byte(Intel386Flag::Nt as usize - 8),
};
pub const FLAG_AC: ParLoc<Intel386> = ParLoc {
    loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Flags2)),
    size: Size::one_byte(Intel386Flag::Ac as usize - 8),
};
pub const FLAG_ID: ParLoc<Intel386> = ParLoc {
    loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Flags2)),
    size: Size::one_byte(Intel386Flag::Id as usize - 8),
};

pub const FLAG_X87_INVALID_OPERATION: ParLoc<Intel386> = ParLoc {
    loc: UnsizedParLoc::Reg(Reg::X87(X87Reg::ExceptionFlags)),
    size: Size::one_byte(X87Flag::InvalidOperation as usize),
};
pub const FLAG_X87_DENORMALIZED_OPERAND: ParLoc<Intel386> = ParLoc {
    loc: UnsizedParLoc::Reg(Reg::X87(X87Reg::ExceptionFlags)),
    size: Size::one_byte(X87Flag::DenormalizedOperand as usize),
};
pub const FLAG_X87_ZERO_DIVIDE: ParLoc<Intel386> = ParLoc {
    loc: UnsizedParLoc::Reg(Reg::X87(X87Reg::ExceptionFlags)),
    size: Size::one_byte(X87Flag::ZeroDevide as usize),
};
pub const FLAG_X87_OVERFLOW: ParLoc<Intel386> = ParLoc {
    loc: UnsizedParLoc::Reg(Reg::X87(X87Reg::ExceptionFlags)),
    size: Size::one_byte(X87Flag::Overflow as usize),
};
pub const FLAG_X87_UNDERFLOW: ParLoc<Intel386> = ParLoc {
    loc: UnsizedParLoc::Reg(Reg::X87(X87Reg::ExceptionFlags)),
    size: Size::one_byte(X87Flag::Underflow as usize),
};
pub const FLAG_X87_PRECISION: ParLoc<Intel386> = ParLoc {
    loc: UnsizedParLoc::Reg(Reg::X87(X87Reg::ExceptionFlags)),
    size: Size::one_byte(X87Flag::Precision as usize),
};
pub const FLAG_X87_STACK_FAULT: ParLoc<Intel386> = ParLoc {
    loc: UnsizedParLoc::Reg(Reg::X87(X87Reg::ExceptionFlags)),
    size: Size::one_byte(X87Flag::StackFault as usize),
};

pub const FLAG_X87_MASKED_INVALID_OPERATION: ParLoc<Intel386> = ParLoc {
    loc: UnsizedParLoc::Reg(Reg::X87(X87Reg::ExceptionMasks)),
    size: Size::one_byte(X87Flag::InvalidOperation as usize),
};
pub const FLAG_X87_MASKED_DENORMALIZED_OPERAND: ParLoc<Intel386> = ParLoc {
    loc: UnsizedParLoc::Reg(Reg::X87(X87Reg::ExceptionMasks)),
    size: Size::one_byte(X87Flag::DenormalizedOperand as usize),
};
pub const FLAG_X87_MASKED_ZERO_DIVIDE: ParLoc<Intel386> = ParLoc {
    loc: UnsizedParLoc::Reg(Reg::X87(X87Reg::ExceptionMasks)),
    size: Size::one_byte(X87Flag::ZeroDevide as usize),
};
pub const FLAG_X87_MASKED_OVERFLOW: ParLoc<Intel386> = ParLoc {
    loc: UnsizedParLoc::Reg(Reg::X87(X87Reg::ExceptionMasks)),
    size: Size::one_byte(X87Flag::Overflow as usize),
};
pub const FLAG_X87_MASKED_UNDERFLOW: ParLoc<Intel386> = ParLoc {
    loc: UnsizedParLoc::Reg(Reg::X87(X87Reg::ExceptionMasks)),
    size: Size::one_byte(X87Flag::Underflow as usize),
};
pub const FLAG_X87_MASKED_PRECISION: ParLoc<Intel386> = ParLoc {
    loc: UnsizedParLoc::Reg(Reg::X87(X87Reg::ExceptionMasks)),
    size: Size::one_byte(X87Flag::Precision as usize),
};
pub const FLAG_X87_MASKED_STACK_FAULT: ParLoc<Intel386> = ParLoc {
    loc: UnsizedParLoc::Reg(Reg::X87(X87Reg::ExceptionMasks)),
    size: Size::one_byte(X87Flag::StackFault as usize),
};

pub const FLAG_X87_CC0: ParLoc<Intel386> = ParLoc {
    loc: UnsizedParLoc::Reg(Reg::X87(X87Reg::ConditionCodes)),
    size: Size::one_byte(0),
};
pub const FLAG_X87_CC1: ParLoc<Intel386> = ParLoc {
    loc: UnsizedParLoc::Reg(Reg::X87(X87Reg::ConditionCodes)),
    size: Size::one_byte(1),
};
pub const FLAG_X87_CC2: ParLoc<Intel386> = ParLoc {
    loc: UnsizedParLoc::Reg(Reg::X87(X87Reg::ConditionCodes)),
    size: Size::one_byte(2),
};
pub const FLAG_X87_CC3: ParLoc<Intel386> = ParLoc {
    loc: UnsizedParLoc::Reg(Reg::X87(X87Reg::ConditionCodes)),
    size: Size::one_byte(3),
};

#[derive(
    Copy,
    Clone,
    Debug,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    Encode,
    Decode,
    strum::EnumCount,
    strum::VariantArray,
)]
pub enum X87Flag {
    InvalidOperation,
    DenormalizedOperand,
    ZeroDevide,
    Overflow,
    Underflow,
    Precision,
    StackFault,
}

#[derive(
    Copy,
    Clone,
    Debug,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    Encode,
    Decode,
    strum::EnumCount,
    strum::VariantArray,
)]
pub enum Intel386Flag {
    // Flags 1
    Cf = 0,
    Pf,
    Af,
    Zf,
    Sf,
    Df,
    Of,
    If,

    // Flags 2
    Nt,
    Tf,
    Rf,
    Vm,
    Ac,
    Vif,
    Vip,
    Id,
}

impl Display for Intel386Flag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Intel386Flag::Cf => "CF",
            Intel386Flag::Pf => "PF",
            Intel386Flag::Af => "AF",
            Intel386Flag::Zf => "ZF",
            Intel386Flag::Sf => "SF",
            Intel386Flag::Df => "DF",
            Intel386Flag::Of => "OF",
            Intel386Flag::If => "IF",
            Intel386Flag::Nt => "NT",
            Intel386Flag::Tf => "TF",
            Intel386Flag::Rf => "RF",
            Intel386Flag::Vm => "VM",
            Intel386Flag::Ac => "AC",
            Intel386Flag::Vif => "VIF",
            Intel386Flag::Vip => "VIP",
            Intel386Flag::Id => "ID",
        })
    }
}

impl Flag for Intel386Flag {
    fn iter() -> impl Iterator<Item = Self> {
        [
            Intel386Flag::Cf,
            Intel386Flag::Pf,
            Intel386Flag::Af,
            Intel386Flag::Zf,
            Intel386Flag::Sf,
            Intel386Flag::Df,
            Intel386Flag::Of,
            Intel386Flag::If,
            Intel386Flag::Nt,
            Intel386Flag::Tf,
            Intel386Flag::Rf,
            Intel386Flag::Vm,
            Intel386Flag::Ac,
            Intel386Flag::Vif,
            Intel386Flag::Vip,
            Intel386Flag::Id,
        ]
        .into_iter()
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Encode, Decode, mem_dbg::MemSize)]
pub enum Reg {
    Gp(GpReg),
    X87(X87Reg),
}

impl Reg {
    pub fn is_segment_base(&self) -> bool {
        matches!(self, Reg::Gp(reg) if reg.is_segment_base())
    }
}

impl Register for Reg {
    fn is_pc(&self) -> bool {
        matches!(self, Reg::Gp(GpReg::Ip))
    }

    fn is_zero(&self) -> bool {
        matches!(self, Reg::Gp(GpReg::Riz))
    }

    fn is_flags(&self) -> bool {
        match self {
            Reg::Gp(gp_reg) => gp_reg.is_flags(),
            Reg::X87(x87_reg) => x87_reg.is_flags(),
        }
    }

    fn mask(&self) -> Option<u64> {
        match self {
            Reg::Gp(gp_reg) => gp_reg.mask(),
            Reg::X87(x87_reg) => x87_reg.mask(),
        }
    }

    fn is_addr_reg(&self) -> bool {
        todo!()
    }

    fn byte_size(&self) -> usize {
        match self {
            Reg::Gp(gp_reg) => gp_reg.byte_size(),
            Reg::X87(x87_reg) => x87_reg.byte_size(),
        }
    }

    fn reg_type(self) -> ValueType {
        match self {
            Reg::Gp(gp_reg) => gp_reg.reg_type(),
            Reg::X87(x87_reg) => x87_reg.reg_type(),
        }
    }
}

impl From<GpReg> for Reg {
    fn from(value: GpReg) -> Self {
        Self::Gp(value)
    }
}

impl Display for Reg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Reg::Gp(gp_reg) => Display::fmt(gp_reg, f),
            Reg::X87(x87_reg) => Display::fmt(x87_reg, f),
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Encode, Decode, mem_dbg::MemSize)]
pub enum X87Reg {
    MmIsValid,
    DataPointer,
    DataSelector,
    InstructionPointer,
    InstructionSelector,
    LastInstructionOpcode,
    Top,
    ConditionCodes,
    ExceptionFlags,
    ExceptionMasks,
    RoundingControl,
    PrecisionControl,
    InfinityControl,
    Mm(u8),
}

impl Register for X87Reg {
    fn is_pc(&self) -> bool {
        false
    }

    fn is_zero(&self) -> bool {
        false
    }

    fn is_flags(&self) -> bool {
        false
        // TODO: matches!(self, Self::ExceptionFlags | Self::ExceptionMasks | Self::ConditionCodes)
    }

    fn mask(&self) -> Option<u64> {
        match self {
            X87Reg::ExceptionFlags => Some(0x01010101_01010101),
            X87Reg::ExceptionMasks => Some(0x0101_01010101),
            X87Reg::ConditionCodes => Some(0x01010101),
            _ => None,
        }
    }

    fn is_addr_reg(&self) -> bool {
        false
    }

    fn byte_size(&self) -> usize {
        match self {
            X87Reg::Top | X87Reg::RoundingControl | X87Reg::PrecisionControl | X87Reg::InfinityControl => 1,
            X87Reg::DataSelector | X87Reg::InstructionSelector => 2,
            X87Reg::ConditionCodes => 4,
            X87Reg::MmIsValid => 8,
            X87Reg::DataPointer | X87Reg::InstructionPointer | X87Reg::LastInstructionOpcode => 4,
            X87Reg::ExceptionFlags | X87Reg::ExceptionMasks => 8,
            X87Reg::Mm(_) => 16,
        }
    }

    fn reg_type(self) -> ValueType {
        match self {
            X87Reg::Mm(_) => ValueType::Bytes(16),
            _ => ValueType::Num,
        }
    }
}

impl Display for X87Reg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            X87Reg::Top => write!(f, "TOP"),
            X87Reg::ConditionCodes => write!(f, "CC"),
            X87Reg::ExceptionFlags => write!(f, "EF"),
            X87Reg::Mm(n) => write!(f, "MM{n}"),
            X87Reg::MmIsValid => write!(f, "TAG"),
            X87Reg::DataPointer => write!(f, "FDP"),
            X87Reg::DataSelector => write!(f, "FDS"),
            X87Reg::InstructionPointer => write!(f, "FIP"),
            X87Reg::InstructionSelector => write!(f, "FIS"),
            X87Reg::LastInstructionOpcode => write!(f, "LASTOP"),
            X87Reg::ExceptionMasks => write!(f, "EM"),
            X87Reg::RoundingControl => write!(f, "RC"),
            X87Reg::PrecisionControl => write!(f, "PC"),
            X87Reg::InfinityControl => write!(f, "X"),
        }
    }
}

#[derive(
    Copy,
    Clone,
    Debug,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    Encode,
    Decode,
    FromPrimitive,
    ToPrimitive,
    strum::EnumCount,
    strum::VariantArray,
    mem_dbg::MemSize,
)]
pub enum GpReg {
    Ax,
    Cx,
    Dx,
    Bx,
    Sp,
    Bp,
    Si,
    Di,
    // ! WARNING: Do not change [seg, base, ar, limit] ordering without also updating JITs
    // Keep ES-CS-SS-DS ordering for performance
    Es,
    EsBase,
    EsAr,
    EsLimit,
    Cs,
    CsBase,
    CsAr,
    CsLimit,
    Ss,
    SsBase,
    SsAr,
    SsLimit,
    Ds,
    DsBase,
    DsAr,
    DsLimit,
    Fs,
    FsBase,
    FsAr,
    FsLimit,
    Gs,
    GsBase,
    GsAr,
    GsLimit,
    Ip,
    Cr0,
    Cr2,
    Cr3,
    Cr4,
    GdtBase,
    GdtLimit,
    IdtBase,
    IdtLimit,
    Ldt,
    LdtBase,
    LdtLimit,
    Iopl,
    /// Lowest 8 bits are used to store CPL.
    /// Bits 8-16 are used to store CPL != 0 as a boolean (0 or 1).
    Cpl,
    Tr,
    TrBase,
    TrLimit,
    Dr0,
    Dr1,
    Dr2,
    Dr3,
    Dr6,
    Dr7,
    SysEnterCs,
    SysEnterIp,
    SysEnterSp,
    Flags1,
    Flags2,
    Riz,
}

impl Display for GpReg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GpReg::Ax => write!(f, "AX"),
            GpReg::Dx => write!(f, "DX"),
            GpReg::Cx => write!(f, "CX"),
            GpReg::Bx => write!(f, "BX"),
            GpReg::Bp => write!(f, "BP"),
            GpReg::Si => write!(f, "SI"),
            GpReg::Di => write!(f, "DI"),
            GpReg::Sp => write!(f, "SP"),
            GpReg::Cs => write!(f, "CS"),
            GpReg::Ds => write!(f, "DS"),
            GpReg::Ss => write!(f, "SS"),
            GpReg::Es => write!(f, "ES"),
            GpReg::Fs => write!(f, "FS"),
            GpReg::Gs => write!(f, "GS"),
            GpReg::CsBase => write!(f, "CS_BASE"),
            GpReg::DsBase => write!(f, "DS_BASE"),
            GpReg::SsBase => write!(f, "SS_BASE"),
            GpReg::EsBase => write!(f, "ES_BASE"),
            GpReg::FsBase => write!(f, "FS_BASE"),
            GpReg::GsBase => write!(f, "GS_BASE"),
            GpReg::CsAr => write!(f, "CS_AR"),
            GpReg::DsAr => write!(f, "DS_AR"),
            GpReg::SsAr => write!(f, "SS_AR"),
            GpReg::EsAr => write!(f, "ES_AR"),
            GpReg::FsAr => write!(f, "FS_AR"),
            GpReg::GsAr => write!(f, "GS_AR"),
            GpReg::CsLimit => write!(f, "CS_LIMIT"),
            GpReg::DsLimit => write!(f, "DS_LIMIT"),
            GpReg::SsLimit => write!(f, "SS_LIMIT"),
            GpReg::EsLimit => write!(f, "ES_LIMIT"),
            GpReg::FsLimit => write!(f, "FS_LIMIT"),
            GpReg::GsLimit => write!(f, "GS_LIMIT"),
            GpReg::Ip => write!(f, "IP"),
            GpReg::Cr0 => write!(f, "CR0"),
            GpReg::Cr2 => write!(f, "CR2"),
            GpReg::Cr3 => write!(f, "CR3"),
            GpReg::Cr4 => write!(f, "CR4"),
            GpReg::GdtBase => write!(f, "GDT"),
            GpReg::GdtLimit => write!(f, "GDT_LIMIT"),
            GpReg::IdtBase => write!(f, "IDT"),
            GpReg::IdtLimit => write!(f, "IDT_LIMIT"),
            GpReg::Ldt => write!(f, "LDT"),
            GpReg::LdtBase => write!(f, "LDT_BASE"),
            GpReg::LdtLimit => write!(f, "LDT_LIMIT"),
            GpReg::Iopl => write!(f, "IOPL"),
            GpReg::Cpl => write!(f, "CPL"),
            GpReg::Tr => write!(f, "TR"),
            GpReg::TrBase => write!(f, "TR_BASE"),
            GpReg::TrLimit => write!(f, "TR_LIMIT"),
            GpReg::Flags1 => write!(f, "EFLAGS1"),
            GpReg::Flags2 => write!(f, "EFLAGS2"),
            GpReg::Dr0 => write!(f, "DR0"),
            GpReg::Dr1 => write!(f, "DR1"),
            GpReg::Dr2 => write!(f, "DR2"),
            GpReg::Dr3 => write!(f, "DR3"),
            GpReg::Dr6 => write!(f, "DR6"),
            GpReg::Dr7 => write!(f, "DR7"),
            GpReg::SysEnterCs => write!(f, "SYSENTER_CS"),
            GpReg::SysEnterIp => write!(f, "SYSENTER_IP"),
            GpReg::SysEnterSp => write!(f, "SYSENTER_SP"),
            GpReg::Riz => write!(f, "RZ"),
        }
    }
}

impl GpReg {
    pub fn is_sreg(&self) -> bool {
        use GpReg::*;
        matches!(
            self,
            Cs | CsBase
                | CsAr
                | CsLimit
                | Ds
                | DsBase
                | DsAr
                | DsLimit
                | Ss
                | SsBase
                | SsAr
                | SsLimit
                | Es
                | EsBase
                | EsAr
                | EsLimit
                | Fs
                | FsBase
                | FsAr
                | FsLimit
                | Gs
                | GsBase
                | GsAr
                | GsLimit
        )
    }

    /// When passed a segment selector reg, returns the related `(limit, access_rights, base)` segment registers.
    pub fn related_segment_regs(&self) -> (GpReg, GpReg, GpReg) {
        match self {
            GpReg::Cs => (GpReg::CsBase, GpReg::CsLimit, GpReg::CsAr),
            GpReg::Ds => (GpReg::DsBase, GpReg::DsLimit, GpReg::DsAr),
            GpReg::Es => (GpReg::EsBase, GpReg::EsLimit, GpReg::EsAr),
            GpReg::Ss => (GpReg::SsBase, GpReg::SsLimit, GpReg::SsAr),
            GpReg::Fs => (GpReg::FsBase, GpReg::FsLimit, GpReg::FsAr),
            GpReg::Gs => (GpReg::GsBase, GpReg::GsLimit, GpReg::GsAr),
            _ => panic!("not a segment: {self:?}"),
        }
    }

    pub fn is_segment_base(&self) -> bool {
        use GpReg::*;
        matches!(self, CsBase | DsBase | EsBase | SsBase | FsBase | GsBase)
    }
}

impl NumberedRegister for GpReg {
    fn as_num(&self) -> usize {
        self.to_usize().unwrap()
    }

    fn from_num(num: usize) -> Self {
        Self::from_usize(num).unwrap()
    }
}

impl Register for GpReg {
    fn is_pc(&self) -> bool {
        self == &GpReg::Ip
    }

    fn is_zero(&self) -> bool {
        self == &GpReg::Riz
    }

    #[inline]
    fn is_flags(&self) -> bool {
        self == &GpReg::Flags1 || self == &GpReg::Flags2
    }

    fn mask(&self) -> Option<u64> {
        if Register::is_flags(self) {
            Some(0x0101_0101_0101_0101)
        } else if *self == GpReg::Iopl {
            Some(0x3)
        } else {
            None
        }
    }

    fn is_addr_reg(&self) -> bool {
        false
    }

    fn byte_size(&self) -> usize {
        match self {
            GpReg::Flags1 | GpReg::Flags2 | GpReg::CsAr | GpReg::DsAr | GpReg::EsAr | GpReg::SsAr | GpReg::FsAr | GpReg::GsAr => {
                8
            },
            _ => 4,
        }
    }

    fn reg_type(self) -> ValueType {
        ValueType::Num
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct State {
    #[serde(with = "serde_big_array::BigArray")]
    pub(crate) regs: [u64; GpReg::COUNT],
    pub(crate) x87: X87State,
}

#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize, Encode, Decode)]
pub struct X87State {
    pub(crate) mm: [u128; 8],
    pub(crate) top: u8,
    pub(crate) condition_codes: u32,
    pub(crate) mm_is_valid: u64,
    pub(crate) data_pointer: u64,
    pub(crate) data_selector: u64,
    pub(crate) instruction_pointer: u64,
    pub(crate) instruction_selector: u64,
    pub(crate) last_instruction_opcode: u64,
    pub(crate) exception_flags: u64,
    pub(crate) exception_masks: u64,
    pub(crate) rounding_control: u8,
    pub(crate) precision_control: u8,
    pub(crate) infinity_control: u8,
}

impl Default for State {
    fn default() -> Self {
        Self {
            regs: [0; _],
            x87: X87State::default(),
        }
    }
}

impl Display for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f)?;
        for chunk in Intel386::iter_gpregs().chunks(7).into_iter() {
            for reg in chunk {
                if reg.is_flags() {
                    continue
                }

                write!(f, "{:3} = {:08X}  ", reg.to_string(), CpuState::reg(self, Reg::Gp(reg)))?;
            }

            writeln!(f)?;
        }
        for chunk in [
            X87Reg::Mm(0),
            X87Reg::Mm(1),
            X87Reg::Mm(2),
            X87Reg::Mm(3),
            X87Reg::Mm(4),
            X87Reg::Mm(5),
            X87Reg::Mm(6),
            X87Reg::Mm(7),
            X87Reg::ExceptionFlags,
            X87Reg::ExceptionMasks,
            X87Reg::ConditionCodes,
            X87Reg::PrecisionControl,
            X87Reg::RoundingControl,
            X87Reg::Top,
            X87Reg::MmIsValid,
        ]
        .chunks(4)
        {
            for &reg in chunk {
                if reg.is_flags() {
                    continue
                }

                write!(f, "{:3} = {:08X}  ", reg.to_string(), CpuState::reg(self, Reg::X87(reg)))?;
            }

            writeln!(f)?;
        }

        for flag in Intel386Flag::iter() {
            write!(f, "{} = {}  ", flag, i32::from(CpuState::flag(self, flag)))?;
        }

        writeln!(f)?;

        Ok(())
    }
}

impl CpuState<Intel386> for State {
    #[inline]
    fn gpreg(&self, reg: GpReg) -> u64 {
        if reg.is_zero() { 0 } else { self.regs[reg.as_num()] }
    }

    #[inline]
    fn set_gpreg(&mut self, reg: GpReg, value: u64) {
        if !reg.is_zero() {
            self.regs[reg.as_num()] = value;
        }
    }

    fn reg(&self, reg: Reg) -> Value<'_> {
        match reg {
            Reg::Gp(reg) => Value::Num(self.gpreg(reg)),
            Reg::X87(X87Reg::Top) => Value::Num(self.x87.top as u64),
            Reg::X87(X87Reg::Mm(n)) => Value::Bytes(bytemuck::cast_ref::<_, [u8; 16]>(&self.x87.mm[n as usize])),
            Reg::X87(X87Reg::ConditionCodes) => Value::Num(self.x87.condition_codes as u64),
            Reg::X87(X87Reg::ExceptionMasks) => Value::Num(self.x87.exception_masks),
            Reg::X87(X87Reg::ExceptionFlags) => Value::Num(self.x87.exception_flags),
            Reg::X87(X87Reg::DataPointer) => Value::Num(self.x87.data_pointer),
            Reg::X87(X87Reg::DataSelector) => Value::Num(self.x87.data_selector),
            Reg::X87(X87Reg::InstructionPointer) => Value::Num(self.x87.instruction_pointer),
            Reg::X87(X87Reg::InstructionSelector) => Value::Num(self.x87.instruction_selector),
            Reg::X87(X87Reg::LastInstructionOpcode) => Value::Num(self.x87.last_instruction_opcode),
            Reg::X87(X87Reg::RoundingControl) => Value::Num(self.x87.rounding_control as u64),
            Reg::X87(X87Reg::PrecisionControl) => Value::Num(self.x87.precision_control as u64),
            Reg::X87(X87Reg::InfinityControl) => Value::Num(self.x87.infinity_control as u64),
            Reg::X87(X87Reg::MmIsValid) => Value::Num(self.x87.mm_is_valid),
        }
    }

    fn modify_reg<F: FnOnce(MutValue)>(&mut self, reg: Reg, update: F) {
        match reg {
            Reg::Gp(reg) => {
                let mut v = self.gpreg(reg);
                update(MutValue::Num(&mut v));

                if !reg.is_zero() {
                    self.set_gpreg(reg, v);
                }
            },
            Reg::X87(X87Reg::Top) => {
                let mut v = self.x87.top as u64;
                update(MutValue::Num(&mut v));
                self.x87.top = v as u8;
            },
            Reg::X87(X87Reg::Mm(n)) => update(MutValue::Bytes(bytemuck::cast_mut::<_, [u8; 16]>(
                &mut self.x87.mm[n as usize],
            ))),
            Reg::X87(X87Reg::ConditionCodes) => {
                let mut v = self.x87.condition_codes as u64;
                update(MutValue::Num(&mut v));
                self.x87.condition_codes = v as u32;
            },
            Reg::X87(X87Reg::ExceptionMasks) => {
                update(MutValue::Num(&mut self.x87.exception_masks));
            },
            Reg::X87(X87Reg::ExceptionFlags) => {
                update(MutValue::Num(&mut self.x87.exception_flags));
            },
            Reg::X87(X87Reg::DataPointer) => {
                let mut v = self.x87.data_pointer;
                update(MutValue::Num(&mut v));
                self.x87.data_pointer = v;
            },
            Reg::X87(X87Reg::DataSelector) => {
                let mut v = self.x87.data_selector;
                update(MutValue::Num(&mut v));
                self.x87.data_selector = v;
            },
            Reg::X87(X87Reg::InstructionPointer) => {
                let mut v = self.x87.instruction_pointer;
                update(MutValue::Num(&mut v));
                self.x87.instruction_pointer = v;
            },
            Reg::X87(X87Reg::InstructionSelector) => {
                let mut v = self.x87.instruction_selector;
                update(MutValue::Num(&mut v));
                self.x87.instruction_selector = v;
            },
            Reg::X87(X87Reg::LastInstructionOpcode) => {
                let mut v = self.x87.last_instruction_opcode;
                update(MutValue::Num(&mut v));
                self.x87.last_instruction_opcode = v;
            },
            Reg::X87(X87Reg::RoundingControl) => {
                let mut v = self.x87.rounding_control as u64;
                update(MutValue::Num(&mut v));
                self.x87.rounding_control = v as u8;
            },
            Reg::X87(X87Reg::PrecisionControl) => {
                let mut v = self.x87.precision_control as u64;
                update(MutValue::Num(&mut v));
                self.x87.precision_control = v as u8;
            },
            Reg::X87(X87Reg::InfinityControl) => {
                let mut v = self.x87.infinity_control as u64;
                update(MutValue::Num(&mut v));
                self.x87.infinity_control = v as u8;
            },
            Reg::X87(X87Reg::MmIsValid) => {
                update(MutValue::Num(&mut self.x87.mm_is_valid));
            },
        }
    }

    fn flag(&self, flag: Intel386Flag) -> bool {
        let n = flag as u32;
        // This only works on little-endian architectures
        let bytes: &[u8; 8] = bytemuck::cast_ref(&self.regs[GpReg::Flags1.as_num() + n as usize / 8]);
        bytes[n as usize & 7] != 0
    }

    fn set_flag(&mut self, flag: Intel386Flag, value: bool) {
        let n = flag as u32;
        let bytes: &mut [u8; 8] = bytemuck::cast_mut(&mut self.regs[GpReg::Flags1.as_num() + n as usize / 8]);
        bytes[n as usize & 7] = value as u8;
    }

    type DiffMask = ();
}

#[derive(
    Copy, Clone, Debug, PartialEq, Eq, Hash, Default, PartialOrd, Ord, Serialize, Deserialize, Encode, Decode, mem_dbg::MemSize,
)]
pub struct Intel386;

impl State {
    #[inline(always)]
    pub fn byte_offset_of(reg: Reg) -> usize {
        match reg {
            Reg::Gp(reg) => offset_of!(Self, regs) + reg.as_num() * size_of_val(&Self::default().regs[0]),
            Reg::X87(reg) => offset_of!(Self, x87) + X87State::byte_offset_of(reg),
        }
    }
}

impl X87State {
    #[inline(always)]
    pub fn byte_offset_of(reg: X87Reg) -> usize {
        match reg {
            X87Reg::Top => offset_of!(Self, top),
            X87Reg::ConditionCodes => offset_of!(Self, condition_codes),
            X87Reg::MmIsValid => offset_of!(Self, mm_is_valid),
            X87Reg::DataPointer => offset_of!(Self, data_pointer),
            X87Reg::DataSelector => offset_of!(Self, data_selector),
            X87Reg::InstructionPointer => offset_of!(Self, instruction_pointer),
            X87Reg::InstructionSelector => offset_of!(Self, instruction_selector),
            X87Reg::LastInstructionOpcode => offset_of!(Self, last_instruction_opcode),
            X87Reg::ExceptionFlags => offset_of!(Self, exception_flags),
            X87Reg::Mm(n) => offset_of!(Self, mm) + n as usize * size_of_val(&Self::default().mm[0]),
            X87Reg::ExceptionMasks => offset_of!(Self, exception_masks),
            X87Reg::RoundingControl => offset_of!(Self, rounding_control),
            X87Reg::PrecisionControl => offset_of!(Self, precision_control),
            X87Reg::InfinityControl => offset_of!(Self, infinity_control),
        }
    }
}

impl Arch for Intel386 {
    type CpuState = State;
    type Reg = Reg;
    type GpReg = GpReg;
    type Flag = Intel386Flag;

    const PAGE_BITS: usize = 12;
    const PC: Self::GpReg = GpReg::Ip;
    const ZERO: Self::GpReg = GpReg::Riz;
    const INSTRUCTION_ALIGNMENT: usize = 1;

    fn reg(reg: Self::GpReg) -> Self::Reg {
        Reg::Gp(reg)
    }

    fn flagreg_to_flags(reg: Reg, start_byte: usize, end_byte: usize) -> &'static [Self::Flag] {
        match reg {
            Reg::Gp(GpReg::Flags1) => &Intel386Flag::VARIANTS[..8][start_byte..=end_byte],
            Reg::Gp(GpReg::Flags2) => &Intel386Flag::VARIANTS[8..][start_byte..=end_byte],
            // TODO
            Reg::X87(X87Reg::ExceptionFlags | X87Reg::ExceptionMasks | X87Reg::ConditionCodes) => &[],
            _ => unreachable!(),
        }
    }

    #[inline]
    fn iter_gpregs() -> impl Iterator<Item = Self::GpReg> {
        GpReg::VARIANTS.iter().copied()
    }

    fn iter_regs() -> impl Iterator<Item = Self::Reg> {
        Self::iter_gpregs().map(Reg::Gp).chain(
            [
                X87Reg::Mm(0),
                X87Reg::Mm(1),
                X87Reg::Mm(2),
                X87Reg::Mm(3),
                X87Reg::Mm(4),
                X87Reg::Mm(5),
                X87Reg::Mm(6),
                X87Reg::Mm(7),
                X87Reg::ExceptionFlags,
                X87Reg::ExceptionMasks,
                X87Reg::ConditionCodes,
                X87Reg::PrecisionControl,
                X87Reg::RoundingControl,
                X87Reg::Top,
                X87Reg::MmIsValid,
            ]
            .into_iter()
            .map(Reg::X87),
        )
    }

    fn try_reg_to_gpreg(reg: <Intel386 as Arch>::Reg) -> Option<<Intel386 as Arch>::GpReg> {
        match reg {
            Reg::Gp(gp_reg) => Some(gp_reg),
            _ => None,
        }
    }
}

impl State {
    pub fn is_userspace(&self) -> bool {
        self.gpreg(GpReg::Cpl) != 0
    }
}
