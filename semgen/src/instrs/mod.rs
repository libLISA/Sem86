pub mod arith;
pub mod bitwise;
pub mod bmi;
pub mod flags;
pub mod flow;
pub mod fpu;
pub mod io;
pub mod mmu;
pub mod mov;
pub mod other;
pub mod stack;
pub mod string;

use liblisa::encoding::UnsizedParLoc;
use liblisa::encoding::bitpattern::PartMapping;
use liblisa::encoding::dataflows::MemoryAccess;
use liblisa::state::Size;
use liblisa::utils::bitmask_u64;
use sem86_arch::exceptions::Exception;
use sem86_core::arch::intel386::Intel386;
use sem86_core::il::{BinOp, Cmd, Op, Val};

use crate::Config;
use crate::builder::{Builder, SemSpec};
use crate::context::Context;
use crate::dsl::LoadIntoVal;

pub const QWORD: Size = Size::new(0, 7);
pub const DWORD: Size = Size::new(0, 3);
pub const WORD: Size = Size::new(0, 1);
pub const LOW_BYTE: Size = Size::new(0, 0);
pub const HIGH_BYTE: Size = Size::new(1, 1);

pub use sem86_core::arch::intel386::{
    FLAG_AC, FLAG_AF, FLAG_CF, FLAG_DF, FLAG_ID, FLAG_IF, FLAG_NT, FLAG_OF, FLAG_PF, FLAG_RF, FLAG_SF, FLAG_TF, FLAG_VM, FLAG_ZF,
};

struct EffectiveAddress<'a>(&'a MemoryAccess<Intel386>);

impl LoadIntoVal<Intel386> for EffectiveAddress<'_> {
    fn load_into(self, ctx: &mut Context, target: Val<Intel386>, ops: &mut Vec<Cmd<Intel386>>) {
        let sum = ctx.fresh_temp_var();
        let term_val = ctx.fresh_temp_var();
        let addr = self.0;
        ops.push(Cmd::mov(sum, Val::const_val(0)));
        for (input, term) in addr
            .inputs
            .iter()
            .zip(addr.calculation.unwrap_calculation().terms.iter())
            .skip(1)
        {
            ops.extend([
                Cmd::store(
                    term_val,
                    Op::BinOp {
                        args: [
                            if let UnsizedParLoc::Part(part) = input.loc {
                                let part = &ctx.parts()[part];
                                if matches!(part.mapping, PartMapping::Imm { .. }) {
                                    Val::Conv {
                                        loc: *input,
                                        source_bits: part.size.try_into().unwrap(),
                                        target_bits: (ctx.addr_size() * 8).try_into().unwrap(),
                                        sign_extend: true,
                                        swap_endianness: true,
                                    }
                                } else {
                                    Val::Loc(*input)
                                }
                            } else {
                                Val::Loc(*input)
                            },
                            Val::const_val(term.primary.shift.mult() as u64),
                        ],
                        op: BinOp::Mul,
                    },
                ),
                Cmd::store(
                    sum,
                    Op::BinOp {
                        args: [sum, term_val],
                        op: BinOp::Add,
                    },
                ),
            ])
        }

        ops.extend([Cmd::store(
            target,
            Op::BinOp {
                args: [sum, Val::const_val(bitmask_u64(ctx.addr_size() as u32 * 8))],
                op: BinOp::And,
            },
        )]);
    }
}

pub fn invoke_gp() -> Cmd<Intel386> {
    Cmd::Exception {
        exception: Exception::GeneralProtectionFault(0),
        code: Val::const_val(0),
    }
}

fn all_unprefixed(config: Config) -> impl Builder<Output = SemSpec<Intel386>> {
    [
        Box::new(arith::builder(config)) as Box<dyn Builder<Output = SemSpec<Intel386>>>,
        Box::new(bitwise::builder(config)) as Box<dyn Builder<Output = SemSpec<Intel386>>>,
        Box::new(bmi::builder(config)) as Box<dyn Builder<Output = SemSpec<Intel386>>>,
        Box::new(flags::builder(config)) as Box<dyn Builder<Output = SemSpec<Intel386>>>,
        Box::new(flow::builder(config)) as Box<dyn Builder<Output = SemSpec<Intel386>>>,
        Box::new(mmu::builder(config)) as Box<dyn Builder<Output = SemSpec<Intel386>>>,
        Box::new(mov::builder(config)) as Box<dyn Builder<Output = SemSpec<Intel386>>>,
        Box::new(other::builder(config)) as Box<dyn Builder<Output = SemSpec<Intel386>>>,
        Box::new(stack::builder(config)) as Box<dyn Builder<Output = SemSpec<Intel386>>>,
        Box::new(string::builder(config)) as Box<dyn Builder<Output = SemSpec<Intel386>>>,
        Box::new(io::builder(config)) as Box<dyn Builder<Output = SemSpec<Intel386>>>,
        Box::new(fpu::builder(config)) as Box<dyn Builder<Output = SemSpec<Intel386>>>,
    ]
}

pub fn all(config: Config) -> impl Builder<Output = SemSpec<Intel386>> {
    [Box::new(all_unprefixed(config)) as Box<dyn Builder<Output = SemSpec<Intel386>>>]
}
