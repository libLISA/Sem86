use std::marker::PhantomData;
use std::mem::swap;

use liblisa::arch::Arch;
use liblisa::encoding::bitpattern::{Bit, ImmBitOrder, MappingOrBitOrder, Part, PartMapping, PartValue};
use liblisa::encoding::dataflows::{
    AccessKind, AddrTerm, AddrTermShift, AddrTermSize, AddressComputation, Inputs, MemoryAccess, MemorySizeRange,
    ParameterizedComputation,
};
use liblisa::encoding::{ParLoc, UnsizedParLoc};
use liblisa::state::Size;
use sem86_core::arch::intel386::{GpReg, Intel386, Reg};
use sem86_core::il::{Cmd, Jump, Val};

use crate::context::{Context, Mode};
use crate::instrs::DWORD;

type V = Val<Intel386>;

pub trait Builder {
    type Output;

    fn build(&self, ctx: Context, next: &mut dyn FnMut(Context, Self::Output));
}

impl<O> Builder for Box<dyn Builder<Output = O>> {
    type Output = O;

    fn build(&self, ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        (**self).build(ctx, next)
    }
}

pub struct Chain<A, B>(A, B);

impl<A, B> Chain<A, B> {
    pub fn new(a: A, b: B) -> Self {
        Self(a, b)
    }
}

impl<A: Builder, C: Builder, B: Fn(A::Output) -> C> Builder for Chain<A, B> {
    type Output = C::Output;

    fn build(&self, ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        self.0.build(ctx, &mut |ctx, a| {
            let b = self.1(a);
            b.build(ctx, &mut |ctx, b| next(ctx, b))
        })
    }
}

