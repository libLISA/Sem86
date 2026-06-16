use liblisa::encoding::dataflows::{AddrTerm, AddrTermSize};
use liblisa::encoding::{ParLoc, UnsizedParLoc};
use sem86_core::arch::intel386::{FLAG_AF, FLAG_OF, Intel386};
use sem86_core::il::{UnOp, Val};

use crate::builder::*;
use crate::context::BuildFromContext;
use crate::dsl::*;
use crate::instrs::arith::{compute_pf, compute_sf};
use crate::instrs::{FLAG_CF, FLAG_ZF};
use crate::{Config, encoding, encoding_group, ops};

#[derive(Copy, Clone, Debug)]
enum BitOp {
    None,
    Reset,
    Complement,
    Set,
}

impl TryFrom<u64> for BitOp {
    type Error = ();

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        match value {
            0b100 => Ok(BitOp::None),
            0b111 => Ok(BitOp::Complement),
            0b110 => Ok(BitOp::Reset),
            0b101 => Ok(BitOp::Set),
            _ => Err(()),
        }
    }
}

pub fn builder(_config: Config) -> impl Builder<Output = SemSpec<Intel386>> {
    [
        encoding! {
            Name { "BSF/BSR" },
            Wide { true },
            Prefixes, #0x0F,
            1, 0, 1, 1, 1, 1, 0, ExpandedBit = not_forward,
            ModRm = (reg, rm),
            ops! {
                #[context(ctx)]
                let tmp0 = (rm, if not_forward {
                    UnOp::HighestBitSet
                } else {
                    UnOp::TrailingZeros
                });
                FLAG_ZF := is_zero(rm);

                let result = ite(rm, reg, tmp0);

                // These are all undefined; We copy bochs' behavior.
                ..compute_pf(result);
                ..compute_sf(ctx.op_size() * 8, result);
                FLAG_AF := 0;
                FLAG_OF := 0;
                FLAG_CF := 0;

                reg := result;

            }
        },
        encoding_group! {
            [
                Name { "BTx_imm" },
                Wide { true },
                Prefixes, #0x0F, #0xBA,
                Mod = md, TryBitsInto::<BitOp> { 3 } = op, Rm { md } = rm,
                Imm { 8 } = bit_index,
            ] = (rm, bit_index, op),
            [
                Name { "BTx_reg" },
                Wide { true },
                Prefixes, #0x0F,
                1, 0, TryBitsInto::<BitOp> { 3 } = op, 0, 1, 1,
                ModRm = (reg, rm),
            ] = (rm, reg, op),
            map |(dest, bit_index, op)| BuildFromContext::new(move |ctx| {
                if let Val::Loc(ParLoc { loc: UnsizedParLoc::Mem(mem_index), .. }) = rm {
                    let size = ctx.op_size();
                    let m = ctx.access_mut(mem_index);
                    let bit_index_loc = match bit_index {
                        Val::Loc(loc) => loc,
                        Val::Conv { loc, source_bits, sign_extend, .. } => {
                            assert!(!sign_extend);
                            assert_eq!(source_bits, 8);

                            loc
                        },
                        _ => unreachable!(),
                    };

                    m.inputs.push(bit_index_loc);
                    m.calculation.unwrap_mut_calculaton().add_term(match size {
                        2 => AddrTerm::single(AddrTermSize::U16, 4, 2),
                        4 => AddrTerm::single(AddrTermSize::U32, 5, 4),
                        _ => unreachable!(),
                    });
                }

                ops! {
                    #[context(ctx)]
                    let bit_index = and(bit_index, ctx.op_size() as u64 * 8 - 1);
                    let bit = shl(1, bit_index);
                    let extracted_bit = and(bit, dest);
                    FLAG_CF := ite(extracted_bit, 0, 1);

                    #[match op] {
                        BitOp::None => {},
                        BitOp::Reset => {
                            let mask = xor(bit, u64::MAX);
                            dest := and(dest, mask);
                        }
                        BitOp::Complement => {
                            dest := xor(dest, bit);
                        }
                        BitOp::Set => {
                            dest := or(dest, bit);
                        }
                    }


                }
            })
        },
    ]
}
