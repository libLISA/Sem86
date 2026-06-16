use liblisa::arch::Arch;
use liblisa::encoding::{ParLoc, UnsizedParLoc};
use liblisa::state::Size;
use sem86_arch::exceptions::Exception;
use sem86_core::arch::intel386::{HandlerId, Intel386};
use sem86_core::il::{BinOp, Cmd, FpBinOp, Op, UnOp, Val};

use crate::context::Context;
use crate::ops;

pub struct PerformMemoryReads<R>(pub R);

impl From<&mut Context> for PerformMemoryReads<Vec<ParLoc<Intel386>>> {
    fn from(ctx: &mut Context) -> Self {
        Self(
            (0..ctx.num_accesses())
                .map(|index| ParLoc {
                    loc: UnsizedParLoc::Mem(index),
                    size: Size::qword(),
                })
                .collect::<Vec<_>>(),
        )
    }
}

impl<A: Arch, R: IntoIterator<Item = ParLoc<A>>> AppendToOpVec<A> for PerformMemoryReads<R> {
    fn append_to_op_vec(self, _ctx: &mut Context, vec: &mut Vec<Cmd<A>>) {
        for v in self.0.into_iter() {
            let ParLoc {
                loc: UnsizedParLoc::Mem(index),
                ..
            } = v
            else {
                panic!("Can only pass memory accessos to PerformMemoryReads")
            };
            vec.push(Cmd::ReadMemory {
                index,
            });
        }
    }
}

pub struct PerformMemoryWrites<W>(pub W);

impl From<&mut Context> for PerformMemoryWrites<Vec<ParLoc<Intel386>>> {
    fn from(ctx: &mut Context) -> Self {
        Self(
            (0..ctx.num_accesses())
                .map(|index| ParLoc {
                    loc: UnsizedParLoc::Mem(index),
                    size: Size::qword(),
                })
                .collect::<Vec<_>>(),
        )
    }
}

impl<A: Arch, W: IntoIterator<Item = ParLoc<A>>> AppendToOpVec<A> for PerformMemoryWrites<W> {
    fn append_to_op_vec(self, _ctx: &mut Context, vec: &mut Vec<Cmd<A>>) {
        for v in self.0.into_iter() {
            let ParLoc {
                loc: UnsizedParLoc::Mem(index),
                ..
            } = v
            else {
                panic!("Can only pass memory accessos to PerformMemoryWrites")
            };
            vec.push(Cmd::WriteMemory {
                index,
            });
        }
    }
}

pub trait AppendToOpVec<A: Arch> {
    fn append_to_op_vec(self, _ctx: &mut Context, vec: &mut Vec<Cmd<A>>);
}

impl<A: Arch> AppendToOpVec<A> for Cmd<A> {
    fn append_to_op_vec(self, _ctx: &mut Context, vec: &mut Vec<Cmd<A>>) {
        vec.push(self);
    }
}

impl<A: Arch> AppendToOpVec<A> for Vec<Cmd<A>> {
    fn append_to_op_vec(self, _ctx: &mut Context, vec: &mut Vec<Cmd<A>>) {
        vec.extend(self);
    }
}

impl<const N: usize, A: Arch> AppendToOpVec<A> for [Cmd<A>; N] {
    fn append_to_op_vec(self, _ctx: &mut Context, vec: &mut Vec<Cmd<A>>) {
        vec.extend(self);
    }
}

impl<A: Arch> AppendToOpVec<A> for HandlerId {
    fn append_to_op_vec(self, _ctx: &mut Context, vec: &mut Vec<Cmd<A>>) {
        vec.push(Cmd::Handler {
            id: self,
            args: Vec::new(),
        });
    }
}

impl<A: Arch, C: Into<Val<A>>> AppendToOpVec<A> for (Exception, C) {
    fn append_to_op_vec(self, _ctx: &mut Context, vec: &mut Vec<Cmd<A>>) {
        vec.push(Cmd::Exception {
            exception: self.0,
            code: self.1.into(),
        });
    }
}