#[derive(Copy, Clone, Default)]
pub struct Name(&'static str);

impl Name {
    pub fn new(s: &'static str) -> Self {
        Self(s)
    }
}

impl Builder for Name {
    type Output = ();

    fn build(&self, mut ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        ctx.set_name(self.0);
        next(ctx, ())
    }
}

#[derive(Copy, Clone, Default)]
pub struct LegacyPrefixesWithRep;

impl LegacyPrefixesWithRep {
    pub fn new() -> Self {
        Self
    }
}

impl Builder for LegacyPrefixesWithRep {
    type Output = Option<bool>;

    fn build(&self, ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        Prefixes {
            enable_rep: true,
        }
        .build(ctx, next)
    }
}

#[derive(Copy, Clone, Default)]
pub struct Prefixes {
    enable_rep: bool,
}

impl Prefixes {
    pub fn new() -> Self {
        Self {
            enable_rep: false,
        }
    }
}

impl Builder for Prefixes {
    type Output = Option<bool>;

    fn build(&self, mut ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        let lockable = ctx.is_lockable();
        ctx.set_reppable(self.enable_rep);

        // No override
        next(ctx.clone(), None);

        for enabled_groups in 1..16u32 {
            if !lockable && !self.enable_rep && enabled_groups & 1 != 0 {
                continue
            }

            let mut ctxes = vec![(ctx.clone(), None)];
            let enabled_groups = (0..4).filter(|n| (enabled_groups >> n) & 1 != 0).map(|n| n + 1);
            for group in enabled_groups {
                match group {
                    1 => {
                        let mut old_ctxes = Vec::new();
                        swap(&mut ctxes, &mut old_ctxes);

                        for (mut ctx, _) in old_ctxes {
                            if self.enable_rep {
                                {
                                    let mut ctx = ctx.clone();
                                    ctx.add_bit(Bit::Fixed(1));
                                    ctx.add_bit(Bit::Fixed(1));
                                    ctx.add_bit(Bit::Fixed(1));
                                    ctx.add_bit(Bit::Fixed(1));
                                    ctx.add_bit(Bit::Fixed(0));
                                    ctx.add_bit(Bit::Fixed(0));
                                    ctx.add_bit(Bit::Fixed(1));
                                    ctx.add_bit(Bit::Fixed(0));
                                    ctx.append_name("_repne");
                                    ctx.set_rep(true);
                                    ctxes.push((ctx, Some(false)))
                                }

                                {
                                    let mut ctx = ctx.clone();
                                    ctx.add_bit(Bit::Fixed(1));
                                    ctx.add_bit(Bit::Fixed(1));
                                    ctx.add_bit(Bit::Fixed(1));
                                    ctx.add_bit(Bit::Fixed(1));
                                    ctx.add_bit(Bit::Fixed(0));
                                    ctx.add_bit(Bit::Fixed(0));
                                    ctx.add_bit(Bit::Fixed(1));
                                    ctx.add_bit(Bit::Fixed(1));
                                    ctx.append_name("_repe");
                                    ctx.set_rep(true);
                                    ctxes.push((ctx, Some(true)))
                                }
                            }

                            if lockable {
                                ctx.add_bit(Bit::Fixed(1));
                                ctx.add_bit(Bit::Fixed(1));
                                ctx.add_bit(Bit::Fixed(1));
                                ctx.add_bit(Bit::Fixed(1));
                                ctx.add_bit(Bit::Fixed(0));
                                ctx.add_bit(Bit::Fixed(0));
                                ctx.add_bit(Bit::Fixed(0));
                                ctx.add_bit(Bit::Fixed(0));
                                ctxes.push((ctx, None))
                            }
                        }
                    },
                    2 => {
                        let mut old_ctxes = Vec::new();
                        swap(&mut ctxes, &mut old_ctxes);

                        for (ctx, rep) in old_ctxes {
                            // TODO: Both overrides in one part:
                            // 0a1aa1aa
                            //  0 00 10 = ES (2)
                            //  0 01 10 = CS (6)
                            //  0 10 10 = SS (10)
                            //  0 11 10 = DS (14)
                            //  1 00 00 = FS (16)
                            //  1 00 01 = GS (17)
                            // Override ES/CS/SS/DS
                            {
                                let mut ctx = ctx.clone();
                                ctx.add_bit(Bit::Fixed(0));
                                ctx.add_bit(Bit::Fixed(0));
                                ctx.add_bit(Bit::Fixed(1));
                                let part = ctx.add_part(Part {
                                    size: 2,
                                    value: 0,
                                    mapping: PartMapping::Register {
                                        mapping: vec![
                                            Some(Reg::Gp(GpReg::EsBase)), // 0x26
                                            Some(Reg::Gp(GpReg::CsBase)), // 0x2E
                                            Some(Reg::Gp(GpReg::SsBase)), // 0x36
                                            Some(Reg::Gp(GpReg::DsBase)), // 0x3E
                                        ],
                                    },
                                });
                                ctx.add_bit(Bit::Fixed(1));
                                ctx.add_bit(Bit::Fixed(1));
                                ctx.add_bit(Bit::Fixed(0));

                                ctx.set_segment_override(ParLoc {
                                    loc: UnsizedParLoc::Part(part),
                                    size: Size::qword(),
                                });
                                ctx.append_name("_override_ecsd");

                                ctxes.push((ctx, rep));
                            }

                            // Override FS/GS
                            {
                                let mut ctx = ctx;
                                ctx.add_bit(Bit::Fixed(0));
                                ctx.add_bit(Bit::Fixed(1));
                                ctx.add_bit(Bit::Fixed(1));
                                ctx.add_bit(Bit::Fixed(0));
                                ctx.add_bit(Bit::Fixed(0));
                                ctx.add_bit(Bit::Fixed(1));
                                ctx.add_bit(Bit::Fixed(0));
                                let part = ctx.add_part(Part {
                                    size: 1,
                                    value: 0,
                                    mapping: PartMapping::Register {
                                        mapping: vec![
                                            Some(Reg::Gp(GpReg::FsBase)), // 0x64
                                            Some(Reg::Gp(GpReg::GsBase)), // 0x65
                                        ],
                                    },
                                });

                                ctx.set_segment_override(ParLoc {
                                    loc: UnsizedParLoc::Part(part),
                                    size: Size::qword(),
                                });
                                ctx.append_name("_override_fg");

                                ctxes.push((ctx, rep));
                            }
                        }
                    },
                    3 => {
                        for (ctx, _) in ctxes.iter_mut() {
                            ctx.add_bit(Bit::Fixed(0));
                            ctx.add_bit(Bit::Fixed(1));
                            ctx.add_bit(Bit::Fixed(1));
                            ctx.add_bit(Bit::Fixed(0));
                            ctx.add_bit(Bit::Fixed(0));
                            ctx.add_bit(Bit::Fixed(1));
                            ctx.add_bit(Bit::Fixed(1));
                            ctx.add_bit(Bit::Fixed(0));

                            ctx.override_wide_operand_size();
                        }
                    },
                    4 => {
                        for (ctx, _) in ctxes.iter_mut() {
                            ctx.add_bit(Bit::Fixed(0));
                            ctx.add_bit(Bit::Fixed(1));
                            ctx.add_bit(Bit::Fixed(1));
                            ctx.add_bit(Bit::Fixed(0));
                            ctx.add_bit(Bit::Fixed(0));
                            ctx.add_bit(Bit::Fixed(1));
                            ctx.add_bit(Bit::Fixed(1));
                            ctx.add_bit(Bit::Fixed(1));

                            ctx.override_address_size();
                        }
                    },
                    _ => unreachable!(),
                }
            }

            for (ctx, rep) in ctxes {
                next(ctx, rep);
            }
        }
    }
}

#[derive(Copy, Clone, Default)]
pub struct SegmentOverride;

impl SegmentOverride {
    pub fn new() -> Self {
        Self
    }
}

impl Builder for SegmentOverride {
    type Output = ();

    fn build(&self, original_ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        // No override
        next(original_ctx.clone(), ());
    }
}

#[derive(Copy, Clone, Default)]
pub struct FixedBit<const B: u8>;

impl<const B: u8> FixedBit<B> {
    pub fn new() -> Self {
        Self
    }
}

impl<const B: u8> Builder for FixedBit<B> {
    type Output = ();

    fn build(&self, mut ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        ctx.add_bit(Bit::Fixed(B));
        next(ctx, ())
    }
}

#[derive(Copy, Clone, Default)]
pub struct Byte<const B: u8>;

impl<const B: u8> Byte<B> {
    pub fn new() -> Self {
        Self
    }
}

impl<const B: u8> Builder for Byte<B> {
    type Output = ();

    fn build(&self, mut ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        for n in (0..8).rev() {
            ctx.add_bit(Bit::Fixed((B >> n) & 1));
        }

        next(ctx, ())
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ModVal {
    Mod00,
    Mod01,
    Mod10,
    Mod11,
}

impl From<u64> for ModVal {
    fn from(value: u64) -> Self {
        match value {
            0 => ModVal::Mod00,
            1 => ModVal::Mod01,
            2 => ModVal::Mod10,
            3 => ModVal::Mod11,
            _ => unreachable!(),
        }
    }
}

#[derive(Copy, Clone, Default)]
pub struct Mod;

impl Mod {
    pub fn new() -> Self {
        Self
    }
}

impl Builder for Mod {
    type Output = ModVal;

    fn build(&self, ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        BitsInto::<ModVal>::new(2).build(ctx, next)
    }
}

#[derive(Copy, Clone, Default)]
pub struct DontCare;

impl DontCare {
    pub fn new() -> Self {
        Self
    }
}

impl Builder for DontCare {
    type Output = ();

    fn build(&self, mut ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        ctx.add_bit(Bit::DontCare);
        next(ctx, ())
    }
}

#[derive(Copy, Clone, Default)]
pub struct ExpandedBit;

impl ExpandedBit {
    pub fn new() -> Self {
        Self
    }
}

impl Builder for ExpandedBit {
    type Output = bool;

    fn build(&self, ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        let mut c = ctx.clone();
        c.add_bit(Bit::Fixed(0));
        next(c, false);

        let mut c = ctx;
        c.add_bit(Bit::Fixed(1));
        next(c, true)
    }
}

#[derive(Copy, Clone, Default)]
pub struct W;

impl W {
    pub fn new() -> Self {
        Self
    }
}

impl Builder for W {
    type Output = bool;

    fn build(&self, ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        ExpandedBit::new().build(ctx, &mut |mut ctx, val| {
            ctx.set_wide(val);
            next(ctx, val)
        })
    }
}

pub struct AllowHighRegByte(bool);

impl AllowHighRegByte {
    pub fn new(allow_high_byte: bool) -> Self {
        Self(allow_high_byte)
    }
}

impl Builder for AllowHighRegByte {
    type Output = ();

    fn build(&self, mut ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        ctx.set_allow_high_reg_byte(self.0);
        next(ctx, ())
    }
}

pub struct Lockable;

impl Default for Lockable {
    fn default() -> Self {
        Self::new()
    }
}

impl Lockable {
    pub fn new() -> Self {
        Self
    }
}

impl Builder for Lockable {
    type Output = ();

    fn build(&self, mut ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        ctx.mark_lockable();
        next(ctx, ())
    }
}

pub struct AjustForPop;

impl Default for AjustForPop {
    fn default() -> Self {
        Self::new()
    }
}

impl AjustForPop {
    pub fn new() -> Self {
        Self
    }
}

impl Builder for AjustForPop {
    type Output = ();

    fn build(&self, mut ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        ctx.set_stack_adjustment(ctx.op_size() as i64);
        next(ctx, ())
    }
}

pub struct Wide(bool);

impl Wide {
    pub fn new(is_wide: bool) -> Self {
        Self(is_wide)
    }
}

impl Builder for Wide {
    type Output = ();

    fn build(&self, mut ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        ctx.set_wide(self.0);
        next(ctx, ())
    }
}

pub struct OverrideMemorySize(usize);

impl OverrideMemorySize {
    pub fn new(num: usize) -> Self {
        Self(num)
    }
}

impl Builder for OverrideMemorySize {
    type Output = ();

    fn build(&self, mut ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        ctx.override_mem_size(self.0);
        next(ctx, ())
    }
}

pub struct OverrideRmSizingMode(Mode);

impl OverrideRmSizingMode {
    pub fn new(mode: Mode) -> Self {
        Self(mode)
    }
}

impl Builder for OverrideRmSizingMode {
    type Output = ();

    fn build(&self, mut ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        ctx.override_rm_sizing_mode(self.0);
        next(ctx, ())
    }
}

pub struct MaximumOperandSize;

impl Default for MaximumOperandSize {
    fn default() -> Self {
        Self::new()
    }
}

impl MaximumOperandSize {
    pub fn new() -> Self {
        Self
    }
}

impl Builder for MaximumOperandSize {
    type Output = ();

    fn build(&self, mut ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        ctx.maximum_operand_size();
        next(ctx, ())
    }
}

#[derive(Copy, Clone, Default)]
pub struct S;

impl S {
    pub fn new() -> Self {
        Self
    }
}

impl Builder for S {
    type Output = bool;

    fn build(&self, ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        ExpandedBit::new().build(ctx, &mut |mut ctx, val| {
            ctx.set_sign_extend(val);
            next(ctx, val)
        })
    }
}

pub struct SetSignExtend(bool);

impl SetSignExtend {
    pub fn new(sign_extend: bool) -> Self {
        Self(sign_extend)
    }
}

impl Builder for SetSignExtend {
    type Output = ();

    fn build(&self, mut ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        ctx.set_sign_extend(self.0);
        next(ctx, ())
    }
}

pub struct SrcDst {
    to_reg: bool,
    reg: V,
    rm: V,
}

impl SrcDst {
    pub fn new(direction: bool, reg: V, rm: V) -> Self {
        Self {
            to_reg: direction,
            reg,
            rm,
        }
    }
}

impl Builder for SrcDst {
    type Output = (V, V);

    fn build(&self, ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        if self.to_reg {
            next(ctx, (self.rm, self.reg))
        } else {
            next(ctx, (self.reg, self.rm))
        }
    }
}

pub struct Imm(ImmWithMapping);

impl Imm {
    pub fn new(num: usize) -> Imm {
        assert!(num != 0);
        Self(ImmWithMapping {
            num,
            mapping: None,
        })
    }
}

impl Builder for Imm {
    type Output = Val<Intel386>;

    fn build(&self, ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        self.0.build(ctx, next)
    }
}

pub struct ImmWithMapping {
    num: usize,
    mapping: Option<Vec<PartValue>>,
}

impl ImmWithMapping {
    pub fn new(num: usize, mapping: Vec<PartValue>) -> Self {
        assert!(num != 0);
        assert!(mapping.len() == 1 << num);

        Self {
            num,
            mapping: Some(mapping),
        }
    }
}

impl Builder for ImmWithMapping {
    type Output = Val<Intel386>;

    fn build(&self, mut ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        let part = ctx.add_part(Part {
            size: self.num,
            value: 0,
            mapping: PartMapping::Imm {
                mapping: self
                    .mapping
                    .as_ref()
                    .map(|mapping| MappingOrBitOrder::Mapping(mapping.clone())),
                bits: None,
            },
        });

        let val = Val::Conv {
            loc: ParLoc {
                loc: UnsizedParLoc::Part(part),
                size: Size::qword(),
            },
            source_bits: self.num.try_into().unwrap(),
            target_bits: 64,
            sign_extend: ctx.is_sign_extend(),
            swap_endianness: true,
        };

        next(ctx, val)
    }
}

pub struct ImmWithBitOrder {
    num: usize,
    bit_order: Vec<ImmBitOrder>,
}

impl ImmWithBitOrder {
    pub fn new(num: usize, bit_order: Vec<ImmBitOrder>) -> Self {
        assert!(num != 0);
        assert!(bit_order.len() == num);

        Self {
            num,
            bit_order,
        }
    }
}

impl Builder for ImmWithBitOrder {
    type Output = Val<Intel386>;

    fn build(&self, mut ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        let part = ctx.add_part(Part {
            size: self.num,
            value: 0,
            mapping: PartMapping::Imm {
                mapping: Some(MappingOrBitOrder::BitOrder(self.bit_order.clone())),
                bits: None,
            },
        });

        let val = Val::Conv {
            loc: ParLoc {
                loc: UnsizedParLoc::Part(part),
                size: Size::qword(),
            },
            source_bits: self.num.try_into().unwrap(),
            target_bits: 64,
            sign_extend: ctx.is_sign_extend(),
            swap_endianness: true,
        };

        next(ctx, val)
    }
}

#[derive(Copy, Clone, Default)]
pub struct FullImm;

impl FullImm {
    pub fn new() -> Self {
        Self
    }
}

impl Builder for FullImm {
    type Output = Val<Intel386>;

    fn build(&self, ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        let size = if ctx.is_sign_extend() { 8 } else { ctx.op_size() * 8 };

        Imm::new(size).build(ctx, next)
    }
}

pub struct Disp(usize);

impl Disp {
    pub fn new(num: usize) -> Self {
        Self(num)
    }
}

fn create_bitorder(num: usize) -> MappingOrBitOrder {
    MappingOrBitOrder::BitOrder({
        let mut order = Vec::new();
        for n in 0..num {
            order.insert(n % 8, ImmBitOrder::Positive(n))
        }

        if order.len() <= 8 {
            order[7] = match order[7] {
                ImmBitOrder::Positive(n) => ImmBitOrder::Negative(n),
                ImmBitOrder::Negative(n) => ImmBitOrder::Positive(n),
            };
        }

        order
    })
}

impl Builder for Disp {
    type Output = ParLoc<Intel386>;

    fn build(&self, mut ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        let part = ctx.add_part(Part {
            size: self.0,
            value: 0,
            mapping: PartMapping::Imm {
                mapping: Some(create_bitorder(self.0)),
                bits: None,
            },
        });

        next(
            ctx,
            ParLoc {
                loc: UnsizedParLoc::Part(part),
                size: Size::qword(),
            },
        )
    }
}

#[derive(Copy, Clone, Default)]
pub struct FullDisp;

impl FullDisp {
    pub fn new() -> Self {
        Self
    }
}

impl Builder for FullDisp {
    type Output = ParLoc<Intel386>;

    fn build(&self, ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        Disp::new(ctx.addr_size() * 8).build(ctx, next)
    }
}

pub struct ExpandedBits(usize);

impl ExpandedBits {
    pub fn new(num: usize) -> Self {
        Self(num)
    }
}

impl Builder for ExpandedBits {
    type Output = u64;

    fn build(&self, ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        for v in 0..(1 << self.0) {
            let mut ctx = ctx.clone();
            for n in (0..self.0).rev() {
                ctx.add_bit(Bit::Fixed((v >> n) as u8 & 1));
            }

            next(ctx, v)
        }
    }
}

pub struct BitsInto<T> {
    num: usize,
    _ph: PhantomData<T>,
}

impl<T> BitsInto<T> {
    pub fn new(num: usize) -> Self {
        Self {
            num,
            _ph: PhantomData,
        }
    }
}

impl<T: From<u64>> Builder for BitsInto<T> {
    type Output = T;

    fn build(&self, ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        ExpandedBits::new(self.num).build(ctx, &mut |ctx, val| next(ctx, T::from(val)))
    }
}

pub struct TryBitsInto<T> {
    num: usize,
    _ph: PhantomData<T>,
}

impl<T> TryBitsInto<T> {
    pub fn new(num: usize) -> Self {
        Self {
            num,
            _ph: PhantomData,
        }
    }
}

impl<T: TryFrom<u64>> Builder for TryBitsInto<T> {
    type Output = T;

    fn build(&self, ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        ExpandedBits::new(self.num).build(ctx, &mut |ctx, val| {
            if let Ok(t) = T::try_from(val) {
                next(ctx, t)
            }
        })
    }
}

#[derive(Copy, Clone, Default)]
pub struct RegBits;

impl RegBits {
    pub fn new() -> Self {
        Self
    }
}

impl Builder for RegBits {
    type Output = V;

    fn build(&self, mut ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        if ctx.is_segment_op() {
            todo!()
        }

        let full_size = Size::new(0, ctx.op_size() - 1);
        if ctx.is_wide_op() || ctx.is_sign_extend() || !ctx.allow_high_reg_byte() {
            let part = ctx.add_part(Part {
                size: 3,
                value: 0,
                mapping: PartMapping::Register {
                    mapping: vec![
                        Some(Reg::Gp(GpReg::Ax)),
                        Some(Reg::Gp(GpReg::Cx)),
                        Some(Reg::Gp(GpReg::Dx)),
                        Some(Reg::Gp(GpReg::Bx)),
                        Some(Reg::Gp(GpReg::Sp)),
                        Some(Reg::Gp(GpReg::Bp)),
                        Some(Reg::Gp(GpReg::Si)),
                        Some(Reg::Gp(GpReg::Di)),
                    ],
                },
            });

            next(
                ctx,
                Val::Loc(ParLoc {
                    loc: UnsizedParLoc::Part(part),
                    size: full_size,
                }),
            )
        } else {
            ExpandedBit::new().build(ctx, &mut |mut ctx, first_bit| {
                let part = ctx.add_part(Part {
                    size: 2,
                    value: 0,
                    mapping: PartMapping::Register {
                        mapping: vec![
                            Some(Reg::Gp(GpReg::Ax)),
                            Some(Reg::Gp(GpReg::Cx)),
                            Some(Reg::Gp(GpReg::Dx)),
                            Some(Reg::Gp(GpReg::Bx)),
                        ],
                    },
                });

                next(
                    ctx,
                    Val::Loc(ParLoc {
                        loc: UnsizedParLoc::Part(part),
                        size: if first_bit { Size::new(1, 1) } else { Size::new(0, 0) },
                    }),
                )
            })
        }
    }
}

pub struct FixedReg(GpReg);

impl FixedReg {
    pub fn new(reg: GpReg) -> Self {
        Self(reg)
    }
}

impl Builder for FixedReg {
    type Output = V;

    fn build(&self, ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        let full_size = Size::new(0, ctx.op_size() - 1);
        if ctx.is_wide_op() || ctx.is_sign_extend() {
            next(
                ctx,
                Val::Loc(ParLoc {
                    loc: UnsizedParLoc::Reg(Reg::Gp(self.0)),
                    size: full_size,
                }),
            )
        } else {
            next(
                ctx,
                Val::Loc(ParLoc {
                    loc: UnsizedParLoc::Reg(Reg::Gp(self.0)),
                    size: Size::new(0, 0),
                }),
            )
        }
    }
}

#[derive(Copy, Clone, Default)]
pub struct Acc;

impl Acc {
    pub fn new() -> Self {
        Self
    }
}

impl Builder for Acc {
    type Output = V;

    fn build(&self, ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        FixedReg::new(GpReg::Ax).build(ctx, next)
    }
}

#[derive(Copy, Clone, Default)]
pub struct Sreg2 {
    allow_cs: bool,
}

impl Sreg2 {
    pub fn new(allow_cs: bool) -> Self {
        Self {
            allow_cs,
        }
    }
}

impl Builder for Sreg2 {
    type Output = V;

    fn build(&self, mut ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        let part = ctx.add_part(Part {
            size: 2,
            value: 0,
            mapping: PartMapping::Register {
                mapping: vec![
                    Some(Reg::Gp(GpReg::Es)),
                    if self.allow_cs { Some(Reg::Gp(GpReg::Cs)) } else { None },
                    Some(Reg::Gp(GpReg::Ss)),
                    Some(Reg::Gp(GpReg::Ds)),
                ],
            },
        });

        next(
            ctx,
            Val::Loc(ParLoc {
                loc: UnsizedParLoc::Part(part),
                size: Size::new(0, 1),
            }),
        )
    }
}

#[derive(Copy, Clone, Default)]
pub struct Sreg3 {
    allow_ecsd: bool,
}

impl Sreg3 {
    pub fn new(allow_ecsd: bool) -> Self {
        Self {
            allow_ecsd,
        }
    }
}

impl Builder for Sreg3 {
    type Output = V;

    fn build(&self, mut ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        let part = ctx.add_part(Part {
            size: 3,
            value: 0,
            mapping: PartMapping::Register {
                mapping: vec![
                    if self.allow_ecsd { Some(Reg::Gp(GpReg::Es)) } else { None },
                    if self.allow_ecsd { Some(Reg::Gp(GpReg::Cs)) } else { None },
                    if self.allow_ecsd { Some(Reg::Gp(GpReg::Ss)) } else { None },
                    if self.allow_ecsd { Some(Reg::Gp(GpReg::Ds)) } else { None },
                    Some(Reg::Gp(GpReg::Fs)),
                    Some(Reg::Gp(GpReg::Gs)),
                    None,
                    None,
                ],
            },
        });

        next(
            ctx,
            Val::Loc(ParLoc {
                loc: UnsizedParLoc::Part(part),
                size: Size::new(0, 1),
            }),
        )
    }
}

#[derive(Copy, Clone, Default)]
pub struct ExpandedSreg2 {
    allow_cs: bool,
}

impl ExpandedSreg2 {
    pub fn new(allow_cs: bool) -> Self {
        Self {
            allow_cs,
        }
    }
}

impl Builder for ExpandedSreg2 {
    type Output = (V, u64);

    fn build(&self, ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        ExpandedBits::new(2).build(ctx, &mut |ctx, sreg_index| {
            if let Some(sreg) = [
                Some(Reg::Gp(GpReg::Es)),
                if self.allow_cs { Some(Reg::Gp(GpReg::Cs)) } else { None },
                Some(Reg::Gp(GpReg::Ss)),
                Some(Reg::Gp(GpReg::Ds)),
            ][sreg_index as usize]
            {
                let sreg = Val::Loc(ParLoc {
                    loc: UnsizedParLoc::Reg(sreg),
                    size: Size::new(0, 1),
                });
                next(ctx, (sreg, sreg_index))
            }
        })
    }
}

#[derive(Copy, Clone, Default)]
pub struct ExpandedSreg3 {
    allow_ecsd: bool,
}

impl ExpandedSreg3 {
    pub fn new(allow_ecsd: bool) -> Self {
        Self {
            allow_ecsd,
        }
    }
}

impl Builder for ExpandedSreg3 {
    type Output = (V, u64);

    fn build(&self, ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        ExpandedBits::new(3).build(ctx, &mut |ctx, sreg_index| {
            if !self.allow_ecsd && sreg_index < 4 {
                return;
            }

            if let Some(&sreg) = [GpReg::Es, GpReg::Cs, GpReg::Ss, GpReg::Ds, GpReg::Fs, GpReg::Gs].get(sreg_index as usize) {
                let sreg = Val::Loc(ParLoc {
                    loc: UnsizedParLoc::Reg(Reg::Gp(sreg)),
                    size: Size::new(0, 1),
                });
                next(ctx, (sreg, sreg_index))
            }
        })
    }
}

pub struct FarPointerRm {
    md: ModVal,
}

impl FarPointerRm {
    pub fn new(md: ModVal) -> Self {
        Self {
            md,
        }
    }
}

impl Builder for FarPointerRm {
    /// (offset, selector)
    type Output = (V, V);

    fn build(&self, mut ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        if self.md != ModVal::Mod11 {
            let mem_size = ctx.op_size() + 2;
            ctx.override_mem_size(mem_size);
            Rm::new(self.md).build(ctx, &mut |ctx, mem| match mem {
                Val::Loc(ParLoc {
                    loc: UnsizedParLoc::Mem(mem_index),
                    size,
                }) if size.start_byte() == 0 && size.end_byte() == mem_size - 1 => {
                    let offset = Val::Loc(ParLoc {
                        loc: UnsizedParLoc::Mem(mem_index),
                        size: ctx.size(),
                    });

                    let selector = Val::Loc(ParLoc {
                        loc: UnsizedParLoc::Mem(mem_index),
                        size: Size::new(ctx.op_size(), ctx.op_size() + 1),
                    });

                    next(ctx, (offset, selector))
                },
                _ => unreachable!(),
            });
        }
    }
}

pub struct Rm {
    md: ModVal,
}

impl Rm {
    pub fn new(md: ModVal) -> Self {
        Self {
            md,
        }
    }

    fn add_disp(&self, ctx: &mut Context) -> ParLoc<Intel386> {
        let size = match self.md {
            ModVal::Mod01 => 8,
            _ => ctx.memory_reg_and_addr_size().0.max_bit_influence(),
        };

        let disp = ctx.add_part(Part {
            size,
            value: 0,
            mapping: PartMapping::Imm {
                mapping: Some(create_bitorder(size)),
                bits: None,
            },
        });

        ParLoc {
            loc: UnsizedParLoc::Part(disp),
            size: Size::qword(),
        }
    }

    fn determine_segment(ctx: &mut Context, inputs: &[ParLoc<Intel386>], force_stack_op: bool) -> ParLoc<Intel386> {
        ctx.segment_override().unwrap_or_else(|| ParLoc {
            loc: UnsizedParLoc::Reg(Reg::Gp(
                if inputs.iter().any(|input| {
                    force_stack_op
                        || input.loc == UnsizedParLoc::Reg(Reg::Gp(GpReg::Bp))
                        || input.loc == UnsizedParLoc::Reg(Reg::Gp(GpReg::Sp))
                }) {
                    GpReg::SsBase
                } else {
                    GpReg::DsBase
                },
            )),
            size: Size::qword(),
        })
    }

    fn determine_mem_size(&self, ctx: &Context) -> usize {
        if let Some(m) = ctx.rm_sizing_mode_override() {
            ctx.op_size_ext(m, ctx.is_wide_op(), false)
        } else {
            ctx.memory_size()
        }
    }

    fn determine_reg_size(&self, ctx: &Context) -> usize {
        if let Some(m) = ctx.rm_sizing_mode_override() {
            ctx.op_size_ext(m, ctx.is_wide_op(), false)
        } else {
            ctx.op_size()
        }
    }
}

impl Builder for Rm {
    type Output = V;

    fn build(&self, ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        match self.md {
            ModVal::Mod00 | ModVal::Mod01 | ModVal::Mod10 => {
                let (s, _) = ctx.memory_reg_and_addr_size();
                match s {
                    AddrTermSize::U32 => {
                        {
                            let mut ctx = ctx.clone();
                            let terms = [
                                AddrTerm::single(AddrTermSize::U32, 0, 1),
                                AddrTerm::single(s, 0, 1),
                                AddrTerm::single(s, 0, 1),
                                AddrTerm::single(s, 0, 1),
                                AddrTerm::single(s, 0, 1),
                            ];
                            let base = ctx.add_part(Part {
                                size: 3,
                                value: 0,
                                mapping: PartMapping::Register {
                                    mapping: vec![
                                        Some(Reg::Gp(GpReg::Ax)),
                                        Some(Reg::Gp(GpReg::Cx)),
                                        Some(Reg::Gp(GpReg::Dx)),
                                        Some(Reg::Gp(GpReg::Bx)),
                                        None,
                                        // TODO: Can we actually use BP here? Wouldn't BP require the SS segment by default?
                                        if self.md != ModVal::Mod00 {
                                            Some(Reg::Gp(GpReg::Bp))
                                        } else {
                                            None
                                        },
                                        Some(Reg::Gp(GpReg::Si)),
                                        Some(Reg::Gp(GpReg::Di)),
                                    ],
                                },
                            });

                            let inputs = Inputs::unsorted({
                                let mut inputs = vec![ParLoc {
                                    loc: UnsizedParLoc::Part(base),
                                    size: DWORD,
                                }];

                                if self.md == ModVal::Mod01 || self.md == ModVal::Mod10 {
                                    inputs.push(self.add_disp(&mut ctx));
                                }

                                inputs.insert(0, Self::determine_segment(&mut ctx, &inputs, false));
                                inputs
                            });

                            let size = self.determine_mem_size(&ctx);
                            let mem = ctx.add_access(MemoryAccess {
                                kind: AccessKind::InputOutput,
                                size: MemorySizeRange::new(size as u64, size as u64),
                                calculation: ParameterizedComputation::Calculation(
                                    AddressComputation::from_iter(terms[..inputs.len()].iter().cloned(), 0)
                                        .with_addr_size(ctx.memory_reg_and_addr_size().1),
                                ),
                                alignment: 1,
                                inputs,
                            });

                            next(ctx, Val::Loc(mem))
                        }

                        if self.md != ModVal::Mod00 {
                            // base = 0b101 (BP) -- needs stack segment
                            let mut ctx = ctx.clone();
                            let terms = [
                                AddrTerm::single(AddrTermSize::U32, 0, 1),
                                AddrTerm::single(s, 0, 1),
                                AddrTerm::single(s, 0, 1),
                                AddrTerm::single(s, 0, 1),
                                AddrTerm::single(s, 0, 1),
                            ];

                            ctx.add_bit(Bit::Fixed(1));
                            ctx.add_bit(Bit::Fixed(0));
                            ctx.add_bit(Bit::Fixed(1));

                            let inputs = Inputs::unsorted({
                                let mut inputs = vec![ParLoc {
                                    loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Bp)),
                                    size: DWORD,
                                }];

                                if self.md == ModVal::Mod01 || self.md == ModVal::Mod10 {
                                    inputs.push(self.add_disp(&mut ctx));
                                }

                                inputs.insert(0, Self::determine_segment(&mut ctx, &inputs, true));
                                inputs
                            });

                            let size = self.determine_mem_size(&ctx);
                            let mem = ctx.add_access(MemoryAccess {
                                kind: AccessKind::InputOutput,
                                size: MemorySizeRange::new(size as u64, size as u64),
                                calculation: ParameterizedComputation::Calculation(
                                    AddressComputation::from_iter(terms[..inputs.len()].iter().cloned(), 0)
                                        .with_addr_size(ctx.memory_reg_and_addr_size().1),
                                ),
                                alignment: 1,
                                inputs,
                            });

                            next(ctx, Val::Loc(mem))
                        }

                        {
                            let mut ctx = ctx.clone();
                            // R/M = 0b100 (SIB)
                            ctx.add_bit(Bit::Fixed(1));
                            ctx.add_bit(Bit::Fixed(0));
                            ctx.add_bit(Bit::Fixed(0));

                            ExpandedBits::new(2).build(ctx.clone(), &mut |mut ctx, scale| {
                                let scale = 1 << scale;
                                let index = ctx.add_part(Part {
                                    size: 3,
                                    value: 0,
                                    mapping: PartMapping::Register {
                                        mapping: vec![
                                            Some(Reg::Gp(GpReg::Ax)),
                                            Some(Reg::Gp(GpReg::Cx)),
                                            Some(Reg::Gp(GpReg::Dx)),
                                            Some(Reg::Gp(GpReg::Bx)),
                                            Some(Reg::Gp(GpReg::Riz)),
                                            Some(Reg::Gp(GpReg::Bp)),
                                            Some(Reg::Gp(GpReg::Si)),
                                            Some(Reg::Gp(GpReg::Di)),
                                        ],
                                    },
                                });
                                let ctx = ctx;

                                {
                                    let mut ctx = ctx.clone();
                                    let mut terms = [
                                        AddrTerm::single(AddrTermSize::U32, 0, 1),
                                        AddrTerm::single(s, 0, 1),
                                        AddrTerm::single(s, 0, 1),
                                        AddrTerm::single(s, 0, 1),
                                        AddrTerm::single(s, 0, 1),
                                    ];
                                    let base = ctx.add_part(Part {
                                        size: 3,
                                        value: 0,
                                        mapping: PartMapping::Register {
                                            mapping: vec![
                                                Some(Reg::Gp(GpReg::Ax)),
                                                Some(Reg::Gp(GpReg::Cx)),
                                                Some(Reg::Gp(GpReg::Dx)),
                                                Some(Reg::Gp(GpReg::Bx)),
                                                None,
                                                None,
                                                Some(Reg::Gp(GpReg::Si)),
                                                Some(Reg::Gp(GpReg::Di)),
                                            ],
                                        },
                                    });

                                    let inputs = Inputs::unsorted({
                                        let mut inputs = vec![
                                            ParLoc {
                                                loc: UnsizedParLoc::Part(base),
                                                size: DWORD,
                                            },
                                            ParLoc {
                                                loc: UnsizedParLoc::Part(index),
                                                size: DWORD,
                                            },
                                        ];

                                        terms[2].primary.shift = AddrTermShift::new(0, scale);

                                        if self.md == ModVal::Mod01 || self.md == ModVal::Mod10 {
                                            inputs.push(self.add_disp(&mut ctx));
                                        }

                                        inputs.insert(0, Self::determine_segment(&mut ctx, &inputs, false));
                                        inputs
                                    });

                                    let size = self.determine_mem_size(&ctx);
                                    let mem = ctx.add_access(MemoryAccess {
                                        kind: AccessKind::InputOutput,
                                        size: MemorySizeRange::new(size as u64, size as u64),
                                        calculation: ParameterizedComputation::Calculation(
                                            AddressComputation::from_iter(terms[..inputs.len()].iter().cloned(), 0)
                                                .with_addr_size(ctx.memory_reg_and_addr_size().1),
                                        ),
                                        alignment: 1,
                                        inputs,
                                    });

                                    next(ctx, Val::Loc(mem))
                                }

                                {
                                    // Special case for base = 0b100 (SP -- which needs SS segment instead of DS)
                                    // It also needs an ajustment for push/pop instructions.
                                    let mut ctx = ctx.clone();
                                    let mut terms = [
                                        AddrTerm::single(AddrTermSize::U32, 0, 1),
                                        AddrTerm::single(s, 0, 1),
                                        AddrTerm::single(s, 0, 1),
                                        AddrTerm::single(s, 0, 1),
                                        AddrTerm::single(s, 0, 1),
                                    ];

                                    ctx.add_bit(Bit::Fixed(1));
                                    ctx.add_bit(Bit::Fixed(0));
                                    ctx.add_bit(Bit::Fixed(0));

                                    let inputs = Inputs::unsorted({
                                        let mut inputs = vec![
                                            ParLoc {
                                                loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Sp)),
                                                size: DWORD,
                                            },
                                            ParLoc {
                                                loc: UnsizedParLoc::Part(index),
                                                size: DWORD,
                                            },
                                        ];

                                        terms[2].primary.shift = AddrTermShift::new(0, scale);

                                        if self.md == ModVal::Mod01 || self.md == ModVal::Mod10 {
                                            inputs.push(self.add_disp(&mut ctx));
                                        }

                                        inputs.insert(0, Self::determine_segment(&mut ctx, &inputs, true));
                                        inputs
                                    });

                                    let size = self.determine_mem_size(&ctx);
                                    let mem = ctx.add_access(MemoryAccess {
                                        kind: AccessKind::InputOutput,
                                        size: MemorySizeRange::new(size as u64, size as u64),
                                        calculation: ParameterizedComputation::Calculation(
                                            AddressComputation::from_iter(terms[..inputs.len()].iter().cloned(), 0)
                                                .with_addr_size(ctx.memory_reg_and_addr_size().1)
                                                .with_offset(ctx.stack_adjustment()),
                                        ),
                                        alignment: 1,
                                        inputs,
                                    });

                                    next(ctx, Val::Loc(mem))
                                }

                                if self.md != ModVal::Mod00 {
                                    // Special case for base = 0b101 (BP -- which needs SS segment instead of DS)
                                    let mut ctx = ctx.clone();
                                    let mut terms = [
                                        AddrTerm::single(AddrTermSize::U32, 0, 1),
                                        AddrTerm::single(s, 0, 1),
                                        AddrTerm::single(s, 0, 1),
                                        AddrTerm::single(s, 0, 1),
                                        AddrTerm::single(s, 0, 1),
                                    ];

                                    ctx.add_bit(Bit::Fixed(1));
                                    ctx.add_bit(Bit::Fixed(0));
                                    ctx.add_bit(Bit::Fixed(1));

                                    let inputs = Inputs::unsorted({
                                        let mut inputs = vec![
                                            ParLoc {
                                                loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Bp)),
                                                size: DWORD,
                                            },
                                            ParLoc {
                                                loc: UnsizedParLoc::Part(index),
                                                size: DWORD,
                                            },
                                        ];

                                        terms[2].primary.shift = AddrTermShift::new(0, scale);

                                        if self.md == ModVal::Mod01 || self.md == ModVal::Mod10 {
                                            inputs.push(self.add_disp(&mut ctx));
                                        }

                                        inputs.insert(0, Self::determine_segment(&mut ctx, &inputs, true));
                                        inputs
                                    });

                                    let size = self.determine_mem_size(&ctx);
                                    let mem = ctx.add_access(MemoryAccess {
                                        kind: AccessKind::InputOutput,
                                        size: MemorySizeRange::new(size as u64, size as u64),
                                        calculation: ParameterizedComputation::Calculation(
                                            AddressComputation::from_iter(terms[..inputs.len()].iter().cloned(), 0)
                                                .with_addr_size(ctx.memory_reg_and_addr_size().1),
                                        ),
                                        alignment: 1,
                                        inputs,
                                    });

                                    next(ctx, Val::Loc(mem))
                                }

                                if self.md == ModVal::Mod00 {
                                    // Special case for base = 0b101. (no base, full disp)
                                    let mut ctx = ctx.clone();
                                    let mut terms = [
                                        AddrTerm::single(AddrTermSize::U32, 0, 1),
                                        AddrTerm::single(s, 0, 1),
                                        AddrTerm::single(s, 0, 1),
                                        AddrTerm::single(s, 0, 1),
                                        AddrTerm::single(s, 0, 1),
                                    ];

                                    ctx.add_bit(Bit::Fixed(1));
                                    ctx.add_bit(Bit::Fixed(0));
                                    ctx.add_bit(Bit::Fixed(1));

                                    let inputs = Inputs::unsorted({
                                        let mut inputs = vec![
                                            // Base is zero
                                            ParLoc {
                                                loc: UnsizedParLoc::Part(index),
                                                size: DWORD,
                                            },
                                        ];

                                        terms[1].primary.shift = AddrTermShift::new(0, scale);
                                        inputs.push(self.add_disp(&mut ctx));
                                        inputs.insert(0, Self::determine_segment(&mut ctx, &inputs, false));
                                        inputs
                                    });

                                    let size = self.determine_mem_size(&ctx);
                                    let mem = ctx.add_access(MemoryAccess {
                                        kind: AccessKind::InputOutput,
                                        size: MemorySizeRange::new(size as u64, size as u64),
                                        calculation: ParameterizedComputation::Calculation(
                                            AddressComputation::from_iter(terms[..inputs.len()].iter().cloned(), 0)
                                                .with_addr_size(ctx.memory_reg_and_addr_size().1),
                                        ),
                                        alignment: 1,
                                        inputs,
                                    });

                                    next(ctx, Val::Loc(mem))
                                }
                            })
                        }

                        if self.md == ModVal::Mod00 {
                            let mut ctx = ctx.clone();
                            // R/M = 0b101 (disp32)

                            ctx.add_bit(Bit::Fixed(1));
                            ctx.add_bit(Bit::Fixed(0));
                            ctx.add_bit(Bit::Fixed(1));

                            let mut ctx = ctx.clone();
                            let terms = [AddrTerm::single(AddrTermSize::U32, 0, 1), AddrTerm::single(s, 0, 1)];

                            let inputs =
                                Inputs::unsorted(vec![Self::determine_segment(&mut ctx, &[], false), self.add_disp(&mut ctx)]);

                            let size = self.determine_mem_size(&ctx);
                            let mem = ctx.add_access(MemoryAccess {
                                kind: AccessKind::InputOutput,
                                size: MemorySizeRange::new(size as u64, size as u64),
                                calculation: ParameterizedComputation::Calculation(
                                    AddressComputation::from_iter(terms[..inputs.len()].iter().cloned(), 0)
                                        .with_addr_size(ctx.memory_reg_and_addr_size().1),
                                ),
                                alignment: 1,
                                inputs,
                            });

                            next(ctx, Val::Loc(mem))
                        }
                    },
                    // TODO: Use parts here to reduce amount of instructions
                    AddrTermSize::U16 => {
                        let terms = [
                            AddrTerm::single(AddrTermSize::U32, 0, 1),
                            AddrTerm::single(s, 0, 1),
                            AddrTerm::single(s, 0, 1),
                            AddrTerm::single(s, 0, 1),
                            AddrTerm::single(s, 0, 1),
                        ];
                        ExpandedBits::new(2).build(ctx, &mut |mut ctx, rm_hi2| {
                            if rm_hi2 == 0b11 {
                                ExpandedBits::new(1).build(ctx, &mut |mut ctx, rm_lo1| {
                                    let inputs = Inputs::unsorted({
                                        let special_case = rm_lo1 == 0 && self.md == ModVal::Mod00;
                                        let mut inputs = match rm_lo1 {
                                            1 => vec![ParLoc {
                                                loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Bx)),
                                                size: Size::qword(),
                                            }],
                                            0 => {
                                                if special_case {
                                                    vec![]
                                                } else {
                                                    vec![ParLoc {
                                                        loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Bp)),
                                                        size: Size::qword(),
                                                    }]
                                                }
                                            },
                                            _ => unreachable!(),
                                        };

                                        if (self.md == ModVal::Mod01 || self.md == ModVal::Mod10) || special_case {
                                            inputs.push(self.add_disp(&mut ctx));
                                        }

                                        inputs.insert(0, Self::determine_segment(&mut ctx, &inputs, false));
                                        inputs
                                    });

                                    let size = self.determine_mem_size(&ctx);
                                    let mem = ctx.add_access(MemoryAccess {
                                        kind: AccessKind::InputOutput,
                                        size: MemorySizeRange::new(size as u64, size as u64),
                                        calculation: ParameterizedComputation::Calculation(
                                            AddressComputation::from_iter(terms[..inputs.len()].iter().cloned(), 0)
                                                .with_addr_size(ctx.memory_reg_and_addr_size().1),
                                        ),
                                        alignment: 1,
                                        inputs,
                                    });

                                    next(ctx, Val::Loc(mem))
                                })
                            } else {
                                let inputs = Inputs::unsorted({
                                    // TODO: We can create a part for BX/RIZ, but BP needs to be separate because it uses a separate segment
                                    let mut inputs = match rm_hi2 {
                                        0b00 => vec![ParLoc {
                                            loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Bx)),
                                            size: Size::qword(),
                                        }],
                                        0b01 => vec![ParLoc {
                                            loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Bp)),
                                            size: Size::qword(),
                                        }],
                                        0b10 => vec![],
                                        _ => unreachable!(),
                                    };

                                    let sidi = ctx.add_part(Part {
                                        size: 1,
                                        value: 0,
                                        mapping: PartMapping::Register {
                                            mapping: vec![Some(Reg::Gp(GpReg::Si)), Some(Reg::Gp(GpReg::Di))],
                                        },
                                    });
                                    inputs.push(ParLoc {
                                        loc: UnsizedParLoc::Part(sidi),
                                        size: Size::qword(),
                                    });

                                    if self.md == ModVal::Mod01 || self.md == ModVal::Mod10 {
                                        inputs.push(self.add_disp(&mut ctx));
                                    }

                                    inputs.insert(0, Self::determine_segment(&mut ctx, &inputs, false));
                                    inputs
                                });

                                let size = self.determine_mem_size(&ctx);
                                let mem = ctx.add_access(MemoryAccess {
                                    kind: AccessKind::InputOutput,
                                    size: MemorySizeRange::new(size as u64, size as u64),
                                    calculation: ParameterizedComputation::Calculation(
                                        AddressComputation::from_iter(terms[..inputs.len()].iter().cloned(), 0)
                                            .with_addr_size(ctx.memory_reg_and_addr_size().1),
                                    ),
                                    alignment: 1,
                                    inputs,
                                });

                                next(ctx, Val::Loc(mem))
                            }
                        })

                        // ExpandedBits::new(3).build(ctx, &mut |mut ctx, rm| {
                        //     let terms = [
                        //         AddrTerm::single(AddrTermSize::U32, 0, 1),
                        //         AddrTerm::single(s, 0, 1),
                        //         AddrTerm::single(s, 0, 1),
                        //         AddrTerm::single(s, 0, 1),
                        //         AddrTerm::single(s, 0, 1),
                        //     ];
                        //     let inputs = Inputs::unsorted({
                        //         let special_case = rm == 0b110 && self.md == ModVal::Mod00;
                        //         let mut inputs = match rm {
                        //             0b000 | 0b001 | 0b111 => vec![ ParLoc { loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Bx)), size: Size::qword() } ],
                        //             0b010 | 0b011 | 0b110 => if special_case {
                        //                 vec![]
                        //             } else {
                        //                 vec![ ParLoc { loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Bp)), size: Size::qword() } ]
                        //             },
                        //             _ => vec![],
                        //         };

                        //         match rm {
                        //             0b000 | 0b010 | 0b100 => inputs.push(ParLoc { loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Si)), size: Size::qword() }),
                        //             0b001 | 0b011 | 0b101 => inputs.push(ParLoc { loc: UnsizedParLoc::Reg(Reg::Gp(GpReg::Di)), size: Size::qword() }),
                        //             _ => (),
                        //         }

                        //         if (self.md == ModVal::Mod01 || self.md == ModVal::Mod10) || special_case {
                        //             inputs.push(self.add_disp(&mut ctx));
                        //         }

                        //         inputs.insert(0, Self::determine_segment(&mut ctx, &inputs, false));
                        //         inputs
                        //     });

                        //     let size = self.determine_mem_size(&ctx);
                        //     let mem = ctx.add_access(MemoryAccess {
                        //         kind: AccessKind::InputOutput,
                        //         size: MemorySizeRange::new(size as u64, size as u64),
                        //         calculation: ParameterizedComputation::Calculation(
                        //             AddressComputation::from_iter(terms[..inputs.len()].iter().cloned(), 0)
                        //                 .with_addr_size(ctx.memory_reg_and_addr_size().1)
                        //         ),
                        //         alignment: 1,
                        //         inputs,
                        //     });

                        //     next(ctx, Val::Loc(mem))
                        // })
                    },
                    _ => unreachable!(),
                };
            },
            ModVal::Mod11 => {
                if ctx.is_wide_op() {
                    let mut ctx = ctx;
                    let part = ctx.add_part(Part {
                        size: 3,
                        value: 0,
                        mapping: PartMapping::Register {
                            mapping: vec![
                                Some(Reg::Gp(GpReg::Ax)),
                                Some(Reg::Gp(GpReg::Cx)),
                                Some(Reg::Gp(GpReg::Dx)),
                                Some(Reg::Gp(GpReg::Bx)),
                                Some(Reg::Gp(GpReg::Sp)),
                                Some(Reg::Gp(GpReg::Bp)),
                                Some(Reg::Gp(GpReg::Si)),
                                Some(Reg::Gp(GpReg::Di)),
                            ],
                        },
                    });

                    let size = self.determine_reg_size(&ctx);
                    next(
                        ctx,
                        Val::Loc(ParLoc {
                            loc: UnsizedParLoc::Part(part),
                            size: Size::from_bytes(size),
                        }),
                    )
                } else {
                    ExpandedBit::new().build(ctx, &mut |mut ctx, first_bit| {
                        let part = ctx.add_part(Part {
                            size: 2,
                            value: 0,
                            mapping: PartMapping::Register {
                                mapping: vec![
                                    Some(Reg::Gp(GpReg::Ax)),
                                    Some(Reg::Gp(GpReg::Cx)),
                                    Some(Reg::Gp(GpReg::Dx)),
                                    Some(Reg::Gp(GpReg::Bx)),
                                ],
                            },
                        });

                        next(
                            ctx,
                            Val::Loc(ParLoc {
                                loc: UnsizedParLoc::Part(part),
                                size: if first_bit { Size::new(1, 1) } else { Size::new(0, 0) },
                            }),
                        )
                    })
                }
            },
        }
    }
}

