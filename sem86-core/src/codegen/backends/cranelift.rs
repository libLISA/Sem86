use std::error::Error;
use std::ffi::c_void;
use std::fmt::{Debug, Display};
use std::mem::{self, offset_of};

use cranelift::codegen::ir::{FuncRef, UserFuncName};
use cranelift::codegen::{CodegenError, Context, write_function};
use cranelift::prelude::*;
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{FuncId, Linkage, Module, ModuleError, default_libcall_names};
use liblisa::utils::bitmask_u128;
use log::{debug, error, log_enabled, trace};

use crate::arch::intel386::{GpReg, Intel386, Reg, State};
use crate::codegen::backends::{Backend, BackendFn, JitExecutionResult, TracedAccess, UninstantiatedBackendFn};
use crate::codegen::functions::{FnDeclBackend, FunctionName, FunctionTable, Primitive, Ty};
use crate::codegen::lir::{Jump, Lir, LirOp};
use crate::codegen::{DataSize, Ptr};
use crate::emulator::Emulator;
use crate::emulator::exec::ExecutionContext;
use crate::il::part_values::PartValues;
use crate::il::{BinOp, UnOp};

type UninstantiatedJitFn =
    fn(ctx: &mut ExecutionContext<'_, '_, Intel386>, cpu: &mut State, instr_len: u8, part_values: PartValues) -> u64;

#[derive(Copy, Clone)]
pub struct CraneliftFunction {
    // TODO: SAFETY: make sure we don't drop JITModule in CraneliftBackend while still having a pointer to it in `f`.
    f: UninstantiatedJitFn,
    num_ops: usize,
}

unsafe impl Send for CraneliftFunction {}
unsafe impl Sync for CraneliftFunction {}

impl Debug for CraneliftFunction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CraneliftFunction").finish()
    }
}

impl CraneliftFunction {
    pub fn num_ops(&self) -> usize {
        self.num_ops
    }

    pub fn get_assembly(&self) -> &[u8] {
        let ptr = self.f as *const u8;
        let safe_read_len = 0x1000 - ((ptr as u64) & 0xfff);

        // All reads are on the same page, guaranteeing that they won't pagefault.
        // We never modify this memory after codegen, so it's safe to hand out references.
        unsafe { std::slice::from_raw_parts(ptr, safe_read_len as usize) }
    }
}

impl BackendFn for CraneliftFunction {
    #[inline(always)]
    fn execute(&self, emulator: &mut Emulator<'_, '_>, _trace_memory: impl FnMut(TracedAccess)) -> (JitExecutionResult, u64) {
        let result = (self.f)(&mut emulator.ctx, &mut emulator.cpu, 0, PartValues::ALL_ZERO);

        (JitExecutionResult::from(result as u8), result >> 8)
    }

    unsafe fn from_ptr(_ptr: *mut u8) -> Self {
        todo!()
    }
}

impl UninstantiatedBackendFn for CraneliftFunction {
    #[inline(always)]
    fn execute_uninstantiated(
        &self, emulator: &mut Emulator<'_, '_>, instr_len: u8, part_values: PartValues, _trace_memory: impl FnMut(TracedAccess),
    ) -> JitExecutionResult {
        JitExecutionResult::from((self.f)(&mut emulator.ctx, &mut emulator.cpu, instr_len, part_values) as u8)
    }

    unsafe fn from_ptr(_ptr: *mut u8) -> Self {
        todo!()
    }
}

impl Default for CraneliftFunction {
    fn default() -> Self {
        fn empty(_: &mut ExecutionContext<'_, '_, Intel386>, _: &mut State, _: u8, _: PartValues) -> u64 {
            1
        }

        Self {
            f: empty,
            num_ops: 0,
        }
    }
}

#[derive(Debug)]
pub enum CraneliftError {
    TooBig,
}

impl Display for CraneliftError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CraneliftError::TooBig => write!(f, "code too big"),
        }
    }
}

impl Error for CraneliftError {}

