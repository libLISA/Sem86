use std::ffi::c_void;

use log::{debug, info};
use num_traits::FromPrimitive;
use sem86_arch::exceptions::Exception;
use strum::{EnumCount, VariantArray};

use crate::arch::intel386::{Intel386, State};
use crate::emulator::exec::{DescriptorReadResult, ExecutionContext};
use crate::il::{ExecResult, FpBinOp, FpUnOp, MemArea};

#[repr(C)]
struct PortInResult {
    ok: bool,
    result: u64,
}

#[repr(C)]
struct ReadDescriptorStructResult {
    // ok, descriptor_ok, [2x reserved u8], base
    lo: u64,

    // ar, limit
    hi: u64,
}

impl ReadDescriptorStructResult {
    const NOT_OK: ReadDescriptorStructResult = ReadDescriptorStructResult {
        lo: 0,
        hi: 0,
    };
}

impl From<DescriptorReadResult> for ReadDescriptorStructResult {
    fn from(value: DescriptorReadResult) -> Self {
        Self {
            lo: 0x0100_0000_0000_0000 | (value.ok as u64) << 48 | (value.base as u32 as u64),
            hi: (value.access_rights << 32) | (value.limit as u32 as u64),
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub enum Primitive {
    U8,
    U16,
    U32,
    U64,
    U128,
    Ptr { mutable: bool },
}

#[derive(Clone, Debug)]
pub enum Ty {
    Primitive(Primitive),
    Struct { fields: Vec<Primitive> },
}

#[derive(Copy, Clone, Debug, strum::EnumCount, strum::VariantArray, strum::FromRepr, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(usize)]
pub enum FunctionName {
    PrintPartValue,
    PrintInstr,
    PortOut,
    PortIn,
    CreateHandlerResult,
    CreateExceptionResult,
    ReadDescriptor,
    ReadDescriptorStruct,
    MemRead16,
    MemRead32,
    MemReadSimple,
    MemRead1Simple,
    MemRead2Simple,
    MemRead4Simple,
    MemWrite16,
    MemWrite32,
    MemWriteSimple,
    MemWrite2Simple,
    MemWrite4Simple,
    Log,
    F32ToF80,
    F64ToF80,
    F80ToF32,
    F80ToF64,
    F80ToF32IsPrecise,
    F80ToF64IsPrecise,
    I64ToF80,
    F80ToI64,
    F80Add,
    F80Sub,
    F80Mul,
    F80Div,
    F80Rem,
    F80CmpLt,
    F80CmpEq,
    RoundToIntF80,
    RoundF80ToF32,
    RoundF80ToF64,
    SinF80,
    CosF80,
    TanF80,
    SqrtF80,
    F2Xm1F80,
    Log2F80,
    F80Scale,
    ArcTanF80,
}

impl From<FpUnOp> for FunctionName {
    fn from(value: FpUnOp) -> Self {
        match value {
            FpUnOp::F32ToF80 => FunctionName::F32ToF80,
            FpUnOp::F64ToF80 => FunctionName::F64ToF80,
            FpUnOp::F80ToF64 => FunctionName::F80ToF64,
            FpUnOp::F80ToF32 => FunctionName::F80ToF32,
            FpUnOp::F80ToF64IsPrecise => FunctionName::F80ToF64IsPrecise,
            FpUnOp::F80ToF32IsPrecise => FunctionName::F80ToF32IsPrecise,
            FpUnOp::I64ToF32 => unimplemented!(),
            FpUnOp::I64ToF64 => unimplemented!(),
            FpUnOp::I64ToF80 => FunctionName::I64ToF80,
            FpUnOp::F80ToI64 => FunctionName::F80ToI64,
            FpUnOp::SinF80 => FunctionName::SinF80,
            FpUnOp::CosF80 => FunctionName::CosF80,
            FpUnOp::TanF80 => FunctionName::TanF80,
            FpUnOp::RoundToIntF80 => FunctionName::RoundToIntF80,
            FpUnOp::RoundF80ToF32 => FunctionName::RoundF80ToF32,
            FpUnOp::RoundF80ToF64 => FunctionName::RoundF80ToF64,
            FpUnOp::SqrtF80 => FunctionName::SqrtF80,
            FpUnOp::F2Xm1F80 => FunctionName::F2Xm1F80,
            FpUnOp::Log2F80 => FunctionName::Log2F80,
            FpUnOp::ArcTanF80 => FunctionName::ArcTanF80,
        }
    }
}

impl From<FpBinOp> for FunctionName {
    fn from(value: FpBinOp) -> Self {
        match value {
            FpBinOp::F80Add => FunctionName::F80Add,
            FpBinOp::F80Sub => FunctionName::F80Sub,
            FpBinOp::F80Mul => FunctionName::F80Mul,
            FpBinOp::F80Div => FunctionName::F80Div,
            FpBinOp::F80Rem => FunctionName::F80Rem,
            FpBinOp::F80CmpLt => FunctionName::F80CmpLt,
            FpBinOp::F80CmpEq => FunctionName::F80CmpEq,
            FpBinOp::F80Scale => FunctionName::F80Scale,
        }
    }
}

struct FunctionData {
    symbol: &'static str,
    pointer: *const u8,
    params: Vec<Ty>,
    returns: Option<Ty>,
}

impl FunctionData {
    fn from_fn(symbol: &'static str, f: impl ExternalFunctionParams + ExternalFunctionReturns + FunctionPointer) -> Self {
        Self {
            symbol,
            params: f.params(),
            returns: f.returns(),
            pointer: f.as_ptr(),
        }
    }
}

trait Param {
    fn ty() -> Option<Ty>;
}

impl Param for () {
    fn ty() -> Option<Ty> {
        None
    }
}

impl Param for u128 {
    fn ty() -> Option<Ty> {
        Some(Ty::Primitive(Primitive::U128))
    }
}

impl Param for u64 {
    fn ty() -> Option<Ty> {
        Some(Ty::Primitive(Primitive::U64))
    }
}

impl Param for u32 {
    fn ty() -> Option<Ty> {
        Some(Ty::Primitive(Primitive::U32))
    }
}

impl Param for u16 {
    fn ty() -> Option<Ty> {
        Some(Ty::Primitive(Primitive::U16))
    }
}

impl Param for u8 {
    fn ty() -> Option<Ty> {
        Some(Ty::Primitive(Primitive::U8))
    }
}

impl Param for bool {
    fn ty() -> Option<Ty> {
        Some(Ty::Primitive(Primitive::U8))
    }
}

impl<T> Param for *const T {
    fn ty() -> Option<Ty> {
        Some(Ty::Primitive(Primitive::Ptr {
            mutable: false,
        }))
    }
}

impl<T> Param for &mut T {
    fn ty() -> Option<Ty> {
        Some(Ty::Primitive(Primitive::Ptr {
            mutable: true,
        }))
    }
}

impl<T> Param for &T {
    fn ty() -> Option<Ty> {
        Some(Ty::Primitive(Primitive::Ptr {
            mutable: false,
        }))
    }
}

impl Param for PortInResult {
    fn ty() -> Option<Ty> {
        Some(Ty::Struct {
            fields: vec![Primitive::U8, Primitive::U64],
        })
    }
}

impl Param for ReadDescriptorStructResult {
    fn ty() -> Option<Ty> {
        Some(Ty::Struct {
            fields: vec![Primitive::U64, Primitive::U64],
        })
    }
}

trait ExternalFunctionParams {
    fn params(&self) -> Vec<Ty>;
}

trait ExternalFunctionReturns {
    fn returns(&self) -> Option<Ty>;
}

trait FunctionPointer {
    fn as_ptr(self) -> *const u8;
}

macro_rules! impl_external_fn_traits {
    ($($t:ident),*) => {
        impl<$($t: Param),*, R: Param> ExternalFunctionParams for extern "C" fn ($($t,)*) -> R {
            fn params(&self) -> Vec<Ty> {
                [
                    $($t::ty(),)*
                ].into_iter().flatten().collect()
            }
        }

        impl<$($t: Param),*, R: Param> ExternalFunctionReturns for extern "C" fn ($($t,)*) -> R {
            fn returns(&self) -> Option<Ty> { R::ty() }
        }

        impl<$($t: Param),*, R: Param> FunctionPointer for extern "C" fn ($($t,)*) -> R {
            fn as_ptr(self) -> *const u8 { self as *const u8 }
        }
    }
}

impl_external_fn_traits!(A);
impl_external_fn_traits!(A, B);
impl_external_fn_traits!(A, B, C);
impl_external_fn_traits!(A, B, C, D);
impl_external_fn_traits!(A, B, C, D, E);
impl_external_fn_traits!(A, B, C, D, E, F);
impl_external_fn_traits!(A, B, C, D, E, F, G);
impl_external_fn_traits!(A, B, C, D, E, F, G, H);
impl_external_fn_traits!(A, B, C, D, E, F, G, H, I);
impl_external_fn_traits!(A, B, C, D, E, F, G, H, I, J);
impl_external_fn_traits!(A, B, C, D, E, F, G, H, I, J, K);

macro_rules! declare_fn {
    (fn $name:ident ($($arg:ident : $ty:ty),* $(,)*) $(-> $ret:ty)? { $($body:tt)* }) => {{
        extern "C" fn $name ($($arg: $ty),*) $(-> $ret)? {
            $($body)*
        }

        FunctionData::from_fn(stringify!($name), $name as extern "C" fn($( ${ignore($ty)} _),*) -> _)
    }}
}

impl FunctionName {
    fn data_internal(&self) -> FunctionData {
        use FunctionName::*;

        match self {
            PrintPartValue => declare_fn!(
                fn print_part_value(part_index: u64, part_value: u64) {
                    println!("part #{part_index} = 0x{part_value:X}");
                }
            ),
            PrintInstr => declare_fn!(
                fn print_instr(bytes: *const u8, len: u64) {
                    let bytes = unsafe { std::slice::from_raw_parts(bytes, len as usize) };
                    println!("base instruction bytes: {bytes:02X?}");
                }
            ),
            PortOut => declare_fn!(
                fn port_out(ctx: &mut ExecutionContext<'_, '_, Intel386>, cpu: &mut State, port: u16, len: u8, val: u32) -> bool {
                    match ctx.port_out(cpu, port, len, val) {
                        Ok(_) => true,
                        Err(err) => {
                            ctx.result = Err(err).into();
                            false
                        },
                    }
                }
            ),
            PortIn => declare_fn!(
                fn port_in(ctx: &mut ExecutionContext<'_, '_, Intel386>, cpu: &mut State, port: u16, len: u8) -> PortInResult {
                    assert!(
                        len == 1 || len == 2 || len == 4,
                        "unsupported length: {len} reading from port 0x{port:X}"
                    );
                    match ctx.port_in(cpu, port, len) {
                        Ok(val) => PortInResult {
                            ok: true,
                            result: val,
                        },
                        Err(err) => {
                            ctx.result = Err(err).into();
                            PortInResult {
                                ok: false,
                                result: 0,
                            }
                        },
                    }
                }
            ),
            CreateHandlerResult => declare_fn!(
                fn create_handler_result(
                    ctx: &mut ExecutionContext<'_, '_, Intel386>, id_num: u32, arg0: u64, arg1: u64, _arg2: u64, num_args: u32,
                ) {
                    let id = FromPrimitive::from_u32(id_num).expect("handler ID should be valid");
                    assert!(num_args <= 2, "handler {id:?} ({id_num}) invoked with more than two aruments");
                    ctx.result = Ok(ExecResult::InvokeHandler {
                        id,
                        args: [arg0 as u32, arg1 as u32],
                    })
                    .into()
                }
            ),
            CreateExceptionResult => declare_fn!(
                fn create_exception_result(ctx: &mut ExecutionContext<'_, '_, Intel386>, vector: u8, code: u32) {
                    ctx.result = Err(Exception::from_vector_and_params(vector, code, 0)).into()
                }
            ),
            ReadDescriptor => declare_fn!(
                fn read_descriptor(
                    ctx: &mut ExecutionContext<'_, '_, Intel386>, cpu: &mut State, selector_val: u16, force: bool,
                    mark_accessed: bool, descriptor_ok: &mut bool, base: &mut u64, limit: &mut u64, access_rights: &mut u64,
                ) -> bool {
                    match ctx.read_descriptor(cpu, force, mark_accessed, selector_val) {
                        Ok(result) => {
                            *descriptor_ok = result.ok;
                            *base = result.base;
                            *limit = result.limit;
                            *access_rights = result.access_rights;
                            true
                        },
                        Err(err) => {
                            ctx.result = Err(err).into();
                            false
                        },
                    }
                }
            ),
            ReadDescriptorStruct => declare_fn!(
                fn read_descriptor_struct(
                    ctx: &mut ExecutionContext<'_, '_, Intel386>, cpu: &mut State, selector_val: u16, force: bool,
                    mark_accessed: bool,
                ) -> ReadDescriptorStructResult {
                    match ctx.read_descriptor(cpu, force, mark_accessed, selector_val) {
                        Ok(result) => ReadDescriptorStructResult::from(result),
                        Err(err) => {
                            ctx.result = Err(err).into();
                            ReadDescriptorStructResult::NOT_OK
                        },
                    }
                }
            ),
            MemRead16 => declare_fn!(
                fn mem_read16(
                    ctx: &mut ExecutionContext<'_, '_, Intel386>, base: u32, offset: u32, len: u8, is_userspace: bool,
                    val_out: &mut u128,
                ) -> bool {
                    let area = MemArea::Real {
                        segment_offset: base,
                        addr: offset as u16,
                        len,
                    };
                    match area.read_from_mem_as_u128(
                        ctx.memory,
                        is_userspace,
                        &mut ctx.mmio_ctx.hw.mmio(&mut ctx.mmio_ctx.icache),
                    ) {
                        Ok(val) => {
                            // println!("Reading memory area {area:02X?} = 0x{val:X}");

                            *val_out = val;
                            true
                        },
                        Err(err) => {
                            ctx.result = Err(err).into();
                            false
                        },
                    }
                }
            ),
            MemRead32 => declare_fn!(
                fn mem_read32(
                    ctx: &mut ExecutionContext<'_, '_, Intel386>, base: u32, offset: u32, len: u8, is_userspace: bool,
                    val_out: &mut u128,
                ) -> bool {
                    let area = MemArea::Protected {
                        addr: base.wrapping_add(offset),
                        len,
                    };
                    match area.read_from_mem_as_u128(
                        ctx.memory,
                        is_userspace,
                        &mut ctx.mmio_ctx.hw.mmio(&mut ctx.mmio_ctx.icache),
                    ) {
                        Ok(val) => {
                            // println!("Reading memory area {area:02X?} = 0x{val:X}");
                            *val_out = val;
                            true
                        },
                        Err(err) => {
                            ctx.result = Err(err).into();
                            false
                        },
                    }
                }
            ),
            MemReadSimple => declare_fn!(
                fn mem_read_simple(
                    ctx: &mut ExecutionContext<'_, '_, Intel386>, addr: u32, len: u8, is_userspace: bool, val_out: &mut u128,
                ) -> bool {
                    let area = MemArea::Protected {
                        addr: addr as u32,
                        len,
                    };
                    match area.read_from_mem_as_u128(
                        ctx.memory,
                        is_userspace,
                        &mut ctx.mmio_ctx.hw.mmio(&mut ctx.mmio_ctx.icache),
                    ) {
                        Ok(val) => {
                            *val_out = val;
                            return true
                        },
                        Err(err) => {
                            ctx.result = Err(err).into();
                            return false
                        },
                    }
                }
            ),
            MemRead1Simple => declare_fn!(
                fn mem_read1_simple(ctx: &mut ExecutionContext<'_, '_, Intel386>, addr: u32, is_userspace: bool) -> u16 {
                    match ctx.fast_memory.read::<u8>(addr as u32, is_userspace, &mut ctx.mmio_ctx) {
                        Ok(val) => return val as u16 | 0x100,
                        Err(err) => {
                            ctx.result = Err(err).into();
                            return 0
                        },
                    }
                }
            ),
            MemRead2Simple => declare_fn!(
                fn mem_read2_simple(ctx: &mut ExecutionContext<'_, '_, Intel386>, addr: u32, is_userspace: bool) -> u32 {
                    match ctx.fast_memory.read::<u16>(addr as u32, is_userspace, &mut ctx.mmio_ctx) {
                        Ok(val) => return val as u32 | 0x1_0000,
                        Err(err) => {
                            ctx.result = Err(err).into();
                            return 0
                        },
                    }
                }
            ),
            MemRead4Simple => declare_fn!(
                fn mem_read4_simple(ctx: &mut ExecutionContext<'_, '_, Intel386>, addr: u32, is_userspace: bool) -> u64 {
                    match ctx.fast_memory.read::<u32>(addr as u32, is_userspace, &mut ctx.mmio_ctx) {
                        Ok(val) => return val as u64 | (1 << 32),
                        Err(err) => {
                            ctx.result = Err(err).into();
                            return 0
                        },
                    }
                }
            ),
            MemWrite16 => declare_fn!(
                fn mem_write16(
                    ctx: &mut ExecutionContext<'_, '_, Intel386>, base: u32, offset: u32, len: u8, is_userspace: bool, val: u128,
                ) -> bool {
                    let area = MemArea::Real {
                        segment_offset: base,
                        addr: offset as u16,
                        len,
                    };
                    // println!("Writing memory area {area:02X?} = 0x{val:X}");

                    match area.write_u128_to_mem(
                        ctx.memory,
                        is_userspace,
                        &mut ctx.mmio_ctx.hw.mmio(&mut ctx.mmio_ctx.icache),
                        val,
                    ) {
                        Ok(_) => true,
                        Err(err) => {
                            ctx.result = Err(err).into();

                            false
                        },
                    }
                }
            ),
            MemWrite32 => declare_fn!(
                fn mem_write32(
                    ctx: &mut ExecutionContext<'_, '_, Intel386>, base: u32, offset: u32, len: u8, is_userspace: bool, val: u128,
                ) -> bool {
                    let area = MemArea::Protected {
                        addr: base.wrapping_add(offset),
                        len,
                    };
                    // println!("Writing memory area {area:02X?} = 0x{val:X}");

                    match area.write_u128_to_mem(
                        ctx.memory,
                        is_userspace,
                        &mut ctx.mmio_ctx.hw.mmio(&mut ctx.mmio_ctx.icache),
                        val,
                    ) {
                        Ok(_) => true,
                        Err(err) => {
                            ctx.result = Err(err).into();

                            false
                        },
                    }
                }
            ),
            MemWriteSimple => declare_fn!(
                fn mem_write_simple(
                    ctx: &mut ExecutionContext<'_, '_, Intel386>, addr: u32, len: u8, is_userspace: bool, val: u128,
                ) -> bool {
                    let area = MemArea::Protected {
                        addr,
                        len,
                    };
                    // println!("Writing memory area {area:02X?} = 0x{val:X}");

                    match area.write_u128_to_mem(
                        ctx.memory,
                        is_userspace,
                        &mut ctx.mmio_ctx.hw.mmio(&mut ctx.mmio_ctx.icache),
                        val,
                    ) {
                        Ok(_) => true,
                        Err(err) => {
                            ctx.result = Err(err).into();

                            false
                        },
                    }
                }
            ),
            MemWrite2Simple => declare_fn!(
                fn mem_write2_simple(
                    ctx: &mut ExecutionContext<'_, '_, Intel386>, addr: u32, is_userspace: bool, val: u16,
                ) -> bool {
                    match ctx.fast_memory.write(addr, is_userspace, val, &mut ctx.mmio_ctx) {
                        Ok(_) => true,
                        Err(err) => {
                            ctx.result = Err(err).into();

                            false
                        },
                    }
                }
            ),
            MemWrite4Simple => declare_fn!(
                fn mem_write4_simple(
                    ctx: &mut ExecutionContext<'_, '_, Intel386>, addr: u32, is_userspace: bool, val: u32,
                ) -> bool {
                    match ctx.fast_memory.write(addr, is_userspace, val, &mut ctx.mmio_ctx) {
                        Ok(_) => true,
                        Err(err) => {
                            ctx.result = Err(err).into();

                            false
                        },
                    }
                }
            ),
            Log => declare_fn!(
                fn instr_log(message: *const u8, len: u32) {
                    let slice = unsafe { std::slice::from_raw_parts(message, len as usize) };
                    let s = std::str::from_utf8(slice).unwrap();
                    info!(target: extend_path_with!("log"), "{s}");
                }
            ),
            F32ToF80 => declare_fn!(
                fn f32_to_f80(val: u128, rc: u8) -> u128 {
                    let result = FpUnOp::F32ToF80.execute(val, rc);
                    debug!(target: extend_path_with!("x87"), "Converted f32 0x{val:X} ({:?}) to f80 0x{result:X}", f32::from_bits(val as u32));
                    result
                }
            ),
            F64ToF80 => declare_fn!(
                fn f64_to_f80(val: u128, rc: u8) -> u128 {
                    let result = FpUnOp::F64ToF80.execute(val, rc);
                    debug!(target: extend_path_with!("x87"), "Converted f64 0x{val:X} ({:?}) to f80 0x{result:X}", f64::from_bits(val as u64));
                    result
                }
            ),
            F80ToF32 => declare_fn!(
                fn f80_to_f32(val: u128, rc: u8) -> u128 {
                    let result = FpUnOp::F80ToF32.execute(val, rc);
                    debug!(target: extend_path_with!("x87"), "Converted f80 0x{val:X} to f32 0x{result:X}");
                    result
                }
            ),
            F80ToF64 => declare_fn!(
                fn f80_to_f64(val: u128, rc: u8) -> u128 {
                    let result = FpUnOp::F80ToF64.execute(val, rc);
                    debug!(target: extend_path_with!("x87"), "Converted f80 0x{val:X} to f64 0x{result:X}");
                    result
                }
            ),
            F80ToF32IsPrecise => declare_fn!(
                fn f80_to_f32_is_precise(val: u128, rc: u8) -> u128 {
                    let result = FpUnOp::F80ToF32IsPrecise.execute(val, rc);
                    debug!(target: extend_path_with!("x87"), "Converted f80 0x{val:X} to f32 is precise = {result}");
                    result
                }
            ),
            F80ToF64IsPrecise => declare_fn!(
                fn f80_to_f64_is_precise(val: u128, rc: u8) -> u128 {
                    let result = FpUnOp::F80ToF64IsPrecise.execute(val, rc);
                    debug!(target: extend_path_with!("x87"), "Converted f80 0x{val:X} to f64 is precise = {result}");
                    result
                }
            ),
            I64ToF80 => declare_fn!(
                fn i64_to_f80(val: u128, rc: u8) -> u128 {
                    let result = FpUnOp::I64ToF80.execute(val, rc);
                    debug!(target: extend_path_with!("x87"), "Converted i64 0x{val:X} to f80 0x{result:X}");
                    result
                }
            ),
            F80ToI64 => declare_fn!(
                fn f80_to_i64(val: u128, rc: u8) -> u128 {
                    let result = FpUnOp::F80ToI64.execute(val, rc);
                    debug!(target: extend_path_with!("x87"), "Converted f80 0x{val:X} to i64 0x{result:X}");
                    result
                }
            ),
            F80Add => declare_fn!(
                fn f80_add(lhs: u128, rhs: u128, rc: u8) -> u128 {
                    let result = FpBinOp::F80Add.execute(lhs, rhs, rc);
                    debug!(target: extend_path_with!("x87"), "Added f80s 0x{lhs:X} and 0x{rhs:X}, giving 0x{result:X}");
                    result
                }
            ),
            F80Sub => declare_fn!(
                fn f80_sub(lhs: u128, rhs: u128, rc: u8) -> u128 {
                    let result = FpBinOp::F80Sub.execute(lhs, rhs, rc);
                    debug!(target: extend_path_with!("x87"), "Subtracted f80s 0x{lhs:X} and 0x{rhs:X}, giving 0x{result:X}");
                    result
                }
            ),
            F80Mul => declare_fn!(
                fn f80_mul(lhs: u128, rhs: u128, rc: u8) -> u128 {
                    let result = FpBinOp::F80Mul.execute(lhs, rhs, rc);
                    debug!(target: extend_path_with!("x87"), "Multiplied f80s 0x{lhs:X} and 0x{rhs:X}, giving 0x{result:X}");
                    result
                }
            ),
            F80Div => declare_fn!(
                fn f80_div(lhs: u128, rhs: u128, rc: u8) -> u128 {
                    let result = FpBinOp::F80Div.execute(lhs, rhs, rc);
                    debug!(target: extend_path_with!("x87"), "Divided f80s 0x{lhs:X} and 0x{rhs:X}, giving 0x{result:X}");
                    result
                }
            ),
            F80Rem => declare_fn!(
                fn f80_rem(lhs: u128, rhs: u128, rc: u8) -> u128 {
                    let result = FpBinOp::F80Rem.execute(lhs, rhs, rc);
                    debug!(target: extend_path_with!("x87"), "Computed remainder of dividing f80s 0x{lhs:X} and 0x{rhs:X}, giving 0x{result:X}");
                    result
                }
            ),
            F80CmpLt => declare_fn!(
                fn f80_cmp_lt(lhs: u128, rhs: u128, rc: u8) -> u128 {
                    FpBinOp::F80CmpLt.execute(lhs, rhs, rc)
                }
            ),
            F80CmpEq => declare_fn!(
                fn f80_cmp_eq(lhs: u128, rhs: u128, rc: u8) -> u128 {
                    FpBinOp::F80CmpEq.execute(lhs, rhs, rc)
                }
            ),
            RoundToIntF80 => declare_fn!(
                fn round_to_int_f80(val: u128, rc: u8) -> u128 {
                    let result = FpUnOp::RoundToIntF80.execute(val, rc);
                    debug!(target: extend_path_with!("x87"), "Rounded 0x{val:X} to integer, giving 0x{result:X}");
                    result
                }
            ),
            RoundF80ToF32 => declare_fn!(
                fn round_f80_to_f32(val: u128, rc: u8) -> u128 {
                    let result = FpUnOp::RoundF80ToF32.execute(val, rc);
                    debug!(target: extend_path_with!("x87"), "Rounded 0x{val:X} to f32 precision, giving 0x{result:X}");
                    result
                }
            ),
            RoundF80ToF64 => declare_fn!(
                fn round_f80_to_f64(val: u128, rc: u8) -> u128 {
                    let result = FpUnOp::RoundF80ToF64.execute(val, rc);
                    debug!(target: extend_path_with!("x87"), "Rounded 0x{val:X} to f64 precision, giving 0x{result:X}");
                    result
                }
            ),
            SinF80 => declare_fn!(
                fn sin_f80(val: u128, rc: u8) -> u128 {
                    let result = FpUnOp::SinF80.execute(val, rc);
                    debug!(target: extend_path_with!("x87"), "Computed sine of 0x{val:X}, giving 0x{result:X}");
                    result
                }
            ),
            CosF80 => declare_fn!(
                fn cos_f80(val: u128, rc: u8) -> u128 {
                    let result = FpUnOp::CosF80.execute(val, rc);
                    debug!(target: extend_path_with!("x87"), "Computed cosine of 0x{val:X}, giving 0x{result:X}");
                    result
                }
            ),
            TanF80 => declare_fn!(
                fn tan_f80(val: u128, rc: u8) -> u128 {
                    let result = FpUnOp::TanF80.execute(val, rc);
                    debug!(target: extend_path_with!("x87"), "Computed tan of 0x{val:X}, giving 0x{result:X}");
                    result
                }
            ),
            SqrtF80 => declare_fn!(
                fn sqrt_f80(val: u128, rc: u8) -> u128 {
                    let result = FpUnOp::SqrtF80.execute(val, rc);
                    debug!(target: extend_path_with!("x87"), "Computed square root of 0x{val:X}, giving 0x{result:X}");
                    result
                }
            ),
            F2Xm1F80 => declare_fn!(
                fn f2xm1_f80(val: u128, rc: u8) -> u128 {
                    let result = FpUnOp::F2Xm1F80.execute(val, rc);
                    debug!(target: extend_path_with!("x87"), "Computed 2**x-1 for 0x{val:X}, giving 0x{result:X}");
                    result
                }
            ),
            Log2F80 => declare_fn!(
                fn log2_f80(val: u128, rc: u8) -> u128 {
                    let result = FpUnOp::Log2F80.execute(val, rc);
                    debug!(target: extend_path_with!("x87"), "Computed 2**x-1 for 0x{val:X}, giving 0x{result:X}");
                    result
                }
            ),
            F80Scale => declare_fn!(
                fn f80_scale(x: u128, y: u128, rc: u8) -> u128 {
                    let result = FpBinOp::F80Scale.execute(x, y, rc);
                    debug!(target: extend_path_with!("x87"), "Scaled 0x{x:X} by 0x{y:X}, giving 0x{result:X}");
                    result
                }
            ),
            ArcTanF80 => declare_fn!(
                fn arctan_f80(val: u128, rc: u8) -> u128 {
                    let result = FpUnOp::ArcTanF80.execute(val, rc);
                    debug!(target: extend_path_with!("x87"), "Computed atan of 0x{val:X}, giving 0x{result:X}");
                    result
                }
            ),
        }
    }

    fn symbol(&self) -> &'static str {
        self.data_internal().symbol
    }

    fn pointer(&self) -> *const u8 {
        self.data_internal().pointer
    }

    fn params_returns(&self) -> (Vec<Ty>, Option<Ty>) {
        let d = self.data_internal();
        (d.params, d.returns)
    }
}

pub trait FnDeclBackend {
    type FuncId;
    type Output;

    fn declare_function(&mut self, symbol: &str, ptr: *const c_void, params: &[Ty], returns: Option<Ty>) -> Self::FuncId;

    fn postprocess(&mut self, id: Self::FuncId) -> Self::Output;
}

pub struct FunctionTable<ID> {
    items: [Option<ID>; FunctionName::COUNT],
}

impl<ID> Default for FunctionTable<ID>
where
    ID: Copy,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<ID> FunctionTable<ID> {
    pub fn new() -> Self
    where
        ID: Copy,
    {
        Self {
            items: [None; _],
        }
    }

    pub fn symbols_and_ptrs() -> impl Iterator<Item = (&'static str, *const c_void)> {
        FunctionName::VARIANTS
            .iter()
            .map(|name| (name.symbol(), name.pointer() as *const _))
    }

    pub fn get<B: FnDeclBackend<FuncId = ID>>(&mut self, builder: &mut B, name: FunctionName) -> B::Output
    where
        ID: Copy,
    {
        let id = *self.items[name as usize].get_or_insert_with(|| {
            let (params, returns) = name.params_returns();
            builder.declare_function(name.symbol(), name.pointer() as *const c_void, &params, returns)
        });

        builder.postprocess(id)
    }

    pub fn pointers() -> [*const c_void; FunctionName::COUNT] {
        std::array::from_fn(|n| FunctionName::VARIANTS[n].pointer() as *const _)
    }
}