#[derive(Copy, Clone, Default)]
pub struct ModNRm(u8);

impl ModNRm {
    pub fn new(val: u8) -> Self {
        assert_eq!(val & !0x7, 0, "only the lower 3 bits can be used");
        Self(val)
    }
}

impl Builder for ModNRm {
    type Output = V;

    fn build(&self, ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        Mod::new().build(ctx, &mut |mut ctx, md| {
            ctx.add_bit(Bit::Fixed((self.0 >> 2) & 1));
            ctx.add_bit(Bit::Fixed((self.0 >> 1) & 1));
            ctx.add_bit(Bit::Fixed(self.0 & 1));
            Rm::new(md).build(ctx, &mut |ctx, rm| next(ctx, rm))
        })
    }
}

#[derive(Copy, Clone, Default)]
pub struct ModNMemRm(u8);

impl ModNMemRm {
    pub fn new(val: u8) -> Self {
        assert_eq!(val & !0x7, 0, "only the lower 3 bits can be used");
        Self(val)
    }
}

impl Builder for ModNMemRm {
    type Output = V;

    fn build(&self, ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        Mod::new().build(ctx, &mut |mut ctx, md| {
            if md != ModVal::Mod11 {
                ctx.add_bit(Bit::Fixed((self.0 >> 2) & 1));
                ctx.add_bit(Bit::Fixed((self.0 >> 1) & 1));
                ctx.add_bit(Bit::Fixed(self.0 & 1));
                Rm::new(md).build(ctx, &mut |ctx, rm| next(ctx, rm))
            }
        })
    }
}