pub trait LoadIntoVal<A: Arch> {
    fn load_into(self, ctx: &mut Context, target: Val<A>, output: &mut Vec<Cmd<A>>);
}

impl<A: Arch, V: Into<Op<A>>> LoadIntoVal<A> for V {
    fn load_into(self, _ctx: &mut Context, target: Val<A>, output: &mut Vec<Cmd<A>>) {
        output.push(Cmd::Store {
            to: target,
            op: self.into(),
        });
    }
}

pub trait StoreInto<A: Arch> {
    fn store_into(self, ctx: &mut Context, val: impl LoadIntoVal<A>, output: &mut Vec<Cmd<A>>);
}

impl<A: Arch, V: Into<Val<A>>> StoreInto<A> for V {
    fn store_into(self, ctx: &mut Context, val: impl LoadIntoVal<A>, output: &mut Vec<Cmd<A>>) {
        val.load_into(ctx, self.into(), output);
    }
}

pub struct U128(pub u128);

impl LoadIntoVal<Intel386> for U128 {
    fn load_into(self, ctx: &mut Context, target: Val<Intel386>, output: &mut Vec<Cmd<Intel386>>) {
        output.extend(ops! {
            #[context(ctx)]

            let hi = shl((self.0 >> 64) as u64, 64);
            target := or(hi, self.0 as u64);
        });
    }
}

