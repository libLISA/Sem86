use super::Ir;
use crate::codegen::singlepass::{Emitter, Target};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Regs {
    Rax = 0,
    Rcx,
    Rdx,
    Rbx,
    Rsp,
    Rbp,
    Rsi,
    Rdi,
}

pub struct X86;

impl Target for X86 {
    type Reg = Regs;

    const RETURN_REG: Self::Reg = Regs::Rax;
    const RETURN_REG2: Self::Reg = Regs::Rdx;

    const CALLEE_SAVED_REGS: &[Self::Reg] = &[Regs::Rbx];
    const CALLER_SAVED_REGS: &[Self::Reg] = &[Regs::Rax, Regs::Rdi, Regs::Rsi, Regs::Rdx, Regs::Rcx];

    const PARAMETER_REGS: &[Self::Reg] = &[Regs::Rdi, Regs::Rsi, Regs::Rdx, Regs::Rcx];

    // x86 uses post-increment offsets
    fn relocation_offset(size: usize) -> usize {
        size
    }

    fn compile(x: &Ir<Self>, bytes: &mut Emitter) {
        match *x {
            Ir::ReadU32 {
                into,
                base,
                offset,
            } => {
                if offset == 0 {
                    bytes.emit([0x8B, 0x8b, ((into as u8) << 3) + base as u8])
                } else {
                    bytes.emit([
                        0x8B,
                        0x80 + ((into as u8) << 3) + base as u8,
                        offset as u8,
                        (offset >> 8) as u8,
                        (offset >> 16) as u8,
                        (offset >> 24) as u8,
                    ])
                }
            },
            Ir::ReadU64 {
                into,
                base,
                offset,
            } => bytes.emit([
                0x48,
                0x8B,
                0x80 + ((into as u8) << 3) + base as u8,
                offset as u8,
                (offset >> 8) as u8,
                (offset >> 16) as u8,
                (offset >> 24) as u8,
            ]),
            Ir::LoadImm {
                into,
                val,
            } => {
                if val > 0xffff_ffff {
                    // movabs reg, imm
                    bytes.emit([
                        0x48,
                        0xB8 + into as u8,
                        val as u8,
                        (val >> 8) as u8,
                        (val >> 16) as u8,
                        (val >> 24) as u8,
                        (val >> 32) as u8,
                        (val >> 40) as u8,
                        (val >> 48) as u8,
                        (val >> 56) as u8,
                    ]);
                } else if val == 0 {
                    // xor reg, reg
                    bytes.emit([0x31, 0xc0 + into as u8 + ((into as u8) << 3)]);
                } else {
                    // mov reg, imm
                    bytes.emit([
                        0xB8 + into as u8,
                        val as u8,
                        (val >> 8) as u8,
                        (val >> 16) as u8,
                        (val >> 24) as u8,
                    ]);
                }
            },
            Ir::LoadImm8 {
                into,
                val,
            } => {
                if into as u8 >= 4 {
                    bytes.emit(0x40);
                }

                bytes.emit([0xb0 + into as u8, val]);
            },
            Ir::Load {
                into,
                from,
            } => bytes.emit([0x48, 0x89, 0xc0 + into as u8 + ((from as u8) << 3)]),
            Ir::CallRipRelative {
                label,
            } => {
                // call [rip + imm]
                bytes.emit([0xff, 0x15]);
                bytes.emit(label);
            },
            Ir::Return {
                val,
            } => {
                if val != Regs::Rax {
                    Self::compile(
                        &Ir::Load {
                            into: Regs::Rax,
                            from: val,
                        },
                        bytes,
                    );
                }

                bytes.emit(0xC3);
            },
            Ir::Jump {
                to,
            } => {
                bytes.emit(0xE9);
                bytes.emit(to);
            },
            Ir::BrIfReg8False {
                val,
                to,
            } => {
                bytes.emit([
                    // test r8, r8
                    0x84,
                    0xC0 + val as u8,
                    // je rel32
                    0x0f,
                    0x84,
                ]);

                bytes.emit(to);
            },
            Ir::Push {
                val,
            } => bytes.emit(0x50 + val as u8),
            Ir::Pop {
                into,
            } => bytes.emit(0x58 + into as u8),
            Ir::BrIfMem32IsNonZeroAtomic {
                base,
                offset,
                to,
            } => {
                bytes.emit([
                    // cmp dword ptr [base + imm32], 0
                    0x83,
                    0xb8 + base as u8,
                    offset as u8,
                    (offset >> 8) as u8,
                    (offset >> 16) as u8,
                    (offset >> 24) as u8,
                    0x00,
                    // jne rel32
                    0x0f,
                    0x85,
                ]);

                bytes.emit(to);
            },
            Ir::BrIfMem8IsZero {
                base,
                offset,
                to,
            } => {
                bytes.emit([
                    // cmp byte ptr [base + imm32], 0
                    0x80,
                    0xb8 + base as u8,
                    offset as u8,
                    (offset >> 8) as u8,
                    (offset >> 16) as u8,
                    (offset >> 24) as u8,
                    0x00,
                    // je rel32
                    0x0f,
                    0x84,
                ]);

                bytes.emit(to);
            },
            Ir::AlignedDataU64 {
                val,
            } => {
                bytes.emit(val.to_le_bytes());
            },
            Ir::AlignedDataU32 {
                val,
            } => {
                bytes.emit(val.to_le_bytes());
            },
            Ir::AddU32 {
                dst,
                src,
            } => bytes.emit([
                // add reg32, reg32
                0x01,
                0xc0 + dst as u8 + ((src as u8) << 3),
            ]),
            Ir::BandU32Imm {
                reg,
                imm,
            } => bytes.emit([
                // and reg32, imm
                0x81,
                0xE0 + reg as u8,
                imm as u8,
                (imm >> 8) as u8,
                (imm >> 16) as u8,
                (imm >> 24) as u8,
            ]),
            Ir::BrIfEqImm {
                reg,
                imm,
                label,
            } => {
                bytes.emit([
                    // cmp reg, imm
                    0x81,
                    0xF8 + reg as u8,
                    imm as u8,
                    (imm >> 8) as u8,
                    (imm >> 16) as u8,
                    (imm >> 24) as u8,
                    // je rel32
                    0x0F,
                    0x84,
                ]);

                bytes.emit(label);
            },
            Ir::SelectBits {
                into,
                from,
                start,
                end,
            } => {
                assert_eq!(start, 8);
                assert_eq!(end, 16);

                assert!([Regs::Rax, Regs::Rcx, Regs::Rdx, Regs::Rbx].contains(&from));

                // movsx reg32, reg8h
                bytes.emit([0x0F, 0xB6, 0xC4 | ((into as u8) << 3) | (from as u8)])
            },
            Ir::ReadArrayPtrU32 {
                into,
                label,
                index,
            } => {
                // LEA reg, [rip + label]
                bytes.emit([0x48, 0x8D, 0x05 | ((into as u8) << 3)]);
                bytes.emit(label);

                // MOV reg32, [reg + index * 4]
                bytes.emit([0x8B, 0x04 | ((into as u8) << 3), 0x80 | ((index as u8) << 3) | (into as u8)])
            },
        }
    }
}