#[derive(Copy, Clone, Default)]
pub struct ModRm;

impl ModRm {
    pub fn new() -> Self {
        Self
    }
}

impl Builder for ModRm {
    type Output = (V, V);

    fn build(&self, ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        Mod::new().build(ctx, &mut |ctx, md| {
            RegBits::new().build(ctx, &mut |ctx, reg| {
                Rm::new(md).build(ctx, &mut |ctx, rm| next(ctx, (reg, rm)))
            })
        })
    }
}

pub struct Filter<F>(F);

impl<F> Filter<F> {
    pub fn new(f: F) -> Self {
        Self(f)
    }
}

impl<F: Fn(&Context) -> bool> Builder for Filter<F> {
    type Output = ();

    fn build(&self, ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        if (self.0)(&ctx) {
            next(ctx, ())
        }
    }
}

#[non_exhaustive]
pub struct SemSpec<A: Arch> {
    pub manual_memory_accesses: bool,
    pub commands: Vec<Cmd<A>>,
    pub jump: Jump<A>,
}

impl<A: Arch> Default for SemSpec<A> {
    fn default() -> Self {
        Self {
            commands: Vec::new(),
            manual_memory_accesses: false,
            jump: Jump::Sequential,
        }
    }
}

#[macro_export]
macro_rules! encoding {
    ($($everything:tt)*) => {
        Box::new($crate::encoding_internal! { $($everything)* }) as Box<dyn $crate::builder::Builder<Output = SemSpec<Intel386>>>
    };
}

