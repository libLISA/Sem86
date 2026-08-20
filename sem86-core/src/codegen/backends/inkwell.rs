use std::collections::HashMap;
use std::error::Error;
use std::fmt::{Debug, Display};
use std::mem::offset_of;
use std::os::raw::c_void;
use std::sync::OnceLock;

use arrayvec::ArrayVec;
use inkwell::attributes::{Attribute, AttributeLoc};
use inkwell::basic_block::BasicBlock;
use inkwell::builder::Builder;
use inkwell::context::{Context, ContextRef};
use inkwell::intrinsics::Intrinsic;
use inkwell::module::{Linkage, Module};
use inkwell::targets::{FileType, InitializationConfig, Target, TargetMachine};
use inkwell::types::{BasicMetadataTypeEnum, BasicType, BasicTypeEnum, IntType};
use inkwell::values::{
    BasicMetadataValueEnum, BasicValue, BasicValueEnum, FunctionValue, IntValue, LLVMTailCallKind, PointerValue,
};
use inkwell::{AddressSpace, AtomicOrdering, IntPredicate};
use itertools::Itertools;
use liblisa::utils::bitmask_u128;
use log::{Level, info, log_enabled};
use sem86_arch::mem::METADATA_SIZE;
use sem86_arch::mem::metadata::MetadataTest;

use crate::arch::intel386::{GpReg, Intel386Flag, Reg, State};
use crate::codegen::backends::{
    Backend, BackendFn, JitExecutionResult, LirBlock, NextOnPage, TracedAccess, UninstantiatedBackendFn,
};
use crate::codegen::components::StronglyConnectedComponents;
use crate::codegen::functions::{FnDeclBackend, FunctionName, FunctionTable, Primitive, Ty};
use crate::codegen::lir::{BlockId, Jump, Lir, LirOp};
use crate::codegen::mm::Object;
use crate::codegen::{DataSize, Ptr};
use crate::emulator::Emulator;
use crate::il::part_values::PartValues;
use crate::il::{BinOp, PackedExecResult, UnOp};

type JitFn = fn(emulator: &mut Emulator<'_, '_>) -> u64;
type UninstantiatedJitFn = fn(emulator: &mut Emulator<'_, '_>, instr_len: u8, part_values: PartValues) -> u64;

#[derive(Copy, Clone)]
pub struct InkwellFunction {
    f: JitFn,
}

unsafe impl Send for InkwellFunction {}
unsafe impl Sync for InkwellFunction {}

impl Debug for InkwellFunction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CraneliftFunction").finish()
    }
}

impl BackendFn for InkwellFunction {
    #[inline(always)]
    fn execute(&self, emulator: &mut Emulator<'_, '_>, _trace_memory: impl FnMut(TracedAccess)) -> (JitExecutionResult, u64) {
        let result = (self.f)(emulator);

        (JitExecutionResult::from(result as u8), result >> 8)
    }

    fn as_fptr(&self) -> fn(&mut Emulator) -> u64 {
        self.f
    }

    unsafe fn from_ptr(ptr: *mut u8) -> Self {
        Self {
            f: unsafe { std::mem::transmute(ptr) },
        }
    }
}

impl Default for InkwellFunction {
    fn default() -> Self {
        fn empty(_: &mut Emulator<'_, '_>) -> u64 {
            1
        }

        Self {
            f: empty,
        }
    }
}

#[derive(Copy, Clone)]
pub struct UninstantiatedInkwellFunction {
    f: UninstantiatedJitFn,
}

unsafe impl Send for UninstantiatedInkwellFunction {}
unsafe impl Sync for UninstantiatedInkwellFunction {}

impl Debug for UninstantiatedInkwellFunction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CraneliftFunction").finish()
    }
}

impl UninstantiatedBackendFn for UninstantiatedInkwellFunction {
    #[inline(always)]
    fn execute_uninstantiated(
        &self, emulator: &mut Emulator<'_, '_>, instr_len: u8, part_values: PartValues, _trace_memory: impl FnMut(TracedAccess),
    ) -> JitExecutionResult {
        JitExecutionResult::from((self.f)(emulator, instr_len, part_values) as u8)
    }

    fn as_fptr(&self) -> fn(&mut Emulator, u8, PartValues) -> u64 {
        self.f
    }

    unsafe fn from_ptr(ptr: *mut u8) -> Self {
        Self {
            f: unsafe { std::mem::transmute(ptr) },
        }
    }
}

impl Default for UninstantiatedInkwellFunction {
    fn default() -> Self {
        fn empty(_: &mut Emulator<'_, '_>, _: u8, _: PartValues) -> u64 {
            1
        }

        Self {
            f: empty,
        }
    }
}

#[derive(Debug)]
pub enum InkwellError {
    Generic,
}

impl Display for InkwellError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InkwellError::Generic => write!(f, "function is too big"),
        }
    }
}

impl Error for InkwellError {}

impl<'ctx> FnDeclBackend for (&mut Module<'ctx>, &'ctx Context) {
    type FuncId = FunctionValue<'ctx>;
    type Output = FunctionValue<'ctx>;

    fn declare_function(&mut self, symbol: &str, _ptr: *const c_void, params: &[Ty], returns: Option<Ty>) -> Self::FuncId {
        let ctx = self.0.get_context();
        fn map_primitive(ctx: ContextRef<'_>, primitive: Primitive) -> BasicTypeEnum<'_> {
            match primitive {
                Primitive::U8 => ctx.i8_type().into(),
                Primitive::U16 => ctx.i16_type().into(),
                Primitive::U32 => ctx.i32_type().into(),
                Primitive::U64 => ctx.i64_type().into(),
                Primitive::U128 => ctx.i128_type().into(),
                Primitive::Ptr {
                    ..
                } => ctx.ptr_type(AddressSpace::default()).into(),
            }
        }

        let parameters = params
            .iter()
            .map(|ty| match ty {
                Ty::Primitive(primitive) => map_primitive(ctx, *primitive).into(),
                Ty::Struct {
                    ..
                } => todo!(),
            })
            .collect::<Vec<BasicMetadataTypeEnum>>();

        let fn_type = returns
            .map(|ty| match ty {
                Ty::Primitive(primitive) => map_primitive(ctx, primitive).fn_type(&parameters, false),
                Ty::Struct {
                    fields,
                } => ctx
                    .struct_type(
                        &fields
                            .iter()
                            .map(|&primitive| map_primitive(ctx, primitive))
                            .collect::<Vec<_>>(),
                        false,
                    )
                    .fn_type(&parameters, false),
            })
            .unwrap_or_else(|| ctx.void_type().fn_type(&parameters, false));

        let f = self.0.add_function(symbol, fn_type, None);

        let noalias = self.1.create_enum_attribute(Attribute::get_named_enum_kind_id("noalias"), 0);
        let nocapture = self
            .1
            .create_enum_attribute(Attribute::get_named_enum_kind_id("nocapture"), 0);
        // let writeonly = self.1.create_enum_attribute(Attribute::get_named_enum_kind_id("writeonly"), 0);

        for (index, param) in params.iter().enumerate() {
            if let Ty::Primitive(Primitive::Ptr {
                mutable,
            }) = param
            {
                if *mutable {
                    f.add_attribute(AttributeLoc::Param(index as u32), noalias);
                }

                f.add_attribute(AttributeLoc::Param(index as u32), nocapture);
            }
        }

        for attr in ["nocallback", "nofree", "nosync", "nounwind", "willreturn"] {
            let attr = self.1.create_enum_attribute(Attribute::get_named_enum_kind_id(attr), 0);
            f.add_attribute(AttributeLoc::Function, attr);
        }

        if symbol == "mem_read4_simple"
            || symbol == "mem_read2_simple"
            || symbol == "mem_write4_simple"
            || symbol == "mem_write2_simple"
        {
            // TODO: Is this safe? Technically memory is written from this pointer, but LLVM is completely unaware that this pointer exists in the emulator struct, and might as well just believe that memory is stored in some kind of global variable somewhere.
            let attr = self.1.create_enum_attribute(Attribute::get_named_enum_kind_id("readonly"), 0);
            f.add_attribute(AttributeLoc::Param(0), attr);

            let attr = self
                .1
                .create_enum_attribute(Attribute::get_named_enum_kind_id("nocapture"), 0);
            f.add_attribute(AttributeLoc::Param(0), attr);
        }

        f
    }

    fn postprocess(&mut self, id: Self::FuncId) -> Self::Output {
        id
    }
}

/// This type enables some late optimizations for the llvm IR generation.
/// It doesn't meaningfully affect performance of the generated code.
/// However, it makes the IR much more readable for debugging purposes.
#[derive(Clone, PartialEq, Eq)]
enum Unmaterialized<'ctx> {
    Val(IntValue<'ctx>),
    Add {
        lhs: Box<Unmaterialized<'ctx>>,
        rhs: Box<Unmaterialized<'ctx>>,
    },
}

impl<'ctx> Unmaterialized<'ctx> {
    pub fn new(val: IntValue<'ctx>) -> Self {
        Self::Val(val)
    }

    pub fn bits(&self) -> u32 {
        match self {
            Self::Val(val) => val.get_type().get_bit_width(),
            Self::Add {
                lhs,
                rhs,
            } => lhs.bits().max(rhs.bits()) + 1,
        }
    }

    fn get_type(&self, context: &'ctx Context) -> IntType<'ctx> {
        match self {
            Unmaterialized::Val(val) => val.get_type(),
            Unmaterialized::Add {
                lhs,
                rhs,
            } => {
                let types = [
                    context.i8_type(),
                    context.i16_type(),
                    context.i32_type(),
                    context.i64_type(),
                    context.i128_type(),
                ];
                types
                    .into_iter()
                    .find(|t| {
                        let w = t.get_bit_width();
                        w > lhs.bits() && w > rhs.bits()
                    })
                    .unwrap_or_else(|| context.i128_type())
            },
        }
    }

    /// Materializes `self` into an IntValue type.
    /// This type may be of any size.
    /// `resulting_type` is a hint that is provided to allow optimizations.
    /// For example, an addition can avoid scaling to a bigger integer if the resulting type is only i8.
    fn materialize_to_val(
        &self, resulting_type: Option<IntType<'ctx>>, builder: &Builder<'ctx>, context: &'ctx Context,
    ) -> IntValue<'ctx> {
        match self {
            Unmaterialized::Val(val) => *val,
            Self::Add {
                lhs,
                rhs,
            } => {
                // We first emit the integers in their current sizes
                let lhs = lhs.into_int_value(builder, context);
                let rhs = rhs.into_int_value(builder, context);

                // Now we determine the maximum size we need.
                // The maximum sizes required for lhs and rhs are always restricted to at most the resulting_type size.
                // Among lhs and rhs we need to take the maximum size + 1, since there might be overflow when the biggest size is `MAX` and the other operand is non-zero.
                // self.get_type() returns a type that fits max(lhs, rhs) + 1.
                let max_type = self.get_type(context);
                let max_type = if let Some(t) = resulting_type
                    && max_type.get_bit_width() > t.get_bit_width()
                {
                    t
                } else {
                    max_type
                };
                let rhs = Unmaterialized::new(rhs).materialize(max_type, builder, context);
                let lhs = Unmaterialized::new(lhs).materialize(max_type, builder, context);

                builder.build_int_add(lhs, rhs, "add").unwrap()
            },
        }
    }

    pub fn materialize(&self, required_type: IntType<'ctx>, builder: &Builder<'ctx>, context: &'ctx Context) -> IntValue<'ctx> {
        let val = self.materialize_to_val(Some(required_type), builder, context);
        if required_type != val.get_type() {
            if required_type.get_bit_width() > val.get_type().get_bit_width() {
                builder.build_int_z_extend(val, required_type, "ext").unwrap()
            } else {
                builder.build_int_truncate(val, required_type, "tr").unwrap()
            }
        } else {
            val
        }
    }

    pub fn truncate_or_zext(
        &self, required_type: IntType<'ctx>, builder: &Builder<'ctx>, context: &'ctx Context,
    ) -> Unmaterialized<'ctx> {
        Unmaterialized::new(self.materialize(required_type, builder, context))
    }

    pub fn map_any_int_type(
        self, builder: &Builder<'ctx>, context: &'ctx Context, op: impl FnOnce(IntValue<'ctx>) -> IntValue<'ctx>,
    ) -> Unmaterialized<'ctx> {
        Self::Val(op(self.materialize_to_val(None, builder, context)))
    }