impl<'a, 'ctx> FnDeclBackend for (&'a mut JITModule, &'a mut FunctionBuilder<'ctx>, Type) {
    type FuncId = FuncId;
    type Output = FuncRef;

    fn declare_function(&mut self, symbol: &str, _ptr: *const c_void, params: &[Ty], returns: Option<Ty>) -> Self::FuncId {
        fn map_primitive(pointer_ty: Type, primitive: Primitive) -> AbiParam {
            AbiParam::new(match primitive {
                Primitive::U8 => types::I8,
                Primitive::U16 => types::I16,
                Primitive::U32 => types::I32,
                Primitive::U64 => types::I64,
                Primitive::U128 => types::I128,
                Primitive::Ptr {
                    ..
                } => pointer_ty,
            })
        }

        let pointer_ty = self.2;
        let mut signature = self.0.make_signature();
        signature.params = params
            .iter()
            .map(|ty| match ty {
                Ty::Primitive(primitive) => map_primitive(pointer_ty, *primitive),
                Ty::Struct {
                    ..
                } => todo!(),
            })
            .collect::<Vec<_>>();
        signature.returns = match returns {
            Some(Ty::Primitive(primitive)) => vec![map_primitive(pointer_ty, primitive)],
            Some(Ty::Struct {
                fields,
            }) => fields.iter().map(|&primitive| map_primitive(pointer_ty, primitive)).collect(),
            None => Vec::new(),
        };

        debug!("Function {symbol} has signature: {signature:#?}");

        self.0.declare_function(symbol, Linkage::Import, &signature).unwrap()
    }

    fn postprocess(&mut self, id: Self::FuncId) -> Self::Output {
        self.0.declare_func_in_func(id, self.1.func)
    }
}

pub struct CraneliftBackend {
    modules_finished: Vec<JITModule>,
    module: JITModule,
    pointer_ty: Type,
    ftable: FunctionTable<FuncId>,
    num_generated: usize,
    optimize: bool,
}

impl Backend for CraneliftBackend {
    type Fn = CraneliftFunction;
    type UninstantiatedFn = CraneliftFunction;
    type Error = CraneliftError;

    fn codegen_lir(&mut self, lir: &Lir) -> Result<Self::UninstantiatedFn, Self::Error> {
        self.codegen_lir(lir)
    }

    fn codegen_lir_object(&mut self, _lir: &Lir) -> Result<crate::codegen::mm::Object, Self::Error> {
        todo!()
    }
}

impl CraneliftBackend {
    fn prepare_new_module(optimize: bool) -> (Type, JITModule, FunctionTable<FuncId>) {
        let mut flag_builder = settings::builder();
        flag_builder.set("use_colocated_libcalls", "false").unwrap();
        flag_builder
            .set("opt_level", if optimize { "speed" } else { "none" })
            .unwrap();
        flag_builder.set("enable_llvm_abi_extensions", "true").unwrap();
        flag_builder.set("is_pic", "false").unwrap();

        let isa_builder = cranelift_native::builder().unwrap();
        let isa = isa_builder.finish(settings::Flags::new(flag_builder)).unwrap();
        let pointer_ty = isa.pointer_type();

        let mut jit_builder = JITBuilder::with_isa(isa, default_libcall_names());
        let ftable = FunctionTable::new();
        for (symbol, ptr) in FunctionTable::<FuncId>::symbols_and_ptrs() {
            jit_builder.symbol(symbol, ptr as *const u8);
        }

        let module = JITModule::new(jit_builder);
        (pointer_ty, module, ftable)
    }

    pub fn new(optimize: bool) -> Self {
        let (pointer_ty, module, ftable) = Self::prepare_new_module(optimize);

        Self {
            module,
            modules_finished: Vec::new(),
            ftable,
            pointer_ty,
            num_generated: 0,
            optimize,
        }
    }

    pub fn codegen_lir(&mut self, lir: &Lir) -> Result<CraneliftFunction, CraneliftError> {
        let mut ctx = self.module.make_context();
        let func = self.build_lir(&mut ctx, lir);

        if log_enabled!(log::Level::Trace) {
            let mut ir_str = String::new();
            write_function(&mut ir_str, &ctx.func).unwrap();

            trace!("IR: {ir_str}");
        }

        match self.module.define_function(func, &mut ctx) {
            Ok(()) => (),
            Err(ModuleError::Compilation(CodegenError::CodeTooLarge)) => return Err(CraneliftError::TooBig),
            Err(e) => {
                error!("Error: {e}, with LIR: {lir:#?}");

                let e = e.to_string();
                std::panic::catch_unwind(|| {
                    let mut ir_str = String::new();
                    write_function(&mut ir_str, &ctx.func).unwrap();
                    error!("Error: {e}\n{e:#?}\n{ir_str}");
                })
                .ok();
            },
        }

        self.module.clear_context(&mut ctx);

        // Perform linking.
        self.module.finalize_definitions().expect("Failed to finalize definitions");

        // Get a raw pointer to the generated code.
        let code_ptr = self.module.get_finalized_function(func);

        self.num_generated += 1;
        if self.num_generated.is_multiple_of(50) {
            let (pointer_ty, mut module, ftable) = Self::prepare_new_module(self.optimize);
            std::mem::swap(&mut module, &mut self.module);
            self.modules_finished.push(module);
            self.ftable = ftable;
            self.pointer_ty = pointer_ty;
        }

        // Cast it to a rust function pointer type.
        Ok(CraneliftFunction {
            f: unsafe { mem::transmute::<*const u8, UninstantiatedJitFn>(code_ptr) },
            num_ops: lir.num_ops(),
        })
    }