#[macro_export]
macro_rules! encoding_internal {
    (0 $($rest:tt)*) => { $crate::encoding_internal! { $crate::builder::FixedBit::<0> $($rest)* } };
    (1 $($rest:tt)*) => { $crate::encoding_internal! { $crate::builder::FixedBit::<1> $($rest)* } };
    (#$lit:literal $($rest:tt)*) => { $crate::encoding_internal! { $crate::builder::Byte::<$lit> $($rest)* } };
    ($ty:ty $({ $($arg:expr),* $(,)* })? = $pat:pat $(,)+ $($rest:tt)*) => {
        $crate::builder::Chain::new(<$ty>::new($($($arg),*)?), move |$pat: <$ty as $crate::builder::Builder>::Output| {
            $crate::encoding_internal! { $($rest)* }
        })
    };
    ($ty:ty $({ $($arg:expr),* $(,)* })? $(,)+ $($rest:tt)*) => {
        $crate::builder::Chain::new(<$ty>::new($($($arg),*)?), move |_: <$ty as $crate::builder::Builder>::Output| {
            $crate::encoding_internal! { $($rest)* }
        })
    };
    (ops! { #[context($ctx:ident)] $($inner:tt)* }) => {
        BuildFromContext::new(move |$ctx| $crate::ops! {
            #[context($ctx)]

            $($inner)*
        })
    };
    (ops! { $($inner:tt)* }) => {
        BuildFromContext::new(move |ctx| $crate::ops! {
            #[context(ctx)]

            $($inner)*
        })
    };
    ({ $($inner:tt)* }) => {
        $($inner)*
    };
}

#[macro_export]
macro_rules! encoding_group {
    ($([ $($spec:tt)* ] = $e:expr),* $(,)+ map $f:expr) => {
        Box::new([
            $(
                encoding!( $($spec)* { ($f($e)) } ),
            )*
        ]) as Box<dyn $crate::builder::Builder<Output = SemSpec<Intel386>>>
    };
}

impl<T, const N: usize> Builder for [Box<dyn Builder<Output = T>>; N] {
    type Output = T;

    fn build(&self, ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        for item in self.iter() {
            item.build(ctx.clone(), next);
        }
    }
}