    pub fn resize_to_biggest_int_needed<const N: usize>(
        vals: [Unmaterialized<'ctx>; N], builder: &Builder<'ctx>, context: &'ctx Context,
    ) -> [Unmaterialized<'ctx>; N] {
        let ty = Self::compute_biggest_int_needed(vals.iter(), context);
        vals.map(|val| val.truncate_or_zext(ty, builder, context))
    }

    pub fn resize_to_biggest_int_needed_and_map<const N: usize>(
        vals: [Unmaterialized<'ctx>; N], builder: &Builder<'ctx>, context: &'ctx Context,
        map: impl FnOnce([IntValue<'ctx>; N]) -> IntValue<'ctx>,
    ) -> Unmaterialized<'ctx> {
        let vals = Self::resize_to_biggest_int_needed(vals, builder, context);
        let vals = vals.map(|x| x.materialize_to_val(None, builder, context));
        Unmaterialized::new(map(vals))
    }

    pub fn compute_biggest_int_needed<'r>(
        val: impl IntoIterator<Item = &'r Unmaterialized<'ctx>>, context: &'ctx Context,
    ) -> IntType<'ctx>
    where
        'ctx: 'r,
    {
        Self::biggest_int_type(val.into_iter().map(|v| v.get_type(context)))
    }

    fn biggest_int_type(val: impl IntoIterator<Item = IntType<'ctx>>) -> IntType<'ctx> {
        val.into_iter().max_by_key(|t| t.get_bit_width()).unwrap()
    }

    pub fn into_int_value(&self, builder: &Builder<'ctx>, context: &'ctx Context) -> IntValue<'ctx> {
        self.materialize_to_val(None, builder, context)
    }
}

pub struct InkwellContext {
    context: Context,
}

impl Default for InkwellContext {
    fn default() -> Self {
        Self::new()
    }
}

impl InkwellContext {
    pub fn new() -> Self {
        Self {
            context: Context::create(),
        }
    }

    pub fn leak_new() -> &'static InkwellContext {
        let ctx = Box::new(Self::new());
        Box::leak(ctx)
    }
}

pub struct InkwellBackend<'ctx> {
    context: &'ctx Context,
    num_generated: usize,
}

impl Backend for InkwellBackend<'_> {
    type UninstantiatedFn = UninstantiatedInkwellFunction;
    type Fn = InkwellFunction;
    type Error = InkwellError;

    fn codegen_lir(&mut self, _: &Lir) -> Result<Self::UninstantiatedFn, Self::Error> {
        unreachable!()
    }

    fn codegen_lir_object(&mut self, lir: &Lir) -> Result<Object, Self::Error> {
        Ok(self.codegen_object(lir))
    }
}

impl<'ctx> InkwellBackend<'ctx> {
    fn initialize_llvm() {
        static INIT: OnceLock<()> = OnceLock::new();
        INIT.get_or_init(|| {
            #[cfg(not(target_os = "android"))]
            Target::initialize_native(&InitializationConfig::default()).unwrap();
            #[cfg(target_os = "android")]
            Target::initialize_aarch64(&InitializationConfig::default());
        });
    }

    pub fn new(context: &'ctx InkwellContext) -> Self {
        Self::initialize_llvm();
        Self {
            context: &context.context,
            num_generated: 0,
        }
    }

    pub fn codegen_ir_and_asm(&mut self, lir: &Lir) -> (String, String) {
        let mut module = self.context.create_module(&format!("inkwell_debug{}", self.num_generated));
        let name = format!("inkwell_jitfn_{}", self.num_generated);
        let mut ftable = FunctionTable::new();
        let g = FunctionGenerator::new(self.context, &mut module, &mut ftable, &name);
        g.codegen_inner(lir, false, false);

        let ir = module.print_to_string().to_string();

        let target_triple = TargetMachine::get_default_triple();
        let target = Target::from_triple(&target_triple).unwrap();
        let cpu_name = TargetMachine::get_host_cpu_name();
        let cpu_features = TargetMachine::get_host_cpu_features();
        let target_machine = target
            .create_target_machine(
                &target_triple,
                cpu_name.to_str().unwrap(),
                cpu_features.to_str().unwrap(),
                inkwell::OptimizationLevel::Default,
                inkwell::targets::RelocMode::Default,
                inkwell::targets::CodeModel::Default,
            )
            .unwrap();

        let buffer = target_machine.write_to_memory_buffer(&module, FileType::Assembly).unwrap();
        let bytes = buffer.as_slice();

        let asm = std::str::from_utf8(bytes).unwrap().to_string();

        (ir, asm)
    }

    pub fn codegen_object(&mut self, lir: &Lir) -> Object {
        let mut module = self.context.create_module(&format!("inkwell_object{}", self.num_generated));
        let name = format!("inkwell_jitfn_{}", self.num_generated);
        let mut ftable = FunctionTable::new();
        let g = FunctionGenerator::new(self.context, &mut module, &mut ftable, &name);
        g.codegen_inner(lir, false, false);

        #[cfg(debug_assertions)]
        if log_enabled!(Level::Info) {
            info!("IR: {}", module.print_to_string().to_string());
        }

        let target_triple = TargetMachine::get_default_triple();
        let target = Target::from_triple(&target_triple).unwrap();
        let cpu_name = TargetMachine::get_host_cpu_name();
        let cpu_features = TargetMachine::get_host_cpu_features();
        let target_machine = target
            .create_target_machine(
                &target_triple,
                cpu_name.to_str().unwrap(),
                cpu_features.to_str().unwrap(),
                inkwell::OptimizationLevel::Default,
                inkwell::targets::RelocMode::PIC,
                inkwell::targets::CodeModel::Small,
            )
            .unwrap();

        let buffer = target_machine.write_to_memory_buffer(&module, FileType::Object).unwrap();
        let bytes = buffer.as_slice();
        Object::new(bytes.to_vec())
    }

    pub fn codegen_page(&mut self, blocks: &[LirBlock]) -> Result<Object, InkwellError> {
        let mut ftable = FunctionTable::new();
        let mut module = self.context.create_module(&format!("inkwell_object{}", self.num_generated));

        let blocks_with_functions = blocks
            .iter()
            .map(|block| {
                let name = format!("inkwell_jit_pageblock_{:04X}", block.id);
                (
                    block,
                    FunctionGenerator::create_function(self.context, &mut module, &name, block.export, true),
                )
            })
            .collect::<Vec<_>>();

        for &(block, function) in blocks_with_functions.iter() {
            let mut g = FunctionGenerator::new_with_function(self.context, &mut module, &mut ftable, function);
            g.set_local_next_functions(
                block
                    .next
                    .iter()
                    .map(|(&key, n)| {
                        (
                            key,
                            (
                                n.items()
                                    .map(|n| (n.offset, blocks_with_functions[n.block_index].1))
                                    .collect(),
                                n.clone(),
                            ),
                        )
                    })
                    .collect(),
            );
            g.codegen_inner(&block.lir, block.check_intr, true);
        }

        if log_enabled!(Level::Info) {
            info!("PageJIT IR: {}", module.print_to_string().to_string());
        }

        let target_triple = TargetMachine::get_default_triple();
        let target = Target::from_triple(&target_triple).unwrap();
        let cpu_name = TargetMachine::get_host_cpu_name();
        let cpu_features = TargetMachine::get_host_cpu_features();
        let target_machine = target
            .create_target_machine(
                &target_triple,
                cpu_name.to_str().unwrap(),
                cpu_features.to_str().unwrap(),
                inkwell::OptimizationLevel::Default,
                inkwell::targets::RelocMode::PIC,
                inkwell::targets::CodeModel::Small,
            )
            .unwrap();

        if log_enabled!(Level::Debug) {
            let buffer = target_machine.write_to_memory_buffer(&module, FileType::Assembly).unwrap();
            let bytes = buffer.as_slice();

            let asm = std::str::from_utf8(bytes).unwrap().to_string();
            info!("PageJIT ASM: {asm}");
        }

        let buffer = target_machine.write_to_memory_buffer(&module, FileType::Object).unwrap();
        let bytes = buffer.as_slice();
        Ok(Object::new(bytes.to_vec()))
    }

    pub fn codegen_lir(&mut self, _lir: &Lir) -> Result<InkwellFunction, InkwellError> {
        unimplemented!("Use one of the methods that generates an Object instead")
    }
}