    fn u128_const(builder: &mut FunctionBuilder, val: u128) -> Value {
        let lo = val as u64;
        let hi = (val >> 64) as u64;

        let lo_val = builder.ins().iconst(types::I64, lo as i64);
        if hi == 0 {
            builder.ins().uextend(types::I128, lo_val)
        } else {
            let hi_val = builder.ins().iconst(types::I64, hi as i64);

            builder.ins().iconcat(lo_val, hi_val)
        }
    }

    fn invoke_op_function(&mut self, builder: &mut FunctionBuilder, args: &[Value], name: FunctionName) -> Value {
        let f = self.ftable.get(&mut (&mut self.module, &mut *builder, self.pointer_ty), name);
        let inst = builder.ins().call(f, args);
        let [result] = builder.inst_results(inst) else {
            panic!("Wrong number of return values for {name:?}");
        };

        *result
    }

    fn build_lir(&mut self, ctx: &mut Context, lir: &Lir) -> FuncId {
        let mut func_ctx = FunctionBuilderContext::new();
        let mut signature = self.module.make_signature();
        signature.params = vec![
            AbiParam::new(self.pointer_ty),
            AbiParam::new(self.pointer_ty),
            AbiParam::new(types::I8),
            AbiParam::new(types::I128),
        ];
        signature.returns = vec![AbiParam::new(types::I16)];

        let func = self
            .module
            .declare_function(&format!("JIT_block_{}", self.num_generated), Linkage::Local, &signature)
            .unwrap();
        ctx.func.signature = signature;
        ctx.func.name = UserFuncName::user(0, func.as_u32());

        let mut builder: FunctionBuilder = FunctionBuilder::new(&mut ctx.func, &mut func_ctx);
        let entry_block = builder.create_block();
        builder.switch_to_block(entry_block);
        builder.append_block_params_for_function_params(entry_block);
        let [param_ctx, param_cpu_state, param_instr_len, param_part_values] = *builder.block_params(entry_block) else {
            unreachable!("function parameters don't match expected count");
        };

        let instr_len = builder.ins().uextend(types::I128, param_instr_len);

        let cpl = builder.ins().load(
            types::I64,
            MemFlags::new(),
            param_cpu_state,
            State::byte_offset_of(Reg::Gp(GpReg::Cpl)) as i32,
        );
        let one = builder.ins().iconst(types::I8, 1);
        let zero = builder.ins().iconst(types::I8, 0);
        let is_userspace = builder.ins().select(cpl, one, zero);

        let u128_return_val_slot = builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 16, 16));
        let u128_return_val_slot_addr = builder.ins().stack_addr(self.pointer_ty, u128_return_val_slot, 0);

        let descriptor_ok_slot = builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 8));
        let base_slot = builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 8));
        let limit_slot = builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 8));
        let access_rights_slot = builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 8));

        let mut set_exception_fn = None;
        let mut set_handler_fn = None;
        let mut mem_write_fn = None;
        let mut mem_read_fn = None;
        let mut port_out_fn = None;
        let mut port_in_fn = None;
        let mut read_descriptor_fn = None;

        let block_ids = lir.blocks.iter().map(|_| builder.create_block()).collect::<Vec<_>>();
        let def_ids = (0..lir.num_defs())
            .map(|_| builder.declare_var(types::I128))
            .collect::<Vec<_>>();
        builder.ins().jump(block_ids[0], &[]);
        for (block, &id) in lir.blocks.iter().zip(block_ids.iter()) {
            builder.switch_to_block(id);

            let mut stack = Vec::new();
            for &op in block.operations().iter() {
                match op {
                    LirOp::Const(const_id) => stack.push(Self::u128_const(&mut builder, lir[const_id])),
                    LirOp::Load(def_id) => {
                        stack.push(builder.use_var(def_ids[def_id.index()]));
                    },
                    LirOp::Store(def_id) => {
                        let val = stack.pop().unwrap();
                        builder.def_var(def_ids[def_id.index()], val);
                    },
                    LirOp::BinOp(bin_op) => {
                        let rhs = stack.pop().unwrap();
                        let lhs = stack.pop().unwrap();

                        stack.push(match bin_op {
                            BinOp::Add => builder.ins().iadd(lhs, rhs),
                            BinOp::Sub => builder.ins().isub(lhs, rhs),
                            BinOp::Mul => builder.ins().imul(lhs, rhs),
                            BinOp::Xor => builder.ins().bxor(lhs, rhs),
                            BinOp::Or => builder.ins().bor(lhs, rhs),
                            BinOp::And => builder.ins().band(lhs, rhs),
                            BinOp::Shl => builder.ins().ishl(lhs, rhs),
                            BinOp::Shr => builder.ins().ushr(lhs, rhs),
                            BinOp::Rol(num_bits) => {
                                let rhs = builder.ins().ireduce(types::I64, rhs);
                                let shift = builder.ins().urem_imm(rhs, num_bits as i64);
                                let num_bits_val = builder.ins().iconst(types::I64, num_bits as i64);
                                let inv_shift = builder.ins().isub(num_bits_val, shift);
                                let a = builder.ins().ishl(lhs, shift);
                                let b = builder.ins().ushr(lhs, inv_shift);

                                builder.ins().bor(a, b)
                            },
                            BinOp::Ror(num_bits) => {
                                let rhs = builder.ins().ireduce(types::I64, rhs);
                                let shift = builder.ins().urem_imm(rhs, num_bits as i64);
                                let num_bits_val = builder.ins().iconst(types::I64, num_bits as i64);
                                let inv_shift = builder.ins().isub(num_bits_val, shift);
                                let a = builder.ins().ushr(lhs, shift);
                                let b = builder.ins().ishl(lhs, inv_shift);

                                builder.ins().bor(a, b)
                            },
                            BinOp::Sar(n) => {
                                let lhs = builder.ins().ireduce(
                                    match n {
                                        1 => types::I8,
                                        2 => types::I16,
                                        4 => types::I32,
                                        _ => unimplemented!(),
                                    },
                                    lhs,
                                );
                                let result = builder.ins().sshr(lhs, rhs);
                                builder.ins().sextend(types::I128, result)
                            },
                            BinOp::Div => {
                                // TODO: 128-bit support
                                let lhs = builder.ins().ireduce(types::I64, lhs);
                                let rhs = builder.ins().ireduce(types::I64, rhs);
                                let val = builder.ins().udiv(lhs, rhs);
                                builder.ins().uextend(types::I128, val)
                            },
                            BinOp::Mod => {
                                // TODO: 128-bit support
                                let lhs = builder.ins().ireduce(types::I64, lhs);
                                let rhs = builder.ins().ireduce(types::I64, rhs);
                                let val = builder.ins().urem(lhs, rhs);
                                builder.ins().uextend(types::I128, val)
                            },
                            BinOp::SignedDiv64 => {
                                let lhs = builder.ins().ireduce(types::I64, lhs);
                                let rhs = builder.ins().ireduce(types::I64, rhs);
                                let val = builder.ins().sdiv(lhs, rhs);
                                builder.ins().uextend(types::I128, val)
                            },
                            BinOp::SignedMod64 => {
                                let lhs = builder.ins().ireduce(types::I64, lhs);
                                let rhs = builder.ins().ireduce(types::I64, rhs);
                                let val = builder.ins().srem(lhs, rhs);
                                builder.ins().uextend(types::I128, val)
                            },
                            BinOp::CmpGt => {
                                let val = builder.ins().icmp(IntCC::UnsignedGreaterThan, lhs, rhs);
                                builder.ins().uextend(types::I128, val)
                            },
                            BinOp::CmpLt => {
                                let val = builder.ins().icmp(IntCC::UnsignedLessThan, lhs, rhs);
                                builder.ins().uextend(types::I128, val)
                            },
                            BinOp::CmpEq => {
                                let val = builder.ins().icmp(IntCC::Equal, lhs, rhs);
                                builder.ins().uextend(types::I128, val)
                            },
                        });
                    },
                    LirOp::FpBinOp(bin_op) => {
                        let rc = stack.pop().unwrap();
                        let rc = builder.ins().ireduce(types::I8, rc);
                        let rhs = stack.pop().unwrap();
                        let lhs = stack.pop().unwrap();

                        stack.push(self.invoke_op_function(&mut builder, &[lhs, rhs, rc], bin_op.into()));
                    },
                    LirOp::Blend(mask) => {
                        let mask = lir[mask];
                        let new = stack.pop().unwrap();
                        let old = stack.pop().unwrap();

                        stack.push(if mask >> 64 == 0 {
                            let hi = builder.ins().ushr_imm(old, 64);
                            let hi = builder.ins().ireduce(types::I64, hi);

                            let mask = builder.ins().iconst(types::I64, mask as u64 as i64);
                            let old = builder.ins().ireduce(types::I64, old);
                            let new = builder.ins().ireduce(types::I64, new);
                            let lo = builder.ins().bitselect(mask, new, old);

                            builder.ins().iconcat(lo, hi)
                        } else {
                            let mask_val = Self::u128_const(&mut builder, mask);
                            let inv_mask_val = Self::u128_const(&mut builder, !mask);
                            // (mask_val & new) | (~mask_val & old)
                            let new = builder.ins().band(new, mask_val);
                            let old = builder.ins().band(old, inv_mask_val);

                            builder.ins().bor(new, old)
                        })
                    },
                    LirOp::Extract {
                        skip,
                        take,
                    } => {
                        let val = stack.pop().unwrap();
                        let val = builder.ins().ushr_imm(val, skip as i64);
                        stack.push(match take {
                            8 => {
                                let val = builder.ins().ireduce(types::I8, val);
                                builder.ins().uextend(types::I128, val)
                            },
                            16 => {
                                let val = builder.ins().ireduce(types::I16, val);
                                builder.ins().uextend(types::I128, val)
                            },
                            32 => {
                                let val = builder.ins().ireduce(types::I32, val);
                                builder.ins().uextend(types::I128, val)
                            },
                            64 => {
                                let val = builder.ins().ireduce(types::I64, val);
                                builder.ins().uextend(types::I128, val)
                            },
                            _ => {
                                let mask = Self::u128_const(&mut builder, bitmask_u128(take as u32));
                                builder.ins().band(val, mask)
                            },
                        });
                    },
                    LirOp::UnOp(un_op) => {
                        let arg = stack.pop().unwrap();
                        stack.push(match un_op {
                            UnOp::Id => arg,
                            UnOp::ByteSwap16 => {
                                let val = builder.ins().ireduce(types::I16, arg);
                                let val = builder.ins().bswap(val);
                                builder.ins().uextend(types::I128, val)
                            },
                            UnOp::ByteSwap32 => {
                                let val = builder.ins().ireduce(types::I32, arg);
                                let val = builder.ins().bswap(val);
                                builder.ins().uextend(types::I128, val)
                            },
                            UnOp::ByteSwap64 => {
                                let val = builder.ins().ireduce(types::I64, arg);
                                let val = builder.ins().bswap(val);
                                builder.ins().uextend(types::I128, val)
                            },
                            UnOp::IsZero => {
                                let val = builder.ins().icmp_imm(IntCC::Equal, arg, 0);
                                builder.ins().uextend(types::I128, val)
                            },
                            UnOp::SelectBit(n) => {
                                let bit_val = builder.ins().ushr_imm(arg, n as i64);
                                builder.ins().band_imm(bit_val, 1)
                            },
                            UnOp::Parity => {
                                let val = builder.ins().ireduce(types::I8, arg);
                                let val = builder.ins().popcnt(val);
                                let val = builder.ins().band_imm(val, 1);
                                let val = builder.ins().bxor_imm(val, 1);
                                builder.ins().uextend(types::I128, val)
                            },
                            UnOp::TrailingZeros => builder.ins().ctz(arg),
                            UnOp::HighestBitSet => {
                                let max = builder.ins().iconst(types::I64, 127);
                                let val = builder.ins().clz(arg);
                                let val = builder.ins().ireduce(types::I64, val);
                                let result = builder.ins().isub(max, val);
                                builder.ins().uextend(types::I128, result)
                            },
                            UnOp::SignExtend(n) => {
                                let s = (128 - n) as i64;
                                let val = builder.ins().ishl_imm(arg, s);
                                builder.ins().sshr_imm(val, s)
                            },
                        });
                    },
                    LirOp::FpUnOp(un_op) => {
                        let arg = stack.pop().unwrap();
                        stack.push(self.invoke_op_function(&mut builder, &[arg], un_op.into()));
                    },
                    LirOp::Ite => {
                        let cond = stack.pop().unwrap();
                        let if_zero = stack.pop().unwrap();
                        let if_nonzero = stack.pop().unwrap();

                        stack.push(builder.ins().select(cond, if_nonzero, if_zero));
                    },
                    LirOp::LoadPtrWithOffset {
                        ptr,
                        size,
                    } => {
                        let offset = builder.ins().ireduce(self.pointer_ty, stack.pop().unwrap());
                        let ptr = match ptr {
                            Ptr::CpuState => param_cpu_state,
                            Ptr::K => builder
                                .ins()
                                .iadd_imm(param_ctx, offset_of!(ExecutionContext<'_, '_, Intel386>, k) as i64),
                        };

                        let ptr = builder.ins().iadd(ptr, offset);
                        let ty = match size {
                            DataSize::Byte => types::I8,
                            DataSize::Word => types::I16,
                            DataSize::Dword => types::I32,
                            DataSize::Qword => types::I64,
                            DataSize::F80 => todo!(),
                            DataSize::Oword => types::I128,
                        };

                        let val = builder.ins().load(ty, MemFlags::new(), ptr, 0);
                        stack.push(if ty != types::I128 {
                            builder.ins().uextend(types::I128, val)
                        } else {
                            val
                        });
                    },
                    LirOp::LoadPtrImm {
                        ptr,
                        size,
                        offset,
                    } => {
                        let ptr = match ptr {
                            Ptr::CpuState => param_cpu_state,
                            Ptr::K => builder
                                .ins()
                                .iadd_imm(param_ctx, offset_of!(ExecutionContext<'_, '_, Intel386>, k) as i64),
                        };

                        let ty = match size {
                            DataSize::Byte => types::I8,
                            DataSize::Word => types::I16,
                            DataSize::Dword => types::I32,
                            DataSize::Qword => types::I64,
                            DataSize::F80 => todo!(),
                            DataSize::Oword => types::I128,
                        };

                        let val = builder.ins().load(ty, MemFlags::new(), ptr, offset as i32);
                        stack.push(if ty != types::I128 {
                            builder.ins().uextend(types::I128, val)
                        } else {
                            val
                        });
                    },
                    LirOp::StorePtrWithOffset {
                        ptr,
                        size,
                    } => {
                        let val = stack.pop().unwrap();
                        let offset = builder.ins().ireduce(self.pointer_ty, stack.pop().unwrap());
                        let ptr = match ptr {
                            Ptr::CpuState => param_cpu_state,
                            Ptr::K => builder
                                .ins()
                                .iadd_imm(param_ctx, offset_of!(ExecutionContext<'_, '_, Intel386>, k) as i64),
                        };

                        let ptr = builder.ins().iadd(ptr, offset);
                        let ty = match size {
                            DataSize::Byte => types::I8,
                            DataSize::Word => types::I16,
                            DataSize::Dword => types::I32,
                            DataSize::Qword => types::I64,
                            DataSize::F80 => todo!(),
                            DataSize::Oword => types::I128,
                        };

                        let val = if ty != types::I128 {
                            builder.ins().ireduce(ty, val)
                        } else {
                            val
                        };

                        builder.ins().store(MemFlags::new(), val, ptr, 0);
                    },
                    LirOp::StorePtrImm {
                        ptr,
                        size,
                        offset,
                    } => {
                        let val = stack.pop().unwrap();
                        let ptr = match ptr {
                            Ptr::CpuState => param_cpu_state,
                            Ptr::K => builder
                                .ins()
                                .iadd_imm(param_ctx, offset_of!(ExecutionContext<'_, '_, Intel386>, k) as i64),
                        };

                        let ty = match size {
                            DataSize::Byte => types::I8,
                            DataSize::Word => types::I16,
                            DataSize::Dword => types::I32,
                            DataSize::Qword => types::I64,
                            DataSize::F80 => todo!(),
                            DataSize::Oword => types::I128,
                        };

                        let val = if ty != types::I128 {
                            builder.ins().ireduce(ty, val)
                        } else {
                            val
                        };

                        builder.ins().store(MemFlags::new(), val, ptr, offset as i32);
                    },
                    LirOp::SetExceptionWithCode {
                        exception,
                    } => {
                        let code = stack.pop().unwrap();
                        let code = builder.ins().ireduce(types::I32, code);
                        let vector = builder.ins().iconst(types::I8, exception.as_u8() as i64);
                        let f = *set_exception_fn.get_or_insert_with(|| {
                            self.ftable.get(
                                &mut (&mut self.module, &mut builder, self.pointer_ty),
                                FunctionName::CreateExceptionResult,
                            )
                        });
                        builder.ins().call(f, &[param_ctx, vector, code]);
                    },
                    LirOp::SetHandler {
                        id,
                    } => {
                        let arg2 = builder.ins().iconst(types::I64, 0);
                        let arg1 = builder.ins().ireduce(types::I64, stack.pop().unwrap());
                        let arg0 = builder.ins().ireduce(types::I64, stack.pop().unwrap());

                        let num_args = builder.ins().iconst(types::I32, 2);
                        let id = builder.ins().iconst(types::I32, id as i64);

                        let f = *set_handler_fn.get_or_insert_with(|| {
                            self.ftable.get(
                                &mut (&mut self.module, &mut builder, self.pointer_ty),
                                FunctionName::CreateHandlerResult,
                            )
                        });
                        builder.ins().call(f, &[param_ctx, id, arg0, arg1, arg2, num_args]);
                    },
                    LirOp::ReadMemory {
                        num_bytes,
                    } => {
                        let addr = stack.pop().unwrap();
                        let addr = builder.ins().ireduce(types::I32, addr);
                        let len = builder.ins().iconst(types::I8, num_bytes as i64);

                        let f = *mem_read_fn.get_or_insert_with(|| {
                            self.ftable.get(
                                &mut (&mut self.module, &mut builder, self.pointer_ty),
                                FunctionName::MemReadSimple,
                            )
                        });
                        let result = builder
                            .ins()
                            .call(f, &[param_ctx, addr, len, is_userspace, u128_return_val_slot_addr]);
                        let [ok] = *builder.inst_results(result) else {
                            panic!("incorrect number of return values for mem_read")
                        };

                        stack.push(builder.ins().uextend(types::I128, ok));
                        stack.push(builder.ins().stack_load(types::I128, u128_return_val_slot, 0));
                    },
                    LirOp::WriteMemory {
                        num_bytes,
                    } => {
                        let value = stack.pop().unwrap();
                        let addr = stack.pop().unwrap();
                        let addr = builder.ins().ireduce(types::I32, addr);
                        let len = builder.ins().iconst(types::I8, num_bytes as i64);

                        let f = *mem_write_fn.get_or_insert_with(|| {
                            self.ftable.get(
                                &mut (&mut self.module, &mut builder, self.pointer_ty),
                                FunctionName::MemWriteSimple,
                            )
                        });
                        let result = builder.ins().call(f, &[param_ctx, addr, len, is_userspace, value]);
                        let [ok] = *builder.inst_results(result) else {
                            panic!("incorrect number of return values for mem_read")
                        };

                        stack.push(builder.ins().uextend(types::I128, ok));
                    },
                    LirOp::PortOut {
                        len,
                    } => {
                        let data = stack.pop().unwrap();
                        let port = stack.pop().unwrap();
                        let port = builder.ins().ireduce(types::I16, port);
                        let data = builder.ins().ireduce(types::I32, data);
                        let len = builder.ins().iconst(types::I8, len as i64);

                        let f = *port_out_fn.get_or_insert_with(|| {
                            self.ftable
                                .get(&mut (&mut self.module, &mut builder, self.pointer_ty), FunctionName::PortOut)
                        });
                        let result = builder.ins().call(f, &[param_ctx, param_cpu_state, port, len, data]);
                        let [ok] = *builder.inst_results(result) else {
                            panic!("invalid number of return values for port_out")
                        };

                        stack.push(builder.ins().uextend(types::I128, ok));
                    },
                    LirOp::PortIn {
                        len,
                    } => {
                        let port = stack.pop().unwrap();
                        let port = builder.ins().ireduce(types::I16, port);
                        let len = builder.ins().iconst(types::I8, len as i64);

                        let f = *port_in_fn.get_or_insert_with(|| {
                            self.ftable
                                .get(&mut (&mut self.module, &mut builder, self.pointer_ty), FunctionName::PortIn)
                        });
                        let result = builder.ins().call(f, &[param_ctx, param_cpu_state, port, len]);
                        let [ok, result] = *builder.inst_results(result) else {
                            panic!("incorrect number of return values for port_in")
                        };

                        stack.push(builder.ins().uextend(types::I128, ok));
                        stack.push(builder.ins().uextend(types::I128, result));
                    },
                    LirOp::ReadDescriptor {
                        force,
                        mark_accessed,
                    } => {
                        let selector = stack.pop().unwrap();
                        let force = builder.ins().iconst(types::I8, force as i64);
                        let mark_accessed = builder.ins().iconst(types::I8, mark_accessed as i64);
                        let selector = builder.ins().ireduce(types::I16, selector);

                        let zero = builder.ins().iconst(types::I64, 0);
                        builder.ins().stack_store(zero, descriptor_ok_slot, 0);
                        builder.ins().stack_store(zero, base_slot, 0);
                        builder.ins().stack_store(zero, limit_slot, 0);
                        builder.ins().stack_store(zero, access_rights_slot, 0);

                        let args = [
                            param_ctx,
                            param_cpu_state,
                            selector,
                            force,
                            mark_accessed,
                            builder.ins().stack_addr(self.pointer_ty, descriptor_ok_slot, 0),
                            builder.ins().stack_addr(self.pointer_ty, base_slot, 0),
                            builder.ins().stack_addr(self.pointer_ty, limit_slot, 0),
                            builder.ins().stack_addr(self.pointer_ty, access_rights_slot, 0),
                        ];
                        let f = *read_descriptor_fn.get_or_insert_with(|| {
                            self.ftable.get(
                                &mut (&mut self.module, &mut builder, self.pointer_ty),
                                FunctionName::ReadDescriptor,
                            )
                        });
                        let result = builder.ins().call(f, &args);

                        let [execution_ok] = *builder.inst_results(result) else {
                            panic!("incorrect number of return values for read_descriptor")
                        };

                        let descriptor_ok_val = builder.ins().stack_load(types::I64, descriptor_ok_slot, 0);
                        let descriptor_ok_val = builder.ins().uextend(types::I128, descriptor_ok_val);
                        let base_val = builder.ins().stack_load(types::I64, base_slot, 0);
                        let base_val = builder.ins().uextend(types::I128, base_val);
                        let limit_val = builder.ins().stack_load(types::I64, limit_slot, 0);
                        let limit_val = builder.ins().uextend(types::I128, limit_val);
                        let access_rights_val = builder.ins().stack_load(types::I64, access_rights_slot, 0);
                        let access_rights_val = builder.ins().uextend(types::I128, access_rights_val);

                        stack.push(builder.ins().uextend(types::I128, execution_ok));
                        stack.push(descriptor_ok_val);
                        stack.push(base_val);
                        stack.push(limit_val);
                        stack.push(access_rights_val);
                    },
                    LirOp::InstrLen => stack.push(instr_len),
                    LirOp::PartValues => stack.push(param_part_values),
                }
            }

            match block.next() {
                Jump::Next(next) => {
                    builder.ins().jump(block_ids[next.index()], &[]);
                },
                Jump::Cond {
                    if_zero,
                    if_nonzero,
                } => {
                    let val = stack.pop().unwrap();
                    builder
                        .ins()
                        .brif(val, block_ids[if_nonzero.index()], &[], block_ids[if_zero.index()], &[]);
                },
                Jump::Exit {
                    success,
                    metadata,
                    with_last_jump_condition,
                } => {
                    // TODO
                    if *with_last_jump_condition {
                        stack.pop().unwrap();
                    }

                    let val = builder
                        .ins()
                        .iconst(types::I16, (*success as i64) | ((metadata.unwrap_or(0) as i64) << 8));
                    builder.ins().return_(&[val]);
                },
                Jump::Unreachable => panic!("unreachable blocks should have been removed"),
            }
        }

        builder.seal_all_blocks();
        builder.finalize();
        func
    }
}
