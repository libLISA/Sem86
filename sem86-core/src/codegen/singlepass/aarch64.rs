use crate::codegen::singlepass::{Emittable, Label, LabelInU32, Target};

struct Instr(u32);

impl Emittable for Instr {
    fn emit(&self, target: &mut super::Emitter) {
        target.emit(self.0.to_le_bytes())
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Regs {
    X0 = 0,
    X1,
    X2,
    X3,
    X4,
    X5,
    X6,
    X7,
    X8,
    X9,
    X10,
    X11,
    X12,
    X13,
    X14,
    X15,
    X16,
    X17,
    X18,
    X19,
    X20,
    X21,
    X22,
    X23,
    X24,
    X25,
    X26,
    X27,
    X28,
}

pub struct AArch64;

// LDR Wt, [Xn, #imm12]
fn ldr_w(into: u8, base: u8, offset: u32) -> Instr {
    Instr(0b1011100101 << 22 | (((offset >> 2) & 0xfff) << 10) | ((base as u32) << 5) | into as u32)
}

// LDR Xt, [Xn, #imm12]
fn ldr_x(into: u8, base: u8, offset: u32) -> Instr {
    Instr(0b1111100101 << 22 | (((offset >> 3) & 0xfff) << 10) | ((base as u32) << 5) | into as u32)
}

fn movz(rd: u8, imm16: u16, shift: u8) -> Instr {
    Instr(0b110100101 << 23 | ((shift as u32 / 16) << 21) | ((imm16 as u32) << 5) | rd as u32)
}

// MOVZ Wd, #imm
fn movz_w(rd: u8, imm: u16) -> Instr {
    Instr(0b010100101 << 23 | ((imm as u32) << 5) | rd as u32)
}

fn movk(rd: u8, imm16: u16, shift: u8) -> Instr {
    Instr(0b111100101 << 23 | ((shift as u32 / 16) << 21) | ((imm16 as u32) << 5) | rd as u32)
}

// ORR Xd, XZR, Xn
fn mov_reg(dst: u8, src: u8) -> Instr {
    Instr(0b10101010000 << 21 | ((src as u32) << 16) | (31 << 5) | dst as u32)
}

// ADD Wd, Wn, Wm
fn add_w(dst: u8, src: u8) -> Instr {
    Instr(0b00001011000 << 21 | ((src as u32) << 16) | ((dst as u32) << 5) | dst as u32)
}

// ADD Xd, Xn, #imm
fn add_x_imm(dst: u8, src: u8, imm12: u16, shift: bool) -> Instr {
    let shift_bit = if shift { 1 } else { 0 };
    Instr(0x91000000 | (shift_bit << 22) | ((imm12 as u32) << 10) | ((src as u32) << 5) | dst as u32)
}

// AND Wd, Wn, #imm
fn and_imm_w(rd: u8, rn: u8, imm: u32) -> Instr {
    // assume imm encodable (your IR already assumes this)
    Instr(0b000100100 << 23 | ((imm & 0xfff) << 10) | ((rn as u32) << 5) | rd as u32)
}

// CMP Wn, #imm
fn cmp_w_imm(rn: u8, imm: u32) -> Instr {
    assert!(imm <= 0xfff);
    Instr(0b011100010 << 23 | ((imm & 0xfff) << 10) | ((rn as u32) << 5) | 31)
}

fn cmp_w(rn: Regs, rm: Regs) -> Instr {
    // There's an optional imm3 at 11..14
    Instr(0xeb00001f | ((rm as u32) << 16) | ((rn as u32) << 5) | 31)
}

// B.EQ label
fn b_eq(label: Label) -> LabelInU32 {
    LabelInU32::new(label, 0b01010100 << 24, 19, 5, 2)
}

// B rel26
fn b(label: Label) -> LabelInU32 {
    LabelInU32::new(label, 0b000101 << 26, 26, 0, 2)
}

// CBZ Xt, rel19
fn cbz(rt: u8, label: Label) -> LabelInU32 {
    LabelInU32::new(label, (0b10110100 << 24) | rt as u32, 19, 5, 2)
}

// STP Xt, Xt2, [SP, #-16]!
fn push2_x(rt: u8, rt2: u8) -> Instr {
    // Move SP by -16 (2 * 8) to ensure stack alignment is correct for calls
    let imm7 = 0b1111110;
    Instr(
        (0b1010100110 << 22)
        | (imm7 << 15)       // imm9 field
        | ((rt2 as u32) << 10)
        | (31 << 5)          // SP
        | rt as u32,
    )
}

// LDP Xt, Xt2, [SP], #16
fn pop2_x(rt: u8, rt2: u8) -> Instr {
    // Move SP by 16 (2 * 8) to ensure stack alignment is correct for calls
    let imm7 = 2;
    Instr(
        (0b1010100011 << 22)  // opcode for LDR immediate, post-index
        | (imm7 << 15)            // imm9 field
        | ((rt2 as u32) << 10)
        | (31 << 5)               // SP
        | rt as u32,
    )
}

// LDAR Wt, [Xn]
fn ldar_w(rt: u8, rn: u8) -> Instr {
    Instr(0x88dffc00 | ((rn as u32) << 5) | rt as u32)
}

// CBNZ Wt, rel19
fn cbnz_w(rt: u8, label: Label) -> LabelInU32 {
    LabelInU32::new(label, (0b00110101 << 24) | rt as u32, 19, 5, 2)
}

// LDR Xt, #imm19  (literal)
fn ldr_literal(rt: u8, label: Label) -> LabelInU32 {
    LabelInU32::new(label, (0b01011000 << 24) | rt as u32, 19, 5, 2)
}

/// Generate a BLR instruction to branch to the register `rt`.
fn blr(rt: u8) -> Instr {
    Instr(0xd63f0000 | ((rt as u32) << 5))
}

impl Target for AArch64 {
    type Reg = Regs;

    const RETURN_REG: Self::Reg = Regs::X0;
    const RETURN_REG2: Self::Reg = Regs::X1;

    const PARAMETER_REGS: &[Self::Reg] = &[Regs::X0, Regs::X1, Regs::X2, Regs::X3];

    const CALLER_SAVED_REGS: &[Self::Reg] = &[
        Regs::X0,
        Regs::X1,
        Regs::X2,
        Regs::X3,
        Regs::X4,
        Regs::X5,
        Regs::X6,
        Regs::X7,
        Regs::X9,
        Regs::X10,
        Regs::X11,
        Regs::X12,
        Regs::X13,
        Regs::X14,
        Regs::X15,
        // X16 is used by `compile`
        Regs::X17,
    ];

    const CALLEE_SAVED_REGS: &[Self::Reg] = &[
        Regs::X19,
        Regs::X20,
        Regs::X21,
        Regs::X22,
        Regs::X23,
        Regs::X24,
        Regs::X25,
        Regs::X26,
        Regs::X27,
    ];

    // AArch64 uses pre-increment offsets
    fn relocation_offset(_: usize) -> usize {
        0
    }

    fn compile(x: &super::Ir<Self>, e: &mut super::Emitter) {
        match *x {
            super::Ir::ReadU32 {
                into,
                base,
                offset,
            } => e.emit(ldr_w(into as u8, base as u8, offset)),
            super::Ir::ReadU64 {
                into,
                base,
                offset,
            } => e.emit(ldr_x(into as u8, base as u8, offset)),
            super::Ir::LoadImm {
                into,
                val,
            } => {
                let r = into as u8;
                e.emit(movz(r, (val & 0xffff) as u16, 0));
                if val >> 16 != 0 {
                    e.emit(movk(r, ((val >> 16) & 0xffff) as u16, 16));

                    if val >> 32 != 0 {
                        e.emit(movk(r, ((val >> 32) & 0xffff) as u16, 32));

                        if val >> 48 != 0 {
                            e.emit(movk(r, ((val >> 48) & 0xffff) as u16, 48));
                        }
                    }
                }
            },
            super::Ir::LoadImm8 {
                into,
                val,
            } => e.emit(movz_w(into as u8, val as u16)),
            super::Ir::Load {
                into,
                from,
            } => e.emit(mov_reg(into as u8, from as u8)),
            super::Ir::CallRipRelative {
                label,
            } => {
                // LDR X16, [PC + imm]
                e.emit(ldr_literal(Regs::X16 as u8, label));

                // BLR X16
                e.emit(blr(Regs::X16 as u8));
            },
            super::Ir::Return {
                val,
            } => {
                if val != Regs::X0 {
                    e.emit(mov_reg(0, val as u8));
                }

                e.emit(Instr(0xd65f03c0)); // RET
            },
            super::Ir::Jump {
                to,
            } => {
                e.emit(b(to));
            },
            super::Ir::BrIfReg8False {
                val,
                to,
            } => {
                e.emit(cbz(val as u8, to));
            },
            // TODO: Push and Pop are here overloaded as "function entry" and "function exit", we should introduce a proper abstraction for this.
            super::Ir::Push {
                val,
            } => e.emit(push2_x(val as u8, 30)),
            super::Ir::Pop {
                into,
            } => e.emit(pop2_x(into as u8, 30)),
            super::Ir::BrIfMem32IsNonZeroAtomic {
                base,
                offset,
                to,
            } => {
                assert!(offset <= 0xfff);
                e.emit(add_x_imm(Regs::X16 as u8, base as u8, offset as u16, false));
                e.emit(ldar_w(Regs::X16 as u8, Regs::X16 as u8));
                e.emit(cbnz_w(Regs::X16 as u8, to));
            },
            super::Ir::AlignedDataU64 {
                val,
            } => e.emit(val.to_le_bytes()),
            super::Ir::AddU32 {
                dst,
                src,
            } => e.emit(add_w(dst as u8, src as u8)),
            super::Ir::BandU32Imm {
                reg,
                imm,
            } => e.emit(and_imm_w(reg as u8, reg as u8, imm)),
            super::Ir::BrIfEqImm {
                reg,
                imm,
                label,
            } => {
                if imm <= 0xfff {
                    e.emit(cmp_w_imm(reg as u8, imm));
                } else {
                    Self::compile(
                        &super::Ir::LoadImm {
                            into: Regs::X16,
                            val: imm as u64,
                        },
                        e,
                    );
                    e.emit(cmp_w(reg, Regs::X16));
                }
                e.emit(b_eq(label));
            },
            _ => todo!(),
        }
    }
}