struct FunctionGenerator<'ctx, 'r> {
    context: &'ctx Context,
    module: &'r mut Module<'ctx>,
    function: FunctionValue<'ctx>,
    ftable: &'r mut FunctionTable<FunctionValue<'ctx>>,
    inline_memory_accesses: bool,
    builder: Builder<'ctx>,
    next_functions: HashMap<u64, (Vec<(u16, FunctionValue<'ctx>)>, NextOnPage)>,
}

impl<'ctx, 'r> FunctionGenerator<'ctx, 'r> {
    fn create_function(
        context: &'ctx Context, module: &mut Module<'ctx>, name: &str, export: bool, single_param_signature: bool,
    ) -> FunctionValue<'ctx> {
        let ptr_ty = context.ptr_type(AddressSpace::default());
        let args = if single_param_signature {
            &[ptr_ty.into()] as &[_]
        } else {
            &[ptr_ty.into(), context.i8_type().into(), context.i128_type().into()]
        };
        let fn_type = context.i64_type().fn_type(args, false);

        let function = module.add_function(name, fn_type, if export { None } else { Some(Linkage::Private) });

        let noalias = context.create_enum_attribute(Attribute::get_named_enum_kind_id("noalias"), 0);
        let nocapture = context.create_enum_attribute(Attribute::get_named_enum_kind_id("nocapture"), 0);

        function.add_attribute(AttributeLoc::Param(0), noalias);
        function.add_attribute(AttributeLoc::Param(0), nocapture);
        function
    }

    pub fn new(
        context: &'ctx Context, module: &'r mut Module<'ctx>, ftable: &'r mut FunctionTable<FunctionValue<'ctx>>, name: &str,
    ) -> Self {
        let function = Self::create_function(context, module, name, true, false);
        Self::new_with_function(context, module, ftable, function)
    }

    pub fn new_with_function(
        context: &'ctx Context, module: &'r mut Module<'ctx>, ftable: &'r mut FunctionTable<FunctionValue<'ctx>>,
        function: FunctionValue<'ctx>,
    ) -> Self {
        let entry = context.append_basic_block(function, "entry");
        let builder = context.create_builder();
        builder.position_at_end(entry);

        Self {
            ftable,
            inline_memory_accesses: false,
            context,
            module,
            function,
            builder,
            next_functions: HashMap::new(),
        }
    }

    fn set_local_next_functions(&mut self, map: HashMap<u64, (Vec<(u16, FunctionValue<'ctx>)>, NextOnPage)>) {
        self.next_functions = map;
    }

    fn codegen_inner(mut self, lir: &Lir, force_intr_check: bool, single_parameter_signature: bool) {
        // Create function signature: fn(*i8 emulator, instr_len, part_values) -> i16
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let function = self.function;

        // let writeonly = self.context.create_enum_attribute(Attribute::get_named_enum_kind_id("writeonly"), 0);

        // Prepare stack slots emulated with alloca in entry (we're already in entry)
        let i64_ty = self.context.i64_type();
        let i128_ty = self.context.i128_type();

        let builder = &self.builder;

        // function params
        let param_emulator = function.get_nth_param(0).unwrap().into_pointer_value();
        let param_ctx = unsafe {
            builder.build_in_bounds_gep(
                self.context.i8_type(),
                param_emulator,
                &[i64_ty.const_int(offset_of!(Emulator, ctx) as u64, false)],
                "ctx",
            )
        }
        .unwrap();
        let param_cpu_state = unsafe {
            builder.build_in_bounds_gep(
                self.context.i8_type(),
                param_emulator,
                &[i64_ty.const_int(offset_of!(Emulator, cpu) as u64, false)],
                "cpu",
            )
        }
        .unwrap();

        let (instr_len, param_part_values) = if single_parameter_signature {
            (self.context.i8_type().const_zero(), self.context.i128_type().const_zero())
        } else {
            let param_instr_len = function.get_nth_param(1).unwrap().into_int_value();
            let instr_len = builder.build_int_z_extend(param_instr_len, i128_ty, "instr_len").unwrap();
            let param_part_values = function.get_nth_param(2).unwrap().into_int_value();
            (instr_len, param_part_values)
        };

        let initial_pc = if lir.blocks.iter().any(|block| {
            if let Jump::Exit {
                success: true,
                metadata,
                ..
            } = block.next()
                && let Some(metadata) = metadata
                && let Some((_, next)) = self.next_functions.get(metadata)
            {
                matches!(next, NextOnPage::Speculative(n) if !n.is_empty())
            } else {
                false
            }
        }) {
            Some(self.compute_pc(param_cpu_state))
        } else {
            None
        };

        // local stack slots (alloca)
        let need_u128_return = lir.blocks.iter().any(|block| {
            block
                .operations()
                .iter()
                .any(|op| matches!(op, LirOp::ReadMemory { num_bytes } if ![1, 2, 4].contains(&num_bytes)))
        });
        let u128_return_val_slot = if need_u128_return {
            let slot = builder.build_alloca(self.context.i128_type(), "u128_return").unwrap();
            slot.as_instruction().unwrap().set_alignment(16).unwrap();
            Some(slot)
        } else {
            None
        };

        let is_userspace = self.is_userspace(param_cpu_state);

        // Prepare blocks
        let block_count = lir.blocks.len();
        let mut blocks = Vec::with_capacity(block_count);
        for (idx, _) in lir.blocks.iter().enumerate() {
            let b = self.context.append_basic_block(function, &format!("block_{}", idx));
            blocks.push(b);
        }

        let mut num_stores = vec![0; lir.num_defs()];
        for block in lir.blocks.iter() {
            for op in block.operations() {
                if let LirOp::Store(def) = op {
                    num_stores[def.index()] += 1;
                }
            }
        }

        // Jump to first block
        builder.build_unconditional_branch(blocks[0]).unwrap();

        // We can't produce IntValue without a builder with arithmetic, so for constants we'll use builder.build_int_z_ext on smaller ints:
        let make_u128_const = |ctx: &'ctx Context, builder: &Builder<'ctx>, v: u128| -> IntValue<'ctx> {
            // if fits in 64 bits:
            let i128t = ctx.i128_type();
            if v >> 64 == 0 {
                // build from i64 const and zext
                let lo = ctx.i64_type().const_int(v as u64, false);
                builder.build_int_z_extend(lo, i128t, "u128_lo_zext").unwrap()
            } else {
                // build by splitting: (hi << 64) | lo
                let lo = ctx.i64_type().const_int(v as u64, false);
                let hi = ctx.i64_type().const_int((v >> 64) as u64, false);
                let lo_i128 = builder.build_int_z_extend(lo, i128t, "lo_i128").unwrap();
                let hi_i128 = builder.build_int_z_extend(hi, i128t, "hi_i128").unwrap();
                let shifted_hi = builder
                    .build_left_shift(hi_i128, ctx.i128_type().const_int(64, false), "shift_hi")
                    .unwrap();
                builder.build_or(shifted_hi, lo_i128, "combine128").unwrap()
            }
        };

        let mut order: Vec<BlockId> = Vec::new();
        let mut likely = vec![false; lir.blocks.len()];
        StronglyConnectedComponents::iterate(lir, |items| {
            for item in items.iter() {
                likely[item.index()] = match lir[*item].next() {
                    Jump::Next(next) => likely[next.index()],
                    Jump::Cond {
                        if_zero,
                        if_nonzero,
                    } => likely[if_zero.index()] || likely[if_nonzero.index()],
                    Jump::Exit {
                        success, ..
                    } => *success,
                    Jump::Unreachable => false,
                }
            }

            order.extend(items);
        });

        let mut unique_id = 0u64;
        let mut block_vars: Vec<Vec<Vec<(BasicBlock<'_>, Unmaterialized<'_>)>>> =
            vec![vec![Vec::new(); lir.num_defs()]; lir.blocks.len()];

        // Iterate over blocks and produce instructions
        for &block_id in order.iter().rev() {
            let builder = &self.builder;
            let block = &lir[block_id];
            let bb = blocks[block_id.index()];
            builder.position_at_end(bb);

            let mut vars = {
                let in_vars = &block_vars[block_id.index()];

                in_vars
                    .iter()
                    .enumerate()
                    .map(|(index, vals): (_, &Vec<(BasicBlock, Unmaterialized<'_>)>)| {
                        if vals.is_empty() {
                            None
                        } else if vals.iter().tuple_windows().all(|((_, a), (_, b))| a == b) {
                            Some(vals[0].1.clone())
                        } else {
                            let ty = Unmaterialized::compute_biggest_int_needed(vals.iter().map(|(_, val)| val), self.context);
                            let materialized_vals = vals
                                .iter()
                                .map(|(source, val)| {
                                    // We need to ensure materialization is done in the correct block
                                    let term = source.get_terminator().unwrap();
                                    builder.position_before(&term);
                                    (*source, val.materialize(ty, builder, self.context))
                                })
                                .collect::<Vec<_>>();

                            // The phi nodes need to be at the beginning of the current block
                            builder.position_at_end(bb);
                            let val = builder.build_phi(ty, &format!("def{index}_phi")).unwrap();
                            val.add_incoming(
                                &materialized_vals
                                    .iter()
                                    .map(|(source, val)| (val as &dyn BasicValue<'_>, *source))
                                    .collect::<Vec<_>>(),
                            );

                            Some(Unmaterialized::new(val.as_basic_value().into_int_value()))
                        }
                    })
                    .collect::<Vec<_>>()
            };

            let mut stack: Vec<Unmaterialized> = Vec::new();
            for &op in block.operations().iter() {
                let builder = &self.builder;
                match op {
                    LirOp::Const(const_id) => {
                        let v = lir[const_id];
                        stack.push(Unmaterialized::new(if v <= u8::MAX as u128 {
                            self.context.i8_type().const_int(v as u64, false)
                        } else if v <= u16::MAX as u128 {
                            self.context.i16_type().const_int(v as u64, false)
                        } else if v <= u32::MAX as u128 {
                            self.context.i32_type().const_int(v as u64, false)
                        } else if v <= u64::MAX as u128 {
                            self.context.i64_type().const_int(v as u64, false)
                        } else {
                            make_u128_const(self.context, builder, v)
                        }));
                    },
                    LirOp::Load(def_id) => {
                        stack.push(
                            vars[def_id.index()]
                                .clone()
                                .expect("definition should have value set before being used"),
                        );
                    },
                    LirOp::Store(def_id) => {
                        // Since we have no idea when this definition will be used again, we must ensure any complex operations are materialized.
                        // If we did not do this, it might not dominate all future uses as the complex operation would be materialized in just one path.
                        let val = stack.pop().unwrap();
                        vars[def_id.index()] = Some(Unmaterialized::new(val.into_int_value(builder, self.context)));
                    },
                    LirOp::BinOp(bin_op) => {
                        let rhs = stack.pop().unwrap();
                        let lhs = stack.pop().unwrap();
                        let res = match bin_op {
                            BinOp::Add => Unmaterialized::Add {
                                lhs: Box::new(lhs),
                                rhs: Box::new(rhs),
                            },
                            BinOp::Sub => {
                                let rhs = rhs.materialize(self.context.i128_type(), builder, self.context);
                                let lhs = lhs.materialize(self.context.i128_type(), builder, self.context);

                                Unmaterialized::new(builder.build_int_sub(lhs, rhs, "sub").unwrap())
                            },
                            BinOp::Mul => {
                                let rhs = rhs.materialize(self.context.i128_type(), builder, self.context);
                                let lhs = lhs.materialize(self.context.i128_type(), builder, self.context);

                                Unmaterialized::new(builder.build_int_mul(lhs, rhs, "mul").unwrap())
                            },
                            BinOp::Xor => Unmaterialized::resize_to_biggest_int_needed_and_map(
                                [lhs, rhs],
                                builder,
                                self.context,
                                |[lhs, rhs]| builder.build_xor(lhs, rhs, "xor").unwrap(),
                            ),
                            BinOp::Or => Unmaterialized::resize_to_biggest_int_needed_and_map(
                                [lhs, rhs],
                                builder,
                                self.context,
                                |[lhs, rhs]| builder.build_or(lhs, rhs, "or").unwrap(),
                            ),
                            BinOp::And => Unmaterialized::resize_to_biggest_int_needed_and_map(
                                [lhs, rhs],
                                builder,
                                self.context,
                                |[lhs, rhs]| builder.build_and(lhs, rhs, "and").unwrap(),
                            ),
                            BinOp::Shl => {
                                let rhs = rhs.materialize(self.context.i128_type(), builder, self.context);
                                let lhs = lhs.materialize(self.context.i128_type(), builder, self.context);

                                Unmaterialized::new(builder.build_left_shift(lhs, rhs, "shl").unwrap())
                            },
                            BinOp::Shr => {
                                // We must instantiate an i128, because the shift count is truncated differently for different bit widths
                                let lhs = lhs.materialize(self.context.i128_type(), builder, self.context);
                                let rhs = rhs.materialize(self.context.i128_type(), builder, self.context);
                                Unmaterialized::new(builder.build_right_shift(lhs, rhs, false, "ushr").unwrap())
                            },
                            BinOp::Rol(num_bits) => {
                                let rhs = rhs.materialize(self.context.i128_type(), builder, self.context);
                                let lhs = lhs.materialize(self.context.i128_type(), builder, self.context);

                                // (val << (shift % width)) | (val >> (width - (shift % width)))
                                let rhs64 = builder.build_int_truncate(rhs, self.context.i64_type(), "rhs64").unwrap();
                                let shift64 = builder
                                    .build_int_unsigned_rem(
                                        rhs64,
                                        self.context.i64_type().const_int(num_bits as u64, false),
                                        "mod",
                                    )
                                    .unwrap();
                                let shift128 = builder
                                    .build_int_z_extend(shift64, self.context.i128_type(), "sh128")
                                    .unwrap();
                                let width_val = self.context.i128_type().const_int(num_bits as u64, false);
                                let inv = builder.build_int_sub(width_val, shift128, "inv").unwrap();
                                let a = builder.build_left_shift(lhs, shift128, "rol_a").unwrap();
                                let b = builder.build_right_shift(lhs, inv, false, "rol_b").unwrap();
                                Unmaterialized::new(builder.build_or(a, b, "rol_res").unwrap())
                            },
                            BinOp::Ror(num_bits) => {
                                let rhs = rhs.materialize(self.context.i128_type(), builder, self.context);
                                let lhs = lhs.materialize(self.context.i128_type(), builder, self.context);

                                // (val >> (shift % width)) | (val << (width - (shift % width)))
                                let rhs64 = builder.build_int_truncate(rhs, self.context.i64_type(), "rhs64").unwrap();
                                let shift64 = builder
                                    .build_int_unsigned_rem(
                                        rhs64,
                                        self.context.i64_type().const_int(num_bits as u64, false),
                                        "mod",
                                    )
                                    .unwrap();
                                let shift128 = builder
                                    .build_int_z_extend(shift64, self.context.i128_type(), "sh128")
                                    .unwrap();
                                let width_val = self.context.i128_type().const_int(num_bits as u64, false);
                                let inv = builder.build_int_sub(width_val, shift128, "inv").unwrap();
                                let a = builder.build_right_shift(lhs, shift128, false, "ror_a").unwrap();
                                let b = builder.build_left_shift(lhs, inv, "ror_b").unwrap();
                                Unmaterialized::new(builder.build_or(a, b, "ror_res").unwrap())
                            },
                            BinOp::Sar(n) => {
                                let rhs = rhs.materialize(self.context.i128_type(), builder, self.context);
                                let lhs = lhs.materialize(self.context.i128_type(), builder, self.context);

                                let reduced = match n {
                                    1 => builder.build_int_truncate(lhs, self.context.i8_type(), "tr1"),
                                    2 => builder.build_int_truncate(lhs, self.context.i16_type(), "tr2"),
                                    4 => builder.build_int_truncate(lhs, self.context.i32_type(), "tr4"),
                                    _ => unimplemented!(),
                                }
                                .unwrap();
                                let shift = builder.build_int_truncate(rhs, reduced.get_type(), "sh_zext").unwrap();
                                let shr = builder.build_right_shift(reduced, shift, true, "sshr").unwrap();
                                Unmaterialized::new(builder.build_int_s_extend(shr, self.context.i128_type(), "sext128").unwrap())
                            },
                            BinOp::Div => {
                                // TODO: 128-bit division; use 64-bit for now
                                let rhs64 = rhs.materialize(self.context.i64_type(), builder, self.context);
                                let lhs64 = lhs.materialize(self.context.i64_type(), builder, self.context);
                                Unmaterialized::new(builder.build_int_unsigned_div(lhs64, rhs64, "udiv").unwrap())
                            },
                            BinOp::Mod => {
                                // TODO: 128-bit modulo; use 64-bit for now
                                let rhs64 = rhs.materialize(self.context.i64_type(), builder, self.context);
                                let lhs64 = lhs.materialize(self.context.i64_type(), builder, self.context);
                                Unmaterialized::new(builder.build_int_unsigned_rem(lhs64, rhs64, "urem").unwrap())
                            },
                            BinOp::SignedMod64 => {
                                let rhs64 = rhs.materialize(self.context.i64_type(), builder, self.context);
                                let lhs64 = lhs.materialize(self.context.i64_type(), builder, self.context);
                                Unmaterialized::new(builder.build_int_signed_rem(lhs64, rhs64, "srem").unwrap())
                            },
                            BinOp::CmpGt => Unmaterialized::resize_to_biggest_int_needed_and_map(
                                [lhs, rhs],
                                builder,
                                self.context,
                                |[lhs, rhs]| builder.build_int_compare(IntPredicate::UGT, lhs, rhs, "ugt").unwrap(),
                            ),
                            BinOp::CmpLt => Unmaterialized::resize_to_biggest_int_needed_and_map(
                                [lhs, rhs],
                                builder,
                                self.context,
                                |[lhs, rhs]| builder.build_int_compare(IntPredicate::ULT, lhs, rhs, "ult").unwrap(),
                            ),
                            BinOp::CmpEq => Unmaterialized::resize_to_biggest_int_needed_and_map(
                                [lhs, rhs],
                                builder,
                                self.context,
                                |[lhs, rhs]| builder.build_int_compare(IntPredicate::EQ, lhs, rhs, "eq").unwrap(),
                            ),
                            BinOp::SignedDiv64 => {
                                let lhs64 = lhs.materialize(self.context.i64_type(), builder, self.context);
                                let rhs64 = rhs.materialize(self.context.i64_type(), builder, self.context);
                                Unmaterialized::new(builder.build_int_signed_div(lhs64, rhs64, "sdiv").unwrap())
                            },
                        };
                        stack.push(res);
                    },
                    LirOp::FpBinOp(bin_op) => {
                        let rc = stack
                            .pop()
                            .unwrap()
                            .materialize(self.context.i8_type(), builder, self.context);
                        let rhs = stack.pop().unwrap();
                        let lhs = stack.pop().unwrap();

                        let rhs = rhs.materialize(self.context.i128_type(), builder, self.context);
                        let lhs = lhs.materialize(self.context.i128_type(), builder, self.context);

                        stack.push(self.invoke_op_fn([lhs, rhs, rc], bin_op.into()));
                    },
                    LirOp::Blend(mask) => {
                        let mask = lir[mask];
                        let new = stack
                            .pop()
                            .unwrap()
                            .materialize(self.context.i128_type(), builder, self.context);
                        let old = stack
                            .pop()
                            .unwrap()
                            .materialize(self.context.i128_type(), builder, self.context);
                        let mask_const = make_u128_const(self.context, builder, mask);
                        // bitselect(mask, new, old) -> (mask & new) | (~mask & old)
                        let a = builder.build_and(mask_const, new, "mask_and_new").unwrap();
                        let not_mask = builder.build_not(mask_const, "not_mask").unwrap();
                        let b = builder.build_and(not_mask, old, "nmask_and_old").unwrap();
                        let res = builder.build_or(a, b, "blend128").unwrap();
                        stack.push(Unmaterialized::new(res));
                    },
                    LirOp::Extract {
                        skip,
                        take,
                    } => {
                        let val = stack.pop().unwrap();
                        let shifted = val.map_any_int_type(builder, self.context, |val| if skip as u32 >= val.get_type().get_bit_width() {
                            log::trace!("extracting with skip={skip}, which is out of range of value {val} -- likely a bug, always returns 0");
                            val.get_type().const_int(0, false)
                        } else if skip > 0 {
                            builder.build_right_shift(
                                val,
                                val.get_type().const_int(skip as u64, false),
                                false,
                                "shr_extract",
                            ).unwrap()
                        } else {
                            val
                        });

                        let crop_type = match take {
                            1 => Some(self.context.bool_type()),
                            8 => Some(self.context.i8_type()),
                            16 => Some(self.context.i16_type()),
                            32 => Some(self.context.i32_type()),
                            64 => Some(self.context.i64_type()),
                            _ => None,
                        };

                        stack.push(if let Some(crop_type) = crop_type {
                            shifted.truncate_or_zext(crop_type, builder, self.context)
                        } else {
                            shifted.map_any_int_type(builder, self.context, |shifted| {
                                let mask = Unmaterialized::new(make_u128_const(self.context, builder, bitmask_u128(take as u32)));
                                let mask = mask.materialize(shifted.get_type(), builder, self.context);

                                builder.build_and(shifted, mask, "mask_take").unwrap()
                            })
                        });
                    },
                    LirOp::UnOp(un_op) => {
                        let arg = stack.pop().unwrap();
                        let res = match un_op {
                            UnOp::Id => arg,
                            UnOp::ByteSwap16 => {
                                let tr = arg.materialize(self.context.i16_type(), builder, self.context);
                                let intrinsic = Intrinsic::find("llvm.bswap.i16").unwrap();
                                let f: FunctionValue = intrinsic
                                    .get_declaration(self.module, &[self.context.i16_type().into()])
                                    .unwrap();
                                let call = builder.build_call(f, &[tr.into()], "bswapped").unwrap();
                                let bswapped = call.try_as_basic_value().basic().unwrap().into_int_value();
                                Unmaterialized::new(bswapped)
                            },
                            UnOp::ByteSwap32 => {
                                let tr = arg.materialize(self.context.i32_type(), builder, self.context);
                                let intrinsic = Intrinsic::find("llvm.bswap.i32").unwrap();
                                let f: FunctionValue = intrinsic
                                    .get_declaration(self.module, &[self.context.i32_type().into()])
                                    .unwrap();
                                let call = builder.build_call(f, &[tr.into()], "bswapped").unwrap();
                                let bswapped = call.try_as_basic_value().basic().unwrap().into_int_value();
                                Unmaterialized::new(bswapped)
                            },
                            UnOp::ByteSwap64 => {
                                let tr = arg.materialize(self.context.i64_type(), builder, self.context);
                                let intrinsic = Intrinsic::find("llvm.bswap.i64").unwrap();
                                let f: FunctionValue = intrinsic
                                    .get_declaration(self.module, &[self.context.i64_type().into()])
                                    .unwrap();
                                let call = builder.build_call(f, &[tr.into()], "bswapped").unwrap();
                                let bswapped = call.try_as_basic_value().basic().unwrap().into_int_value();
                                Unmaterialized::new(bswapped)
                            },
                            UnOp::IsZero => arg.map_any_int_type(builder, self.context, |arg| {
                                builder
                                    .build_int_compare(IntPredicate::EQ, arg, arg.get_type().const_zero(), "iszero")
                                    .unwrap()
                            }),
                            UnOp::SelectBit(n) => arg.map_any_int_type(builder, self.context, |arg| {
                                if (n as u32) < arg.get_type().get_bit_width() {
                                    let bit_val = builder
                                        .build_right_shift(arg, arg.get_type().const_int(n as u64, false), false, "bitshift")
                                        .unwrap();
                                    builder.build_int_truncate(bit_val, self.context.bool_type(), "tr").unwrap()
                                } else {
                                    self.context.bool_type().const_int(0, false)
                                }
                            }),
                            UnOp::Parity => {
                                // popcnt on i8 then mod2 xor 1
                                let tr = arg.materialize(self.context.i8_type(), builder, self.context);
                                let intrinsic = Intrinsic::find("llvm.ctpop.i8").unwrap();
                                let f: FunctionValue = intrinsic
                                    .get_declaration(self.module, &[self.context.i8_type().into()])
                                    .unwrap();
                                let call = builder.build_call(f, &[tr.into()], "popcnt").unwrap();
                                let popcount = call.try_as_basic_value().basic().unwrap().into_int_value();
                                let popcount = builder
                                    .build_int_z_extend(popcount, self.context.i128_type(), "popcnt_ext")
                                    .unwrap();
                                let one = self.context.i128_type().const_int(1, false);
                                let neg_parity = builder.build_and(popcount, one, "neg_parity").unwrap();
                                let parity = builder
                                    .build_int_compare(
                                        IntPredicate::EQ,
                                        neg_parity,
                                        self.context.i128_type().const_zero(),
                                        "parity",
                                    )
                                    .unwrap();
                                Unmaterialized::new(parity)
                            },
                            UnOp::TrailingZeros => {
                                let arg = arg.materialize(self.context.i128_type(), builder, self.context);
                                let intrinsic = Intrinsic::find("llvm.cttz.i128").unwrap();
                                let f: FunctionValue = intrinsic
                                    .get_declaration(
                                        self.module,
                                        &[self.context.i128_type().into(), self.context.bool_type().into()],
                                    )
                                    .unwrap();
                                let call = builder
                                    .build_call(
                                        f,
                                        &[arg.into(), self.context.bool_type().const_int(0, false).into()],
                                        "trailing_zeros",
                                    )
                                    .unwrap();
                                let trailing_zeros = call.try_as_basic_value().basic().unwrap().into_int_value();
                                Unmaterialized::new(trailing_zeros)
                            },
                            UnOp::HighestBitSet => {
                                let arg = arg.materialize(self.context.i128_type(), builder, self.context);
                                let intrinsic = Intrinsic::find("llvm.ctlz.i128").unwrap();
                                let f: FunctionValue = intrinsic
                                    .get_declaration(
                                        self.module,
                                        &[self.context.i128_type().into(), self.context.bool_type().into()],
                                    )
                                    .unwrap();
                                let call = builder
                                    .build_call(
                                        f,
                                        &[arg.into(), self.context.bool_type().const_int(0, false).into()],
                                        "leading_zeros",
                                    )
                                    .unwrap();
                                let leading_zeros = call.try_as_basic_value().basic().unwrap().into_int_value();
                                let leading_zeros = builder
                                    .build_int_z_extend(leading_zeros, self.context.i128_type(), "leading_zeros_ext")
                                    .unwrap();
                                let n127 = self.context.i128_type().const_int(127, false);

                                Unmaterialized::new(builder.build_int_sub(n127, leading_zeros, "highest_bit_set_index").unwrap())
                            },
                            UnOp::SignExtend(n) => {
                                let arg = arg.materialize(self.context.i128_type(), builder, self.context);
                                let s = 128 - n;
                                let left = builder
                                    .build_left_shift(arg, self.context.i128_type().const_int(s as u64, false), "sleft")
                                    .unwrap();
                                Unmaterialized::new(
                                    builder
                                        .build_right_shift(left, self.context.i128_type().const_int(s as u64, true), true, "sshr")
                                        .unwrap(),
                                )
                            },
                        };
                        stack.push(res);
                    },
                    LirOp::FpUnOp(un_op) => {
                        let rc = stack
                            .pop()
                            .unwrap()
                            .materialize(self.context.i8_type(), builder, self.context);
                        let arg = stack.pop().unwrap();
                        let arg = arg.materialize(self.context.i128_type(), builder, self.context);
                        let res = self.invoke_op_fn([arg, rc], un_op.into());
                        stack.push(res);
                    },
                    LirOp::Ite => {
                        let cond = stack.pop().unwrap();
                        let if_zero = stack
                            .pop()
                            .unwrap()
                            .materialize(self.context.i128_type(), builder, self.context);
                        let if_nonzero = stack
                            .pop()
                            .unwrap()
                            .materialize(self.context.i128_type(), builder, self.context);
                        // boolean selection
                        let cond_bool = cond
                            .map_any_int_type(builder, self.context, |cond| {
                                builder
                                    .build_int_compare(IntPredicate::NE, cond, cond.get_type().const_zero(), "cond_bool")
                                    .unwrap()
                            })
                            .into_int_value(builder, self.context);
                        let sel = builder
                            .build_select(cond_bool, if_nonzero, if_zero, "ite_sel")
                            .unwrap()
                            .into_int_value();
                        stack.push(Unmaterialized::new(sel));
                    },
                    LirOp::LoadPtrWithOffset {
                        ptr,
                        size,
                    } => {
                        let offset = stack
                            .pop()
                            .unwrap()
                            .materialize(self.context.i64_type(), builder, self.context);
                        let base_ptr = match ptr {
                            Ptr::CpuState => param_cpu_state,
                            Ptr::K => unsafe {
                                builder.build_in_bounds_gep(
                                    self.context.i8_type(),
                                    param_emulator,
                                    &[self.context.i64_type().const_int(offset_of!(Emulator, ctx.k) as u64, false)],
                                    "kptr",
                                )
                            }
                            .unwrap(),
                        };
                        // pointer arithmetic
                        let addr =
                            unsafe { builder.build_in_bounds_gep(self.context.i8_type(), base_ptr, &[offset], "addr") }.unwrap();
                        // load appropriate width
                        let ty = match size {
                            DataSize::Byte => self.context.i8_type(),
                            DataSize::Word => self.context.i16_type(),
                            DataSize::Dword => self.context.i32_type(),
                            DataSize::Qword => self.context.i64_type(),
                            DataSize::F80 | DataSize::Oword => self.context.i128_type(),
                        };
                        let load = builder.build_load(ty, addr, "load_ptr").unwrap();
                        let val = if let BasicValueEnum::IntValue(iv) = load {
                            iv
                        } else {
                            panic!("load returned non-int");
                        };
                        let val = if size == DataSize::F80 {
                            let mask = make_u128_const(self.context, builder, bitmask_u128(80));
                            builder.build_and(val, mask, "f80_val").unwrap()
                        } else {
                            val
                        };
                        stack.push(Unmaterialized::new(val));
                    },
                    LirOp::LoadPtrImm {
                        ptr,
                        size,
                        offset,
                    } => {
                        let base_ptr = match ptr {
                            Ptr::CpuState => param_cpu_state,
                            Ptr::K => unsafe {
                                builder.build_in_bounds_gep(
                                    self.context.i8_type(),
                                    param_emulator,
                                    &[self.context.i64_type().const_int(offset_of!(Emulator, ctx.k) as u64, false)],
                                    "kptr",
                                )
                            }
                            .unwrap(),
                        };
                        let addr = unsafe {
                            builder.build_in_bounds_gep(
                                self.context.i8_type(),
                                base_ptr,
                                &[self.context.i32_type().const_int(offset as u64, false)],
                                "addr_imm",
                            )
                        }
                        .unwrap();
                        let ty = match size {
                            DataSize::Byte => self.context.i8_type(),
                            DataSize::Word => self.context.i16_type(),
                            DataSize::Dword => self.context.i32_type(),
                            DataSize::Qword => self.context.i64_type(),
                            DataSize::F80 | DataSize::Oword => self.context.i128_type(),
                        };
                        let load = builder.build_load(ty, addr, "load_ptr_imm").unwrap();
                        let val = if let BasicValueEnum::IntValue(iv) = load {
                            iv
                        } else {
                            panic!("load returned non-int");
                        };
                        let val = if size == DataSize::F80 {
                            let mask = make_u128_const(self.context, builder, bitmask_u128(80));
                            builder.build_and(val, mask, "f80_val").unwrap()
                        } else {
                            val
                        };
                        stack.push(Unmaterialized::new(val));
                    },
                    LirOp::StorePtrWithOffset {
                        ptr,
                        size,
                    } => {
                        let val = stack.pop().unwrap();
                        let offset = stack
                            .pop()
                            .unwrap()
                            .materialize(self.context.i64_type(), builder, self.context);
                        let base_ptr = match ptr {
                            Ptr::CpuState => param_cpu_state,
                            Ptr::K => unsafe {
                                builder.build_in_bounds_gep(
                                    self.context.i8_type(),
                                    param_emulator,
                                    &[self.context.i64_type().const_int(offset_of!(Emulator, ctx.k) as u64, false)],
                                    "kptr",
                                )
                            }
                            .unwrap(),
                        };
                        // pointer arithmetic
                        let addr =
                            unsafe { builder.build_in_bounds_gep(self.context.i8_type(), base_ptr, &[offset], "addr") }.unwrap();
                        let ty = match size {
                            DataSize::Byte => self.context.i8_type(),
                            DataSize::Word => self.context.i16_type(),
                            DataSize::Dword => self.context.i32_type(),
                            DataSize::Qword => self.context.i64_type(),
                            DataSize::F80 | DataSize::Oword => self.context.i128_type(),
                        };
                        let to_store = val.materialize(ty, builder, self.context);
                        builder.build_store(addr, to_store).unwrap();
                    },
                    LirOp::StorePtrImm {
                        ptr,
                        size,
                        offset,
                    } => {
                        let val = stack.pop().unwrap();
                        let base_ptr = match ptr {
                            Ptr::CpuState => param_cpu_state,
                            Ptr::K => unsafe {
                                builder.build_in_bounds_gep(
                                    self.context.i8_type(),
                                    param_emulator,
                                    &[self.context.i64_type().const_int(offset_of!(Emulator, ctx.k) as u64, false)],
                                    "kptr",
                                )
                            }
                            .unwrap(),
                        };
                        let addr = unsafe {
                            builder.build_in_bounds_gep(
                                self.context.i8_type(),
                                base_ptr,
                                &[self.context.i32_type().const_int(offset as u64, false)],
                                "addr_imm",
                            )
                        }
                        .unwrap();
                        let ty = match size {
                            DataSize::Byte => self.context.i8_type(),
                            DataSize::Word => self.context.i16_type(),
                            DataSize::Dword => self.context.i32_type(),
                            DataSize::Qword => self.context.i64_type(),
                            DataSize::F80 | DataSize::Oword => self.context.i128_type(),
                        };
                        let to_store = val.materialize(ty, builder, self.context);
                        builder.build_store(addr, to_store).unwrap();
                    },
                    LirOp::SetExceptionWithCode {
                        exception,
                    } => {
                        let code = stack
                            .pop()
                            .unwrap()
                            .materialize(self.context.i128_type(), builder, self.context);
                        let code32 = builder.build_int_truncate(code, self.context.i32_type(), "code32").unwrap();
                        let vector = self.context.i8_type().const_int(exception.as_u8() as u64, false);
                        let result_ptr = unsafe {
                            builder.build_in_bounds_gep(
                                self.context.i8_type(),
                                param_emulator,
                                &[self
                                    .context
                                    .i32_type()
                                    .const_int(offset_of!(Emulator, ctx.result) as u64, false)],
                                "result_ptr",
                            )
                        }
                        .unwrap();
                        let discr_ptr = unsafe {
                            builder.build_in_bounds_gep(
                                self.context.i8_type(),
                                result_ptr,
                                &[self
                                    .context
                                    .i32_type()
                                    .const_int(offset_of!(PackedExecResult, discr) as u64, false)],
                                "result_ptr",
                            )
                        }
                        .unwrap();
                        let parameter0_ptr = unsafe {
                            builder.build_in_bounds_gep(
                                self.context.i8_type(),
                                result_ptr,
                                &[self
                                    .context
                                    .i32_type()
                                    .const_int(offset_of!(PackedExecResult, parameters) as u64, false)],
                                "result_ptr",
                            )
                        }
                        .unwrap();

                        builder.build_store(discr_ptr, vector).unwrap();
                        builder.build_store(parameter0_ptr, code32).unwrap();
                    },
                    LirOp::SetHandler {
                        id,
                    } => {
                        let arg1 = builder
                            .build_int_truncate(
                                stack
                                    .pop()
                                    .unwrap()
                                    .materialize(self.context.i128_type(), builder, self.context),
                                self.context.i32_type(),
                                "arg1",
                            )
                            .unwrap();
                        let arg0 = builder
                            .build_int_truncate(
                                stack
                                    .pop()
                                    .unwrap()
                                    .materialize(self.context.i128_type(), builder, self.context),
                                self.context.i32_type(),
                                "arg0",
                            )
                            .unwrap();
                        let id_const = self.context.i8_type().const_int(id as u64 + 0x80, false);

                        let result_ptr = unsafe {
                            builder.build_in_bounds_gep(
                                self.context.i8_type(),
                                param_emulator,
                                &[self
                                    .context
                                    .i32_type()
                                    .const_int(offset_of!(Emulator, ctx.result) as u64, false)],
                                "result_ptr",
                            )
                        }
                        .unwrap();
                        let discr_ptr = unsafe {
                            builder.build_in_bounds_gep(
                                self.context.i8_type(),
                                result_ptr,
                                &[self
                                    .context
                                    .i32_type()
                                    .const_int(offset_of!(PackedExecResult, discr) as u64, false)],
                                "result_ptr",
                            )
                        }
                        .unwrap();
                        let parameter0_ptr = unsafe {
                            builder.build_in_bounds_gep(
                                self.context.i8_type(),
                                result_ptr,
                                &[self
                                    .context
                                    .i32_type()
                                    .const_int(offset_of!(PackedExecResult, parameters) as u64, false)],
                                "result_ptr",
                            )
                        }
                        .unwrap();
                        let parameter1_ptr = unsafe {
                            builder.build_in_bounds_gep(
                                self.context.i8_type(),
                                result_ptr,
                                &[self
                                    .context
                                    .i32_type()
                                    .const_int(offset_of!(PackedExecResult, parameters) as u64 + 4, false)],
                                "result_ptr",
                            )
                        }
                        .unwrap();

                        builder.build_store(discr_ptr, id_const).unwrap();
                        builder.build_store(parameter0_ptr, arg0).unwrap();
                        builder.build_store(parameter1_ptr, arg1).unwrap();
                    },
                    LirOp::ReadMemory {
                        num_bytes,
                    } => {
                        let addr = stack
                            .pop()
                            .unwrap()
                            .materialize(self.context.i32_type(), builder, self.context);

                        let can_emit_fast_path = [1, 2, 4, 8, 16].contains(&num_bytes);
                        if can_emit_fast_path && self.inline_memory_accesses {
                            let check_metadata_block = self
                                .context
                                .append_basic_block(function, &format!("check_metadata{unique_id}"));
                            let fast_read_block = self.context.append_basic_block(function, &format!("fast_read{unique_id}"));
                            let slow_read_block = self.context.append_basic_block(function, &format!("slow_read{unique_id}"));
                            let read_done_block = self.context.append_basic_block(function, &format!("read_done{unique_id}"));
                            unique_id += 1;

                            let alignment_bits = self.context.i32_type().const_int((num_bytes - 1) as u64, false);
                            let zero32 = self.context.i32_type().const_int(0, false);
                            let zero8 = self.context.i8_type().const_int(0, false);
                            let addr_alignment_bits = builder.build_and(addr, alignment_bits, "alignment_bits").unwrap();
                            let is_aligned = builder
                                .build_int_compare(IntPredicate::EQ, addr_alignment_bits, zero32, "is_aligned")
                                .unwrap();
                            builder
                                .build_conditional_branch(is_aligned, check_metadata_block, slow_read_block)
                                .unwrap();
                            builder.position_at_end(check_metadata_block);

                            let fast_mem_base_offset = offset_of!(Emulator, ctx.fast_memory.base);
                            let fast_mem_base_offset = self.context.i64_type().const_int(fast_mem_base_offset as u64, false);

                            let fast_mem_base_ptr = unsafe {
                                builder.build_in_bounds_gep(
                                    self.context.i8_type(),
                                    param_emulator,
                                    &[fast_mem_base_offset],
                                    "fast_mem_base_ptr",
                                )
                            }
                            .unwrap();
                            let fast_mem_base = builder
                                .build_load(ptr_ty, fast_mem_base_ptr, "fast_mem_base")
                                .unwrap()
                                .into_pointer_value();
                            let metadata_map_offset = self.context.i64_type().const_int(METADATA_SIZE.wrapping_neg(), true);
                            let metadata_map = unsafe {
                                builder.build_in_bounds_gep(
                                    self.context.i8_type(),
                                    fast_mem_base,
                                    &[metadata_map_offset],
                                    "metadata_map_addr",
                                )
                            }
                            .unwrap();
                            let metadata = {
                                let shift = self.context.i32_type().const_int(12, false);
                                let metadata_offset = builder.build_right_shift(addr, shift, false, "metdata_offset").unwrap();
                                let metadata_offset = builder
                                    .build_int_z_extend(metadata_offset, self.context.i64_type(), "metadata_offset_ext")
                                    .unwrap();
                                let ptr = unsafe {
                                    builder.build_in_bounds_gep(
                                        self.context.i8_type(),
                                        metadata_map,
                                        &[metadata_offset],
                                        "metadata_map_addr",
                                    )
                                }
                                .unwrap();
                                builder
                                    .build_load(self.context.i8_type(), ptr, "metadata")
                                    .unwrap()
                                    .into_int_value()
                            };
                            let system_test_value = MetadataTest::new().require_present().as_bits();
                            let system_test_value_int = self.context.i8_type().const_int(system_test_value as u64, false);
                            let user_test_value = MetadataTest::new()
                                .require_present()
                                .require_accessible_from_userspace()
                                .as_bits();
                            assert_eq!(
                                user_test_value ^ system_test_value,
                                1,
                                "userspace trap flag should be lowest bit"
                            );
                            let test_value = builder
                                .build_or(
                                    system_test_value_int,
                                    builder
                                        .build_int_z_extend(is_userspace, self.context.i8_type(), "is_userspace")
                                        .unwrap(),
                                    "metadata_test",
                                )
                                .unwrap();
                            let trap_bits = builder.build_and(test_value, metadata, "fast_read_nok").unwrap();
                            let should_trap = builder
                                .build_int_compare(IntPredicate::NE, trap_bits, zero8, "should_trap")
                                .unwrap();
                            builder
                                .build_conditional_branch(should_trap, slow_read_block, fast_read_block)
                                .unwrap();
                            builder.position_at_end(fast_read_block);

                            let fast_ok = self.context.bool_type().const_int(1, false);
                            let fast_loaded = {
                                let addr = builder.build_int_z_extend(addr, self.context.i64_type(), "addr_ext").unwrap();
                                let ptr = unsafe {
                                    builder.build_in_bounds_gep(self.context.i8_type(), fast_mem_base, &[addr], "addr")
                                }
                                .unwrap();
                                let ptr_ty = match num_bytes {
                                    1 => self.context.i8_type(),
                                    2 => self.context.i16_type(),
                                    4 => self.context.i32_type(),
                                    8 => self.context.i64_type(),
                                    16 => self.context.i128_type(),
                                    _ => unreachable!(),
                                };

                                builder.build_load(ptr_ty, ptr, "loaded").unwrap().into_int_value()
                            };

                            builder.build_unconditional_branch(read_done_block).unwrap();
                            builder.position_at_end(slow_read_block);
                            let (slow_ok, slow_loaded) =
                                self.emit_slow_memory_read(param_ctx, &u128_return_val_slot, is_userspace, num_bytes, addr);

                            let builder = &self.builder;
                            builder.build_unconditional_branch(read_done_block).unwrap();
                            builder.position_at_end(read_done_block);
                            let ok = {
                                assert_eq!(slow_ok.get_type(), fast_ok.get_type());
                                let phi = builder.build_phi(slow_ok.get_type(), "ok").unwrap();
                                phi.add_incoming(&[
                                    (&slow_ok as &dyn BasicValue, slow_read_block),
                                    (&fast_ok as &dyn BasicValue, fast_read_block),
                                ]);
                                phi.as_basic_value().into_int_value()
                            };
                            let loaded = {
                                assert_eq!(slow_loaded.get_type(), fast_loaded.get_type());
                                let phi = builder.build_phi(slow_loaded.get_type(), "loaded").unwrap();
                                phi.add_incoming(&[
                                    (&slow_loaded as &dyn BasicValue, slow_read_block),
                                    (&fast_loaded as &dyn BasicValue, fast_read_block),
                                ]);
                                phi.as_basic_value().into_int_value()
                            };

                            stack.push(Unmaterialized::new(ok));
                            stack.push(Unmaterialized::new(loaded));
                        } else {
                            let (slow_ok, slow_loaded) =
                                self.emit_slow_memory_read(param_ctx, &u128_return_val_slot, is_userspace, num_bytes, addr);
                            stack.push(Unmaterialized::new(slow_ok));
                            stack.push(Unmaterialized::new(slow_loaded));
                        }
                    },
                    LirOp::WriteMemory {
                        num_bytes,
                    } => {
                        let value = stack.pop().unwrap();
                        let addr = stack
                            .pop()
                            .unwrap()
                            .materialize(self.context.i32_type(), builder, self.context);

                        let can_emit_fast_path = [1, 2, 4, 8, 16].contains(&num_bytes);
                        if can_emit_fast_path && self.inline_memory_accesses {
                            let check_metadata_block = self
                                .context
                                .append_basic_block(function, &format!("check_metadata{unique_id}"));
                            let fast_write_block = self.context.append_basic_block(function, &format!("fast_write{unique_id}"));
                            let slow_write_block = self.context.append_basic_block(function, &format!("slow_write{unique_id}"));
                            let write_done_block = self.context.append_basic_block(function, &format!("write_done{unique_id}"));
                            unique_id += 1;

                            let alignment_bits = self.context.i32_type().const_int((num_bytes - 1) as u64, false);
                            let zero32 = self.context.i32_type().const_int(0, false);
                            let zero8 = self.context.i8_type().const_int(0, false);
                            let addr_alignment_bits = builder.build_and(addr, alignment_bits, "alignment_bits").unwrap();
                            let is_aligned = builder
                                .build_int_compare(IntPredicate::EQ, addr_alignment_bits, zero32, "is_aligned")
                                .unwrap();
                            builder
                                .build_conditional_branch(is_aligned, check_metadata_block, slow_write_block)
                                .unwrap();
                            builder.position_at_end(check_metadata_block);

                            let fast_mem_base_offset = offset_of!(Emulator, ctx.fast_memory.base);
                            let fast_mem_base_offset = self.context.i64_type().const_int(fast_mem_base_offset as u64, false);

                            let fast_mem_base_ptr = unsafe {
                                builder.build_in_bounds_gep(
                                    self.context.i8_type(),
                                    param_emulator,
                                    &[fast_mem_base_offset],
                                    "fast_mem_base_ptr",
                                )
                            }
                            .unwrap();
                            let fast_mem_base = builder
                                .build_load(ptr_ty, fast_mem_base_ptr, "fast_mem_base")
                                .unwrap()
                                .into_pointer_value();
                            let metadata_map_offset = self.context.i64_type().const_int(METADATA_SIZE.wrapping_neg(), true);
                            let metadata_map = unsafe {
                                builder.build_in_bounds_gep(
                                    self.context.i8_type(),
                                    fast_mem_base,
                                    &[metadata_map_offset],
                                    "metadata_map_addr",
                                )
                            }
                            .unwrap();
                            let metadata = {
                                let shift = self.context.i32_type().const_int(12, false);
                                let metadata_offset = builder.build_right_shift(addr, shift, false, "metdata_offset").unwrap();
                                let metadata_offset = builder
                                    .build_int_z_extend(metadata_offset, self.context.i64_type(), "metadata_offset_ext")
                                    .unwrap();
                                let ptr = unsafe {
                                    builder.build_in_bounds_gep(
                                        self.context.i8_type(),
                                        metadata_map,
                                        &[metadata_offset],
                                        "metadata_map_addr",
                                    )
                                }
                                .unwrap();
                                builder
                                    .build_load(self.context.i8_type(), ptr, "metadata")
                                    .unwrap()
                                    .into_int_value()
                            };
                            let system_test_value = MetadataTest::new().require_present().require_writable().as_bits();
                            let system_test_value_int = self.context.i8_type().const_int(system_test_value as u64, false);
                            let user_test_value = MetadataTest::new()
                                .require_present()
                                .require_writable()
                                .require_accessible_from_userspace()
                                .as_bits();
                            assert_eq!(
                                user_test_value ^ system_test_value,
                                1,
                                "userspace trap flag should be lowest bit"
                            );
                            let test_value = builder
                                .build_or(
                                    system_test_value_int,
                                    builder
                                        .build_int_z_extend(is_userspace, self.context.i8_type(), "is_userspace")
                                        .unwrap(),
                                    "metadata_test",
                                )
                                .unwrap();
                            let trap_bits = builder.build_and(test_value, metadata, "fast_write_nok").unwrap();
                            let should_trap = builder
                                .build_int_compare(IntPredicate::NE, trap_bits, zero8, "should_trap")
                                .unwrap();
                            builder
                                .build_conditional_branch(should_trap, slow_write_block, fast_write_block)
                                .unwrap();
                            builder.position_at_end(fast_write_block);

                            let fast_ok = self.context.bool_type().const_int(1, false);
                            {
                                let addr = builder.build_int_z_extend(addr, self.context.i64_type(), "addr_ext").unwrap();
                                let ptr = unsafe {
                                    builder.build_in_bounds_gep(self.context.i8_type(), fast_mem_base, &[addr], "addr")
                                }
                                .unwrap();
                                let ptr_ty = match num_bytes {
                                    1 => self.context.i8_type(),
                                    2 => self.context.i16_type(),
                                    4 => self.context.i32_type(),
                                    8 => self.context.i64_type(),
                                    16 => self.context.i128_type(),
                                    _ => unreachable!(),
                                };

                                builder
                                    .build_store(ptr, value.materialize(ptr_ty, builder, self.context))
                                    .unwrap();
                            }

                            builder.build_unconditional_branch(write_done_block).unwrap();
                            builder.position_at_end(slow_write_block);
                            let slow_ok = self.emit_slow_memory_write(param_ctx, is_userspace, num_bytes, value, addr);

                            let builder = &self.builder;
                            builder.build_unconditional_branch(write_done_block).unwrap();
                            builder.position_at_end(write_done_block);
                            let ok = {
                                assert_eq!(slow_ok.get_type(), fast_ok.get_type());
                                let phi = builder.build_phi(slow_ok.get_type(), "ok").unwrap();
                                phi.add_incoming(&[
                                    (&slow_ok as &dyn BasicValue, slow_write_block),
                                    (&fast_ok as &dyn BasicValue, fast_write_block),
                                ]);
                                phi.as_basic_value().into_int_value()
                            };

                            stack.push(Unmaterialized::new(ok));
                        } else {
                            let slow_ok = self.emit_slow_memory_write(param_ctx, is_userspace, num_bytes, value, addr);
                            stack.push(Unmaterialized::new(slow_ok));
                        }
                    },
                    LirOp::PortOut {
                        len,
                    } => {
                        let data = stack
                            .pop()
                            .unwrap()
                            .materialize(self.context.i128_type(), builder, self.context);
                        let port = stack
                            .pop()
                            .unwrap()
                            .materialize(self.context.i128_type(), builder, self.context);
                        let port16 = builder.build_int_truncate(port, self.context.i16_type(), "port16").unwrap();
                        let data32 = builder.build_int_truncate(data, self.context.i32_type(), "data32").unwrap();
                        let f = self.ftable.get(&mut (&mut *self.module, self.context), FunctionName::PortOut);
                        let call = builder
                            .build_call(
                                f,
                                &[
                                    param_ctx.into(),
                                    param_cpu_state.into(),
                                    port16.into(),
                                    self.context.i8_type().const_int(len as u64, false).into(),
                                    data32.into(),
                                ],
                                "portout",
                            )
                            .unwrap();
                        let ok_val = call.try_as_basic_value().basic().unwrap().into_int_value();
                        stack.push(Unmaterialized::new(ok_val));
                    },
                    LirOp::PortIn {
                        len,
                    } => {
                        let port = stack
                            .pop()
                            .unwrap()
                            .materialize(self.context.i128_type(), builder, self.context);
                        let port16 = builder.build_int_truncate(port, self.context.i16_type(), "port16").unwrap();
                        let f = self.ftable.get(&mut (&mut *self.module, self.context), FunctionName::PortIn);
                        // recall: declared as returning i64 packed (heuristic)
                        let call = builder
                            .build_call(
                                f,
                                &[
                                    param_ctx.into(),
                                    param_cpu_state.into(),
                                    port16.into(),
                                    self.context.i8_type().const_int(len as u64, false).into(),
                                ],
                                "portin",
                            )
                            .unwrap();

                        let returned_struct = call.try_as_basic_value().basic().unwrap();
                        let ok = builder
                            .build_extract_value(returned_struct.into_struct_value(), 0, "ok")
                            .unwrap()
                            .into_int_value(); // because bool is an integer type in LLVM

                        // Extract the second element (the u64)
                        let data_in = builder
                            .build_extract_value(returned_struct.into_struct_value(), 1, "value_read")
                            .unwrap()
                            .into_int_value(); // u64 is also an integer type

                        stack.push(Unmaterialized::new(ok));
                        stack.push(Unmaterialized::new(data_in));
                    },
                    LirOp::ReadDescriptor {
                        force,
                        mark_accessed,
                    } => {
                        let selector = stack
                            .pop()
                            .unwrap()
                            .materialize(self.context.i128_type(), builder, self.context);
                        let force_c = self.context.i8_type().const_int(force as u64, false);
                        let mark_c = self.context.i8_type().const_int(mark_accessed as u64, false);
                        let selector16 = builder
                            .build_int_truncate(selector, self.context.i16_type(), "sel16")
                            .unwrap();

                        let descriptor_ok_slot = builder.build_alloca(self.context.i8_type(), "descriptor_ok").unwrap();
                        let base_slot = builder.build_alloca(i64_ty, "base_slot").unwrap();
                        let limit_slot = builder.build_alloca(i64_ty, "limit_slot").unwrap();
                        let access_rights_slot = builder.build_alloca(i64_ty, "access_rights_slot").unwrap();

                        builder
                            .build_store(descriptor_ok_slot, self.context.i8_type().const_zero())
                            .unwrap();
                        builder.build_store(base_slot, self.context.i64_type().const_zero()).unwrap();
                        builder.build_store(limit_slot, self.context.i64_type().const_zero()).unwrap();
                        builder
                            .build_store(access_rights_slot, self.context.i64_type().const_zero())
                            .unwrap();

                        let args: &[BasicMetadataValueEnum] = &[
                            param_ctx.into(),
                            param_cpu_state.into(),
                            selector16.into(),
                            force_c.into(),
                            mark_c.into(),
                            descriptor_ok_slot.into(),
                            base_slot.into(),
                            limit_slot.into(),
                            access_rights_slot.into(),
                        ];
                        let f = self
                            .ftable
                            .get(&mut (&mut *self.module, self.context), FunctionName::ReadDescriptor);
                        let call = builder.build_call(f, args, "read_descriptor").unwrap();

                        // TODO: Figure out a way to pack the result into two u64 so we don't need stack allocations.
                        let execution_ok = call.try_as_basic_value().basic().unwrap().into_int_value();
                        let descriptor_ok_val = builder
                            .build_load(self.context.i64_type(), descriptor_ok_slot, "desc_ok_ld")
                            .unwrap()
                            .into_int_value();
                        let base_val = builder
                            .build_load(self.context.i64_type(), base_slot, "base_ld")
                            .unwrap()
                            .into_int_value();
                        let limit_val = builder
                            .build_load(self.context.i64_type(), limit_slot, "limit_ld")
                            .unwrap()
                            .into_int_value();
                        let access_rights_val = builder
                            .build_load(self.context.i64_type(), access_rights_slot, "acc_ld")
                            .unwrap()
                            .into_int_value();

                        stack.push(Unmaterialized::new(execution_ok));
                        stack.push(Unmaterialized::new(descriptor_ok_val));
                        stack.push(Unmaterialized::new(base_val));
                        stack.push(Unmaterialized::new(limit_val));
                        stack.push(Unmaterialized::new(access_rights_val));
                    },
                    LirOp::InstrLen => stack.push(Unmaterialized::new(instr_len)),
                    LirOp::PartValues => stack.push(Unmaterialized::new(param_part_values)),
                }
            }

            for next in block.next().iter() {
                for (var, block_var) in vars.iter().zip(block_vars[next.index()].iter_mut()) {
                    if let Some(val) = var {
                        // We need to ensure we materialize any complex operations inside this block, and just keep an int value.
                        block_var.push((self.builder.get_insert_block().unwrap(), val.clone()));
                    }
                }
            }

            // handle jump at end of block
            match block.next() {
                Jump::Next(next) => {
                    self.builder.build_unconditional_branch(blocks[next.index()]).unwrap();
                },
                Jump::Cond {
                    if_zero,
                    if_nonzero,
                } => {
                    let val = stack.pop().unwrap();
                    let cond_bool = val
                        .map_any_int_type(&self.builder, self.context, |val| {
                            self.builder
                                .build_int_compare(IntPredicate::NE, val, val.get_type().const_zero(), "brcond")
                                .unwrap()
                        })
                        .into_int_value(&self.builder, self.context);

                    let cond_bool = if likely[if_zero.index()] != likely[if_nonzero.index()] {
                        let i1_type = self.context.bool_type();
                        let intrinsic_name = "llvm.expect.i1";
                        let intrinsic = self.module.get_function(intrinsic_name).unwrap_or_else(|| {
                            let fn_type = i1_type.fn_type(&[i1_type.into(), i1_type.into()], false);
                            self.module.add_function(intrinsic_name, fn_type, None)
                        });

                        let expected_val = i1_type.const_int(likely[if_nonzero.index()] as u64, false);

                        let call_site = self
                            .builder
                            .build_call(intrinsic, &[cond_bool.into(), expected_val.into()], "expect")
                            .unwrap();

                        call_site.try_as_basic_value().basic().unwrap().into_int_value()
                    } else {
                        cond_bool
                    };

                    self.builder
                        .build_conditional_branch(cond_bool, blocks[if_nonzero.index()], blocks[if_zero.index()])
                        .unwrap();
                },
                Jump::Exit {
                    success,
                    metadata,
                    with_last_jump_condition,
                } => {
                    let last_jump_condition = if *with_last_jump_condition {
                        Some(stack.pop().unwrap())
                    } else {
                        None
                    };

                    if *success && !self.next_functions.is_empty() {
                        assert!(single_parameter_signature);
                        let return_normally = self
                            .context
                            .append_basic_block(self.function, &format!("return_normally_{}", block_id.index()));

                        if force_intr_check {
                            const INTR_COUNT_OFFSET: u32 = offset_of!(Emulator, intr.count) as u32;

                            let intr_check_done = self
                                .context
                                .append_basic_block(self.function, &format!("intr_check_done{}", block_id.index()));
                            let intr_check_if = self
                                .context
                                .append_basic_block(self.function, &format!("skip_intr_check{}", block_id.index()));

                            // We first check if INTR is non-zero.
                            // Since most of the time, IF is set, it is more efficient to first check INTR and only if INTR is set, check IF.
                            let intr_ptr = unsafe {
                                self.builder.build_in_bounds_gep(
                                    self.context.i8_type(),
                                    param_emulator,
                                    &[self.context.i32_type().const_int(INTR_COUNT_OFFSET as u64, false)],
                                    "intr_ptr",
                                )
                            }
                            .unwrap();

                            let intr = self.builder.build_load(self.context.i32_type(), intr_ptr, "intr").unwrap();
                            intr.as_instruction_value()
                                .unwrap()
                                .set_atomic_ordering(AtomicOrdering::Unordered)
                                .unwrap();
                            let intr_val = intr.into_int_value();
                            let intr_zero = self.context.i32_type().const_int(0, false);
                            let intr_is_zero = self
                                .builder
                                .build_int_compare(IntPredicate::EQ, intr_val, intr_zero, "intr_is_zero")
                                .unwrap();
                            self.builder
                                .build_conditional_branch(intr_is_zero, intr_check_done, intr_check_if)
                                .unwrap();
                            self.builder.position_at_end(intr_check_if);

                            // If INTR is non-zero, we need to check IF before determining whether to return.
                            // If IF is zero, no interrupts can occur and we do not need to return.
                            let if_offset = State::byte_offset_of(GpReg::Flags1.into()) + Intel386Flag::If as usize;
                            let if_addr = unsafe {
                                self.builder.build_in_bounds_gep(
                                    self.context.i8_type(),
                                    param_cpu_state,
                                    &[self.context.i32_type().const_int(if_offset as u64, false)],
                                    "if_ptr",
                                )
                            }
                            .unwrap();
                            let load = self.builder.build_load(self.context.i8_type(), if_addr, "load_if").unwrap();
                            let BasicValueEnum::IntValue(if_val) = load else {
                                panic!("load returned non-int")
                            };

                            let if_zero = self.context.i8_type().const_int(0, false);
                            let if_is_zero = self
                                .builder
                                .build_int_compare(IntPredicate::EQ, if_val, if_zero, "if_is_zero")
                                .unwrap();
                            self.builder
                                .build_conditional_branch(if_is_zero, intr_check_done, return_normally)
                                .unwrap();
                            self.builder.position_at_end(intr_check_done);
                        }

                        if let Some((functions, next)) = self.next_functions.get(&metadata.unwrap()) {
                            match next {
                                NextOnPage::FromCondition {
                                    condition_nonzero,
                                    condition_zero,
                                } => {
                                    let cond_zero = self
                                        .context
                                        .append_basic_block(self.function, &format!("next_cond_zero{}", block_id.index()));
                                    let cond_nonzero = self
                                        .context
                                        .append_basic_block(self.function, &format!("next_cond_nonzero{}", block_id.index()));
                                    let cond_is_zero = last_jump_condition
                                        .clone()
                                        .unwrap()
                                        .map_any_int_type(&self.builder, self.context, |val| {
                                            self.builder
                                                .build_int_compare(
                                                    IntPredicate::EQ,
                                                    val,
                                                    val.get_type().const_zero(),
                                                    "cond_is_zero",
                                                )
                                                .unwrap()
                                        })
                                        .into_int_value(&self.builder, self.context);
                                    self.builder
                                        .build_conditional_branch(cond_is_zero, cond_zero, cond_nonzero)
                                        .unwrap();

                                    self.builder.position_at_end(cond_zero);
                                    if let Some(next) = condition_zero {
                                        let function = functions.iter().find(|(offset, _)| *offset == next.offset).unwrap().1;
                                        let call = self
                                            .builder
                                            .build_call(function, &[param_emulator.into()], "next_res")
                                            .unwrap();
                                        call.set_tail_call(true);
                                        call.set_tail_call_kind(LLVMTailCallKind::LLVMTailCallKindMustTail);
                                        self.builder
                                            .build_return(Some(&call.try_as_basic_value().basic().unwrap().into_int_value()))
                                            .unwrap();
                                    } else {
                                        self.builder.build_unconditional_branch(return_normally).unwrap();
                                    }

                                    self.builder.position_at_end(cond_nonzero);
                                    if let Some(next) = condition_nonzero {
                                        let function = functions.iter().find(|(offset, _)| *offset == next.offset).unwrap().1;
                                        let call = self
                                            .builder
                                            .build_call(function, &[param_emulator.into()], "next_res")
                                            .unwrap();
                                        call.set_tail_call(true);
                                        call.set_tail_call_kind(LLVMTailCallKind::LLVMTailCallKindMustTail);
                                        self.builder
                                            .build_return(Some(&call.try_as_basic_value().basic().unwrap().into_int_value()))
                                            .unwrap();
                                    } else {
                                        self.builder.build_unconditional_branch(return_normally).unwrap();
                                    }
                                },
                                NextOnPage::Certain(_) => {
                                    assert_eq!(functions.len(), 1);
                                    let function = functions[0].1;
                                    let call = self
                                        .builder
                                        .build_call(function, &[param_emulator.into()], "next_res")
                                        .unwrap();
                                    call.set_tail_call(true);
                                    call.set_tail_call_kind(LLVMTailCallKind::LLVMTailCallKindMustTail);
                                    self.builder
                                        .build_return(Some(&call.try_as_basic_value().basic().unwrap().into_int_value()))
                                        .unwrap();
                                },
                                NextOnPage::Speculative(_) => {
                                    let current_pc = self.compute_pc(param_cpu_state);
                                    let check_next = self
                                        .context
                                        .append_basic_block(self.function, &format!("check_next_{}", block_id.index()));
                                    if let Some(initial_pc) = initial_pc {
                                        let page_mask = self.context.i32_type().const_int(0xfff, false);
                                        let differences =
                                            self.builder.build_xor(initial_pc, current_pc, "pc_differences").unwrap();
                                        let same_page = self
                                            .builder
                                            .build_int_compare(IntPredicate::ULE, differences, page_mask, "same_page")
                                            .unwrap();

                                        self.builder
                                            .build_conditional_branch(same_page, check_next, return_normally)
                                            .unwrap();
                                    } else {
                                        self.builder.build_unconditional_branch(check_next).unwrap();
                                    }

                                    self.builder.position_at_end(check_next);
                                    let page_mask = self.context.i32_type().const_int(0xfff, false);
                                    let current_page_offset =
                                        self.builder.build_and(current_pc, page_mask, "current_page_offset").unwrap();
                                    for &(offset, function) in functions.iter() {
                                        let perform_tail_call = self.context.append_basic_block(
                                            self.function,
                                            &format!("perform_tail_call{}_at{offset:X}", block_id.index()),
                                        );
                                        let no_match = self.context.append_basic_block(
                                            self.function,
                                            &format!("no_match{}_at{offset:X}", block_id.index()),
                                        );

                                        let offset_val = self.context.i32_type().const_int(offset as u64, false);
                                        let eq = self
                                            .builder
                                            .build_int_compare(IntPredicate::EQ, current_page_offset, offset_val, "eq")
                                            .unwrap();

                                        self.builder
                                            .build_conditional_branch(eq, perform_tail_call, no_match)
                                            .unwrap();
                                        self.builder.position_at_end(perform_tail_call);

                                        let call = self
                                            .builder
                                            .build_call(function, &[param_emulator.into()], "next_res")
                                            .unwrap();
                                        call.set_tail_call(true);
                                        call.set_tail_call_kind(LLVMTailCallKind::LLVMTailCallKindMustTail);
                                        self.builder
                                            .build_return(Some(&call.try_as_basic_value().basic().unwrap().into_int_value()))
                                            .unwrap();

                                        self.builder.position_at_end(no_match);
                                    }

                                    self.builder.build_unconditional_branch(return_normally).unwrap();
                                },
                            }
                        }

                        self.builder.position_at_end(return_normally);
                    }

                    let val = self
                        .context
                        .i64_type()
                        .const_int(*success as u64 | (metadata.unwrap_or(0) << 8), false);
                    let val = {
                        let cond = last_jump_condition
                            .map(|c| {
                                c.map_any_int_type(&self.builder, self.context, |val| {
                                    self.builder
                                        .build_int_compare(IntPredicate::NE, val, val.get_type().const_zero(), "cond_is_nonzero")
                                        .unwrap()
                                })
                                .into_int_value(&self.builder, self.context)
                            })
                            .unwrap_or(self.context.bool_type().const_zero());
                        let cond = self
                            .builder
                            .build_int_z_extend(cond, self.context.i64_type(), "cond_ext")
                            .unwrap();
                        let v = self
                            .builder
                            .build_left_shift(cond, self.context.i64_type().const_int(1, false), "shl")
                            .unwrap();
                        self.builder.build_or(v, val, "combined").unwrap()
                    };
                    self.builder.build_return(Some(&val)).unwrap();
                },
                Jump::Unreachable => panic!("unreachable blocks should have been removed"),
            }
        }
    }

    fn compute_pc(&self, param_cpu_state: PointerValue<'ctx>) -> IntValue<'ctx> {
        let ip_offset = State::byte_offset_of(GpReg::Ip.into());
        let ip_addr = unsafe {
            self.builder.build_in_bounds_gep(
                self.context.i8_type(),
                param_cpu_state,
                &[self.context.i32_type().const_int(ip_offset as u64, false)],
                "addr_imm",
            )
        }
        .unwrap();
        let load = self
            .builder
            .build_load(self.context.i32_type(), ip_addr, "load_ptr_imm")
            .unwrap();
        let BasicValueEnum::IntValue(ip) = load else {
            panic!("load returned non-int")
        };

        let cs_offset = State::byte_offset_of(GpReg::CsBase.into());
        let cs_addr = unsafe {
            self.builder.build_in_bounds_gep(
                self.context.i8_type(),
                param_cpu_state,
                &[self.context.i32_type().const_int(cs_offset as u64, false)],
                "addr_imm",
            )
        }
        .unwrap();
        let load = self
            .builder
            .build_load(self.context.i32_type(), cs_addr, "load_ptr_imm")
            .unwrap();
        let BasicValueEnum::IntValue(cs) = load else {
            panic!("load returned non-int")
        };

        self.builder.build_int_add(ip, cs, "pc").unwrap()
    }

    fn invoke_op_fn<const N: usize, T: Into<BasicMetadataValueEnum<'ctx>>>(
        &mut self, args: [T; N], name: FunctionName,
    ) -> Unmaterialized<'ctx> {
        let f = self.ftable.get(&mut (&mut *self.module, self.context), name);
        let args = args.into_iter().map(|arg| arg.into()).collect::<ArrayVec<_, 4>>();
        let call = self
            .builder
            .build_call(f, &args, &format!("result_{name:?}").to_lowercase())
            .unwrap();
        Unmaterialized::new(call.try_as_basic_value().basic().unwrap().into_int_value())
    }

    fn is_userspace(&self, param_cpu_state: PointerValue<'ctx>) -> IntValue<'ctx> {
        // CPL contains a cached copy of `CPL != 0` at byte offset 1.
        // This saves us a few comparison instructions when we need to pass `is_userspace as an argument`.
        let cpl_offset = State::byte_offset_of(Reg::Gp(GpReg::Cpl)) as u32;
        let ptr = unsafe {
            self.builder.build_in_bounds_gep(
                self.context.i8_type(),
                param_cpu_state,
                &[self.context.i32_type().const_int(cpl_offset as u64 + 1, false)],
                "is_userspace_ptr",
            )
        }
        .unwrap();
        self.builder
            .build_load(self.context.bool_type(), ptr, "is_userspace_load")
            .unwrap()
            .into_int_value()
    }

    fn emit_slow_memory_write(
        &mut self, param_ctx: PointerValue<'ctx>, is_userspace: IntValue<'ctx>, num_bytes: u8, value: Unmaterialized<'ctx>,
        addr32: IntValue<'ctx>,
    ) -> IntValue<'ctx> {
        let ok = match num_bytes {
            2 => {
                let f = self
                    .ftable
                    .get(&mut (&mut *self.module, self.context), FunctionName::MemWrite2Simple);
                let call = self
                    .builder
                    .build_call(
                        f,
                        &[
                            param_ctx.into(),
                            addr32.into(),
                            is_userspace.into(),
                            value.materialize(self.context.i16_type(), &self.builder, self.context).into(),
                        ],
                        "memwrite",
                    )
                    .unwrap();
                call.try_as_basic_value().basic().unwrap().into_int_value()
            },
            4 => {
                let f = self
                    .ftable
                    .get(&mut (&mut *self.module, self.context), FunctionName::MemWrite4Simple);
                let call = self
                    .builder
                    .build_call(
                        f,
                        &[
                            param_ctx.into(),
                            addr32.into(),
                            is_userspace.into(),
                            value.materialize(self.context.i32_type(), &self.builder, self.context).into(),
                        ],
                        "memwrite",
                    )
                    .unwrap();
                call.try_as_basic_value().basic().unwrap().into_int_value()
            },
            _ => {
                let len = self.context.i8_type().const_int(num_bytes as u64, false);
                let value = match num_bytes {
                    1 => {
                        let value = value.materialize(self.context.i8_type(), &self.builder, self.context);
                        self.builder
                            .build_int_z_extend(value, self.context.i128_type(), "value128")
                            .unwrap()
                    },
                    2 => {
                        let value = value.materialize(self.context.i16_type(), &self.builder, self.context);
                        self.builder
                            .build_int_z_extend(value, self.context.i128_type(), "value128")
                            .unwrap()
                    },
                    3 | 4 => {
                        let value = value.materialize(self.context.i32_type(), &self.builder, self.context);
                        self.builder
                            .build_int_z_extend(value, self.context.i128_type(), "value128")
                            .unwrap()
                    },
                    5..=8 => {
                        let value = value.materialize(self.context.i64_type(), &self.builder, self.context);
                        self.builder
                            .build_int_z_extend(value, self.context.i128_type(), "value128")
                            .unwrap()
                    },
                    9..=16 => value.materialize(self.context.i128_type(), &self.builder, self.context),
                    _ => todo!(),
                };

                let f = self
                    .ftable
                    .get(&mut (&mut *self.module, self.context), FunctionName::MemWriteSimple);
                let call = self
                    .builder
                    .build_call(
                        f,
                        &[param_ctx.into(), addr32.into(), len.into(), is_userspace.into(), value.into()],
                        "memwrite",
                    )
                    .unwrap();
                call.try_as_basic_value().basic().unwrap().into_int_value()
            },
        };

        self.builder.build_int_truncate(ok, self.context.bool_type(), "trok").unwrap()
    }

    fn emit_slow_memory_read(
        &mut self, param_ctx: PointerValue<'ctx>, u128_return_val_slot: &Option<PointerValue<'ctx>>,
        is_userspace: IntValue<'ctx>, num_bytes: u8, addr: IntValue<'ctx>,
    ) -> (IntValue<'ctx>, IntValue<'ctx>) {
        match num_bytes {
            1 => {
                let f = self
                    .ftable
                    .get(&mut (&mut *self.module, self.context), FunctionName::MemRead1Simple);
                let call = self
                    .builder
                    .build_call(f, &[param_ctx.into(), addr.into(), is_userspace.into()], "memread")
                    .unwrap();

                let ret = call.try_as_basic_value().basic().unwrap().into_int_value();
                let ok = self
                    .builder
                    .build_int_compare(IntPredicate::NE, ret, self.context.i16_type().const_int(0, false), "ok")
                    .unwrap();
                let loaded = self.builder.build_int_truncate(ret, self.context.i8_type(), "tr").unwrap();

                (ok, loaded)
            },
            2 => {
                let f = self
                    .ftable
                    .get(&mut (&mut *self.module, self.context), FunctionName::MemRead2Simple);
                let call = self
                    .builder
                    .build_call(f, &[param_ctx.into(), addr.into(), is_userspace.into()], "memread")
                    .unwrap();

                let ret = call.try_as_basic_value().basic().unwrap().into_int_value();
                let ok = self
                    .builder
                    .build_int_compare(IntPredicate::NE, ret, self.context.i32_type().const_int(0, false), "ok")
                    .unwrap();
                let loaded = self.builder.build_int_truncate(ret, self.context.i16_type(), "tr").unwrap();

                (ok, loaded)
            },
            4 => {
                let f = self
                    .ftable
                    .get(&mut (&mut *self.module, self.context), FunctionName::MemRead4Simple);
                let call = self
                    .builder
                    .build_call(f, &[param_ctx.into(), addr.into(), is_userspace.into()], "memread")
                    .unwrap();
                let ret = call.try_as_basic_value().basic().unwrap().into_int_value();
                let ok = self
                    .builder
                    .build_int_compare(IntPredicate::NE, ret, self.context.i64_type().const_int(0, false), "ok")
                    .unwrap();
                let loaded = self.builder.build_int_truncate(ret, self.context.i32_type(), "tr").unwrap();

                (ok, loaded)
            },
            _ => {
                let len = self.context.i8_type().const_int(num_bytes as u64, false);
                let f = self
                    .ftable
                    .get(&mut (&mut *self.module, self.context), FunctionName::MemReadSimple);
                let u128_return_val_slot = u128_return_val_slot.unwrap();
                let call = self
                    .builder
                    .build_call(
                        f,
                        &[
                            param_ctx.into(),
                            addr.into(),
                            len.into(),
                            is_userspace.into(),
                            u128_return_val_slot.into(),
                        ],
                        "memread",
                    )
                    .unwrap();

                let ok_val = call.try_as_basic_value().basic().unwrap().into_int_value();
                let ok_val = self
                    .builder
                    .build_int_truncate(ok_val, self.context.bool_type(), "trok")
                    .unwrap();

                let loaded = self
                    .builder
                    .build_load(self.context.i128_type(), u128_return_val_slot, "u128_loaded")
                    .unwrap()
                    .into_int_value();

                let loaded = match num_bytes {
                    1 => self
                        .builder
                        .build_int_truncate(loaded, self.context.i8_type(), "tr8")
                        .unwrap(),
                    2 => self
                        .builder
                        .build_int_truncate(loaded, self.context.i16_type(), "tr16")
                        .unwrap(),
                    3 | 4 => self
                        .builder
                        .build_int_truncate(loaded, self.context.i32_type(), "tr32")
                        .unwrap(),
                    5..=8 => self
                        .builder
                        .build_int_truncate(loaded, self.context.i64_type(), "tr64")
                        .unwrap(),
                    9..=16 => loaded,
                    _ => panic!("unexpected memory size: {num_bytes}"),
                };

                (ok_val, loaded)
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::mem::offset_of;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::mpsc::channel;

    use generativity::make_guard;
    use sem86_arch::mem::{Mem32, Shm};

    use crate::arch::intel386::Intel386;
    use crate::codegen::backends::inkwell::{InkwellBackend, InkwellContext};
    use crate::codegen::see::SingleEncodingExecution;
    use crate::decoder::PackedInstrSem;
    use crate::emulator::exec::ExecutionContext;
    use crate::hw::Hw;
    use crate::hw::intr::Intr;
    use crate::icache::InstructionCache;
    use crate::time::EmulatorClock;

    #[test]
    fn fast_mem_base_offset_is_correct() {
        let shm = Arc::new(Shm::new("bench", 64 << 20)); // 64MiB
        let mem = Arc::new(Mem32::new(shm.clone()));

        let intr = Intr::new();
        let intr = Pin::new(&intr);
        let hw = Hw::new(
            mem.clone(),
            Vec::new(),
            channel().0,
            channel().1,
            Arc::new(Shm::new("vgabios", 16)),
            Intr::handle(intr),
            EmulatorClock::new_asynchronous(),
        );
        make_guard!(guard);
        let cache = InstructionCache::new(
            guard,
            Arc::new(PackedInstrSem::empty()),
            SingleEncodingExecution::new(InkwellBackend::new(InkwellContext::leak_new()), 0),
        );
        let mut ctx = ExecutionContext::new(hw, &mem, None, cache);
        ctx.protected_mode = true;
        let fast_mem_base_offset = offset_of!(ExecutionContext<'_, '_, Intel386>, fast_memory.base);
        println!("Offset: {fast_mem_base_offset}");
        let base = unsafe {
            let ctx = &mut ctx;
            let ctx = ctx as *mut _ as *mut *mut u8;
            let mem_base_offset = ctx.byte_add(fast_mem_base_offset);
            mem_base_offset.read()
        };

        assert_eq!(ctx.fast_memory.base, base);
    }
}