#[macro_export]
macro_rules! op {
    (#{ op: BinOp:: $fn:ident ($lhs:expr, $rhs:expr) }) => {
        Op::BinOp {
            op: BinOp::$fn,
            args: [
                Val::from($lhs),
                Val::from($rhs),
            ],
        }
    };
    ($to:expr => $($op:tt)*) => {
        Cmd::Store {
            to: Val::from($to),
            op: $crate::op!(#{ op: $($op)* })
        }
    };
}

#[macro_export]
macro_rules! ops_internal {
    // .. := expr;
    (#[cmds($vec:ident, $ctx:ident)]{ ($to:expr) := $val:expr; $($rest:tt)* }) => {
        $crate::dsl::StoreInto::store_into($to, $ctx, $val, &mut $vec);
        $crate::ops_internal!(#[cmds($vec, $ctx)]{ $($rest)* })
    };
    (#[cmds($vec:ident, $ctx:ident)]{ $($to:ident)::* $(($($to_arg:expr),*))? := $val:expr; $($rest:tt)* }) => {
        $crate::dsl::StoreInto::store_into($($to)::* $(($($to_arg),*))?, $ctx, $val, &mut $vec);
        $crate::ops_internal!(#[cmds($vec, $ctx)]{ $($rest)* })
    };

    // .. += expr;
    (#[cmds($vec:ident, $ctx:ident)]{ ($to:expr) += $val:expr; $($rest:tt)* }) => {
        $vec.push(Cmd::Store {
            to: Val::from($to),
            op: Op::BinOp {
                op: BinOp::Add,
                args: [
                    Val::from($to),
                    Val::from($val),
                ]
            },
        });

        $crate::ops_internal!(#[cmds($vec, $ctx)]{ $($rest)* })
    };
    (#[cmds($vec:ident, $ctx:ident)]{ $($to:ident)::* $(($($to_arg:expr),*))? += $val:expr; $($rest:tt)* }) => {
        $vec.push(Cmd::Store {
            to: Val::from($($to)::* $(($($to_arg),*))?),
            op: Op::BinOp {
                op: BinOp::Add,
                args: [
                    Val::from($($to)::* $(($($to_arg),*))?),
                    Val::from($val),
                ]
            },
        });

        $crate::ops_internal!(#[cmds($vec, $ctx)]{ $($rest)* })
    };

    // .. -= expr;
    (#[cmds($vec:ident, $ctx:ident)]{ ($to:expr) -= $val:expr; $($rest:tt)* }) => {
        {
            let __to = Val::from($to);
            $vec.push(Cmd::Store {
                to: __to,
                op: Op::BinOp {
                    op: BinOp::Sub,
                    args: [
                        __to,
                        Val::from($val),
                    ]
                },
            });
        }

        $crate::ops_internal!(#[cmds($vec, $ctx)]{ $($rest)* })
    };
    (#[cmds($vec:ident, $ctx:ident)]{ $($to:ident)::* $(($($to_arg:expr),*))? -= $val:expr; $($rest:tt)* }) => {
        {
            let __to = Val::from($($to)::* $(($($to_arg),*))?);
            $vec.push(Cmd::Store {
                to: __to,
                op: Op::BinOp {
                    op: BinOp::Sub,
                    args: [
                        __to,
                        Val::from($val),
                    ]
                },
            });
        }

        $crate::ops_internal!(#[cmds($vec, $ctx)]{ $($rest)* })
    };

    // ..expr;
    (#[cmds($vec:ident, $ctx:ident)]{ ..$val:expr; $($rest:tt)* }) => {
        $crate::dsl::AppendToOpVec::append_to_op_vec($val, $ctx, &mut $vec);
        $crate::ops_internal!(#[cmds($vec, $ctx)]{ $($rest)* })
    };

    // if is_zero(val) { .. } else { .. }
    (#[cmds($vec:ident, $ctx:ident)]{ if is_zero($val:expr) { $($then_body:tt)* } else { $($else_body:tt)* } $($rest:tt)* }) => {
        $vec.push(sem86_core::il::Cmd::If {
            val: sem86_core::il::Val::from($val),
            if_zero: {
                let mut v = Vec::new();
                $crate::ops_internal!(#[cmds(v, $ctx)]{ $($then_body)* });
                sem86_core::il::Commands::Ops(v)
            },
            if_nonzero: {
                let mut v = Vec::new();
                $crate::ops_internal!(#[cmds(v, $ctx)]{ $($else_body)* });
                sem86_core::il::Commands::Ops(v)
            },
        });

        $crate::ops_internal!(#[cmds($vec, $ctx)]{ $($rest)* })
    };

    // if is_zero(val) { .. }
    (#[cmds($vec:ident, $ctx:ident)]{ if is_zero($val:expr) { $($then_body:tt)* } $($rest:tt)* }) => {
        $vec.push(Cmd::If {
            val: Val::from($val),
            if_zero: {
                let mut v = Vec::new();
                $crate::ops_internal!(#[cmds(v, $ctx)]{ $($then_body)* });
                sem86_core::il::Commands::Ops(v)
            },
            if_nonzero: sem86_core::il::Commands::Ops(Vec::new()),
        });

        $crate::ops_internal!(#[cmds($vec, $ctx)]{ $($rest)* })
    };

    // if !is_zero(val) { .. } else { .. }
    (#[cmds($vec:ident, $ctx:ident)]{ if !is_zero($val:expr) { $($then_body:tt)* } else { $($else_body:tt)* } $($rest:tt)* }) => {
        $vec.push(sem86_core::il::Cmd::If {
            val: sem86_core::il::Val::from($val),
            if_zero: {
                let mut v = Vec::new();
                $crate::ops_internal!(#[cmds(v, $ctx)]{ $($else_body)* });
                sem86_core::il::Commands::Ops(v)
            },
            if_nonzero: {
                let mut v = Vec::new();
                $crate::ops_internal!(#[cmds(v, $ctx)]{ $($then_body)* });
                sem86_core::il::Commands::Ops(v)
            },
        });

        $crate::ops_internal!(#[cmds($vec, $ctx)]{ $($rest)* })
    };

    // if !is_zero(val) { .. }
    (#[cmds($vec:ident, $ctx:ident)]{ if !is_zero($val:expr) { $($then_body:tt)* } $($rest:tt)* }) => {
        $vec.push(sem86_core::il::Cmd::If {
            val: sem86_core::il::Val::from($val),
            if_zero: sem86_core::il::Commands::Ops(Vec::new()),
            if_nonzero: {
                let mut v = Vec::new();
                $crate::ops_internal!(#[cmds(v, $ctx)]{ $($then_body)* });
                sem86_core::il::Commands::Ops(v)
            },
        });

        $crate::ops_internal!(#[cmds($vec, $ctx)]{ $($rest)* })
    };

    // const tmp_name = expr;
    (#[cmds($vec:ident, $ctx:ident)]{ const $ident:ident = $val:expr; $($rest:tt)* }) => {
        let $ident = $val;
        $crate::ops_internal!(#[cmds($vec, $ctx)]{ $($rest)* })
    };

    // let tmp_name = expr;
    (#[cmds($vec:ident, $ctx:ident)]{ let $ident:ident = $val:expr; $($rest:tt)* }) => {
        let $ident = {
            let __tmp = $ctx.fresh_temp_var();
            $crate::dsl::LoadIntoVal::load_into($val, $ctx, __tmp, &mut $vec);

            __tmp
        };

        $crate::ops_internal!(#[cmds($vec, $ctx)]{ $($rest)* })
    };

    // let tmp_name;
    (#[cmds($vec:ident, $ctx:ident)]{ let $ident:ident; $($rest:tt)* }) => {
        let $ident = $ctx.fresh_temp_var();
        $crate::ops_internal!(#[cmds($vec, $ctx)]{ $($rest)* })
    };

    // #[if condition] { .. } else { .. }
    (#[cmds($vec:ident, $ctx:ident)]{ #[if $cond:expr] { $($then_body:tt)* } else { $($else_body:tt)* }  $($rest:tt)* }) => {
        if $cond {
            $crate::ops_internal!(#[cmds($vec, $ctx)]{ $($then_body)* });
        } else {
            $crate::ops_internal!(#[cmds($vec, $ctx)]{ $($else_body)* });
        }

        $crate::ops_internal!(#[cmds($vec, $ctx)]{ $($rest)* })
    };

    // #[if condition] { .. }
    (#[cmds($vec:ident, $ctx:ident)]{ #[if $cond:expr] { $($body:tt)* } $($rest:tt)* }) => {
        if $cond {
            $crate::ops_internal!(#[cmds($vec, $ctx)]{ $($body)* });
        }

        $crate::ops_internal!(#[cmds($vec, $ctx)]{ $($rest)* })
    };

    // #[match item] { .. }
    (#[cmds($vec:ident, $ctx:ident)]{ #[match $item:expr] { $($pat:pat => { $($body:tt)* }$(,)?)* } $($rest:tt)* }) => {
        match $item {
            $(
                $pat => {
                    $crate::ops_internal!(#[cmds($vec, $ctx)]{ $($body)* });
                }
            )*
        }

        $crate::ops_internal!(#[cmds($vec, $ctx)]{ $($rest)* })
    };
    (#[cmds($vec:ident)]{ #[match $item:expr] { $($pat:pat => { $($body:tt)* }$(,)?)* } $($rest:tt)* }) => {
        match $item {
            $(
                $pat => {
                    $crate::ops_internal!(#[cmds($vec)]{ $($body)* });
                }
            )*
        }

        $crate::ops_internal!(#[cmds($vec)]{ $($rest)* })
    };

    // empty tail
    (#[cmds($vec:ident, $ctx:ident)]{ }) => {};
}

pub struct MissingContext;

#[macro_export]
macro_rules! ops {
    (#[context($ctx:ident)] $($tt:tt)*) => {{
        let mut v = Vec::new();
        $crate::ops_internal!(#[cmds(v, $ctx)]{ $($tt)* });
        v
    }};
    ($($tt:tt)*) => {{
        let mut v = Vec::new();
        let mut _ctx = $crate::dsl::MissingContext;
        $crate::ops_internal!(#[cmds(v, _ctx)]{ $($tt)* });
        v
    }};
}

// BinOps
pub fn add<A: Arch>(lhs: impl Into<Val<A>>, rhs: impl Into<Val<A>>) -> Op<A> {
    Op::BinOp {
        args: [lhs.into(), rhs.into()],
        op: BinOp::Add,
    }
}

pub fn sub<A: Arch>(lhs: impl Into<Val<A>>, rhs: impl Into<Val<A>>) -> Op<A> {
    Op::BinOp {
        args: [lhs.into(), rhs.into()],
        op: BinOp::Sub,
    }
}

pub fn mul<A: Arch>(lhs: impl Into<Val<A>>, rhs: impl Into<Val<A>>) -> Op<A> {
    Op::BinOp {
        args: [lhs.into(), rhs.into()],
        op: BinOp::Mul,
    }
}

pub fn xor<A: Arch>(lhs: impl Into<Val<A>>, rhs: impl Into<Val<A>>) -> Op<A> {
    Op::BinOp {
        args: [lhs.into(), rhs.into()],
        op: BinOp::Xor,
    }
}

pub fn or<A: Arch>(lhs: impl Into<Val<A>>, rhs: impl Into<Val<A>>) -> Op<A> {
    Op::BinOp {
        args: [lhs.into(), rhs.into()],
        op: BinOp::Or,
    }
}

pub fn and<A: Arch>(lhs: impl Into<Val<A>>, rhs: impl Into<Val<A>>) -> Op<A> {
    Op::BinOp {
        args: [lhs.into(), rhs.into()],
        op: BinOp::And,
    }
}

pub fn shl<A: Arch>(lhs: impl Into<Val<A>>, rhs: impl Into<Val<A>>) -> Op<A> {
    Op::BinOp {
        args: [lhs.into(), rhs.into()],
        op: BinOp::Shl,
    }
}

pub fn shr<A: Arch>(lhs: impl Into<Val<A>>, rhs: impl Into<Val<A>>) -> Op<A> {
    Op::BinOp {
        args: [lhs.into(), rhs.into()],
        op: BinOp::Shr,
    }
}

pub fn rol<A: Arch>(lhs: impl Into<Val<A>>, rhs: impl Into<Val<A>>, bit_size: u8) -> Op<A> {
    Op::BinOp {
        args: [lhs.into(), rhs.into()],
        op: BinOp::Rol(bit_size),
    }
}

pub fn ror<A: Arch>(lhs: impl Into<Val<A>>, rhs: impl Into<Val<A>>, bit_size: u8) -> Op<A> {
    Op::BinOp {
        args: [lhs.into(), rhs.into()],
        op: BinOp::Ror(bit_size),
    }
}

pub fn sar<A: Arch>(lhs: impl Into<Val<A>>, rhs: impl Into<Val<A>>, bit_size: u8) -> Op<A> {
    Op::BinOp {
        args: [lhs.into(), rhs.into()],
        op: BinOp::Sar(bit_size),
    }
}

pub fn div<A: Arch>(lhs: impl Into<Val<A>>, rhs: impl Into<Val<A>>) -> Op<A> {
    Op::BinOp {
        args: [lhs.into(), rhs.into()],
        op: BinOp::Div,
    }
}

pub fn modulo<A: Arch>(lhs: impl Into<Val<A>>, rhs: impl Into<Val<A>>) -> Op<A> {
    Op::BinOp {
        args: [lhs.into(), rhs.into()],
        op: BinOp::Mod,
    }
}

pub fn signedmod64<A: Arch>(lhs: impl Into<Val<A>>, rhs: impl Into<Val<A>>) -> Op<A> {
    Op::BinOp {
        args: [lhs.into(), rhs.into()],
        op: BinOp::SignedMod64,
    }
}

pub fn signeddiv64<A: Arch>(lhs: impl Into<Val<A>>, rhs: impl Into<Val<A>>) -> Op<A> {
    Op::BinOp {
        args: [lhs.into(), rhs.into()],
        op: BinOp::SignedDiv64,
    }
}

pub fn cmp_gt<A: Arch>(lhs: impl Into<Val<A>>, rhs: impl Into<Val<A>>) -> Op<A> {
    Op::BinOp {
        args: [lhs.into(), rhs.into()],
        op: BinOp::CmpGt,
    }
}

pub fn cmp_lt<A: Arch>(lhs: impl Into<Val<A>>, rhs: impl Into<Val<A>>) -> Op<A> {
    Op::BinOp {
        args: [lhs.into(), rhs.into()],
        op: BinOp::CmpLt,
    }
}

pub fn cmp_eq<A: Arch>(lhs: impl Into<Val<A>>, rhs: impl Into<Val<A>>) -> Op<A> {
    Op::BinOp {
        args: [lhs.into(), rhs.into()],
        op: BinOp::CmpEq,
    }
}

pub fn f80_add<A: Arch>(lhs: impl Into<Val<A>>, rhs: impl Into<Val<A>>, rc: impl Into<Val<A>>) -> Op<A> {
    Op::FpBinOp {
        args: [lhs.into(), rhs.into()],
        rc: rc.into(),
        op: FpBinOp::F80Add,
    }
}

pub fn f80_sub<A: Arch>(lhs: impl Into<Val<A>>, rhs: impl Into<Val<A>>, rc: impl Into<Val<A>>) -> Op<A> {
    Op::FpBinOp {
        args: [lhs.into(), rhs.into()],
        rc: rc.into(),
        op: FpBinOp::F80Sub,
    }
}

pub fn f80_mul<A: Arch>(lhs: impl Into<Val<A>>, rhs: impl Into<Val<A>>, rc: impl Into<Val<A>>) -> Op<A> {
    Op::FpBinOp {
        args: [lhs.into(), rhs.into()],
        rc: rc.into(),
        op: FpBinOp::F80Mul,
    }
}

pub fn f80_div<A: Arch>(lhs: impl Into<Val<A>>, rhs: impl Into<Val<A>>, rc: impl Into<Val<A>>) -> Op<A> {
    Op::FpBinOp {
        args: [lhs.into(), rhs.into()],
        rc: rc.into(),
        op: FpBinOp::F80Div,
    }
}

pub fn f80_rem<A: Arch>(lhs: impl Into<Val<A>>, rhs: impl Into<Val<A>>, rc: impl Into<Val<A>>) -> Op<A> {
    Op::FpBinOp {
        args: [lhs.into(), rhs.into()],
        rc: rc.into(),
        op: FpBinOp::F80Rem,
    }
}

pub fn f80_cmp_lt<A: Arch>(lhs: impl Into<Val<A>>, rhs: impl Into<Val<A>>, rc: impl Into<Val<A>>) -> Op<A> {
    Op::FpBinOp {
        args: [lhs.into(), rhs.into()],
        rc: rc.into(),
        op: FpBinOp::F80CmpLt,
    }
}

pub fn f80_cmp_eq<A: Arch>(lhs: impl Into<Val<A>>, rhs: impl Into<Val<A>>, rc: impl Into<Val<A>>) -> Op<A> {
    Op::FpBinOp {
        args: [lhs.into(), rhs.into()],
        rc: rc.into(),
        op: FpBinOp::F80CmpEq,
    }
}

// UnOps
pub fn select_bit<A: Arch>(val: impl Into<Val<A>>, bit_index: u8) -> Op<A> {
    Op::UnOp {
        arg: val.into(),
        op: UnOp::SelectBit(bit_index),
    }
}

pub fn is_zero<A: Arch>(val: impl Into<Val<A>>) -> Op<A> {
    Op::UnOp {
        arg: val.into(),
        op: UnOp::IsZero,
    }
}

pub fn sign_extend<A: Arch>(val: impl Into<Val<A>>, num_bits: u8) -> Op<A> {
    Op::UnOp {
        arg: val.into(),
        op: UnOp::SignExtend(num_bits),
    }
}

// ITE
/// Returns the first value if the condition is zero, and the second value if the condition is non-zero.
pub fn ite<A: Arch>(condition: impl Into<Val<A>>, if_zero: impl Into<Val<A>>, if_nonzero: impl Into<Val<A>>) -> Op<A> {
    Op::Ite {
        cond: condition.into(),
        if_zero: if_zero.into(),
        if_nonzero: if_nonzero.into(),
    }
}
