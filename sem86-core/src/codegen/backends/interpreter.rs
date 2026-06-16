use std::cell::RefCell;

use arrayvec::ArrayVec;
use liblisa::utils::bitmask_u128;
use softfloat::Float80;

use crate::arch::intel386::{Intel386, State};
use crate::codegen::backends::TracedAccess;
use crate::codegen::lir::{BlockId, Jump, Lir, LirOp};
use crate::codegen::{DataSize, Ptr};
use crate::emulator::Emulator;
use crate::emulator::exec::ExecutionContext;
use crate::il::{ExecResult, MemArea};

const MAX_STACK_DEPTH: usize = 128;
const MAX_NUM_DEFS: usize = 32768;

#[derive(Clone, Debug)]
pub struct InterpretedFunction {
    lir: Lir,
}

impl InterpretedFunction {
    pub fn new(lir: Lir) -> Self {
        Self {
            lir,
        }
    }

    pub fn execute(&self, emulator: &mut Emulator<'_, '_>, mut trace_memory: impl FnMut(TracedAccess)) -> (bool, u64) {
        assert!(
            self.lir.num_defs() < MAX_NUM_DEFS,
            "increase MAX_NUM_DEFS: {} needed",
            self.lir.num_defs()
        );

        thread_local! {
            static VARIABLES: RefCell<[u128; MAX_NUM_DEFS]> = const { RefCell::new([0; MAX_NUM_DEFS]) };
            static STACK: RefCell<Stack<u128>> = RefCell::new(Stack::new());
        }

        VARIABLES.with_borrow_mut(|variables| {
            STACK.with_borrow_mut(|stack| {
                let mut state = InterpreterState {
                    variables: &mut *variables,
                    cpu: &mut emulator.cpu,
                    ctx: &mut emulator.ctx,
                };

                let mut current_block = BlockId::ROOT;
                loop {
                    let Some(block) = &self.lir.get(current_block) else {
                        panic!("jumped to non-existant block {current_block:?} in {:#?}", self.lir);
                    };
                    match state.execute_block(&self.lir, block, stack, &mut trace_memory) {
                        Ok(next) => current_block = next,
                        Err((success, metadata)) => return (success, metadata),
                    }
                }
            })
        })
    }

    pub fn num_ops(&self) -> usize {
        self.lir.num_ops()
    }

    pub fn lir(&self) -> &Lir {
        &self.lir
    }
}

#[derive(Clone, Debug)]
struct Stack<T> {
    data: ArrayVec<T, MAX_STACK_DEPTH>,
}

impl<T> Stack<T> {
    pub fn new() -> Self {
        Self {
            data: Default::default(),
        }
    }

    pub fn push(&mut self, val: T) {
        match self.data.try_push(val) {
            Ok(_) => (),
            Err(e) => panic!("unable to push onto stack (current len={}): {e}", self.data.len()),
        }
    }

    pub fn pop<P: Poppable<T>>(&mut self) -> P {
        P::pop(self)
    }

    pub fn op<P: Poppable<T>>(&mut self, f: impl FnOnce(P) -> T) {
        let p = self.pop();
        self.push(f(p));
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

trait Poppable<T> {
    fn pop(stack: &mut Stack<T>) -> Self;
}

impl<T> Poppable<T> for T {
    fn pop(stack: &mut Stack<T>) -> Self {
        stack.data.pop().unwrap()
    }
}

impl<const N: usize, T> Poppable<T> for [T; N] {
    fn pop(stack: &mut Stack<T>) -> Self {
        assert!(
            stack.data.len() >= N,
            "not enough elements on stack: tried to pop {N}, but only {} elements are on the stack",
            stack.data.len()
        );
        let mut d = stack.data.drain(stack.data.len() - N..);
        std::array::from_fn(|_| d.next().unwrap())
    }
}

struct InterpreterState<'a, 'mem, 'tag> {
    variables: &'a mut [u128],
    cpu: &'a mut State,
    ctx: &'a mut ExecutionContext<'mem, 'tag, Intel386>,
}

impl InterpreterState<'_, '_, '_> {
    fn get_ptr(&mut self, ptr: Ptr, offset: usize) -> *mut u8 {
        // TODO: Assert offset is safe
        let ptr = match ptr {
            Ptr::CpuState => self.cpu as *mut _ as *mut u8,
            Ptr::K => &mut self.ctx.k as *mut u64 as *mut u8,
        };

        unsafe { ptr.byte_add(offset) }
    }

    #[inline(always)]
    fn execute_block(
        &mut self, lir: &Lir, block: &crate::codegen::lir::LirBlock, stack: &mut Stack<u128>,
        mut trace_memory: impl FnMut(TracedAccess),
    ) -> Result<BlockId, (bool, u64)> {
        for op in block.operations() {
            // trace!("Executing {op:?} on stack {stack:X?}");
            match op {
                LirOp::Const(n) => stack.push(lir[*n]),
                LirOp::Load(def_id) => stack.push(self.variables[def_id.index()]),
                LirOp::Store(def_id) => {
                    let val = stack.pop();
                    self.variables[def_id.index()] = val;
                    // debug!("Store: {def_id:?} = 0x{val:X}")
                },
                LirOp::BinOp(bin_op) => stack.op(|[lhs, rhs]: [u128; 2]| bin_op.execute(lhs, rhs)),
                LirOp::FpBinOp(bin_op) => stack.op(|[lhs, rhs, rc]: [u128; 3]| bin_op.execute(lhs, rhs, rc as u8)),
                LirOp::FpUnOp(un_op) => stack.op(|[val, rc]: [u128; 2]| un_op.execute(val, rc as u8)),
                LirOp::Blend(mask) => stack.op(|[lhs, rhs]: [u128; 2]| {
                    let mask = lir[*mask];
                    (lhs & !mask) | (rhs & mask)
                }),
                LirOp::Extract {
                    skip,
                    take,
                } => stack.op(|x: u128| (x >> skip) & bitmask_u128(*take as u32)),
                LirOp::UnOp(un_op) => stack.op(|x| un_op.execute(x)),
                LirOp::Ite => stack.op(|[if_nonzero, if_zero, cond]: [u128; 3]| {
                    // debug!("ITE: 0x{cond:X} if_zero={if_zero:X}, if_nonzero={if_nonzero:X}");
                    if cond == 0 { if_zero } else { if_nonzero }
                }),
                LirOp::LoadPtrImm {
                    ptr,
                    size,
                    offset,
                } => {
                    let ptr = self.get_ptr(*ptr, *offset as usize);
                    stack.push(unsafe {
                        match size {
                            DataSize::Byte => ptr.read() as u128,
                            DataSize::Word => (ptr as *mut u16).read_unaligned() as u128,
                            DataSize::Dword => (ptr as *mut u32).read_unaligned() as u128,
                            DataSize::Qword => (ptr as *mut u64).read_unaligned() as u128,
                            DataSize::F80 => (ptr as *mut Float80).read_unaligned().to_bits(),
                            DataSize::Oword => (ptr as *mut u128).read_unaligned(),
                        }
                    });
                },
                LirOp::LoadPtrWithOffset {
                    ptr,
                    size,
                } => {
                    let offset = stack.pop::<u128>();
                    let ptr = self.get_ptr(*ptr, offset as usize);
                    stack.push(unsafe {
                        match size {
                            DataSize::Byte => ptr.read() as u128,
                            DataSize::Word => (ptr as *mut u16).read_unaligned() as u128,
                            DataSize::Dword => (ptr as *mut u32).read_unaligned() as u128,
                            DataSize::Qword => (ptr as *mut u64).read_unaligned() as u128,
                            DataSize::F80 => (ptr as *mut Float80).read_unaligned().to_bits(),
                            DataSize::Oword => (ptr as *mut u128).read_unaligned(),
                        }
                    });
                },
                LirOp::StorePtrImm {
                    ptr,
                    size,
                    offset,
                } => {
                    let value = stack.pop();
                    let ptr = self.get_ptr(*ptr, *offset as usize);
                    unsafe {
                        match size {
                            DataSize::Byte => ptr.write(value as u8),
                            DataSize::Word => (ptr as *mut u16).write_unaligned(value as u16),
                            DataSize::Dword => (ptr as *mut u32).write_unaligned(value as u32),
                            DataSize::Qword => (ptr as *mut u64).write_unaligned(value as u64),
                            DataSize::F80 => (ptr as *mut Float80).write_unaligned(Float80::from_bits(value)),
                            DataSize::Oword => (ptr as *mut u128).write_unaligned(value),
                        }
                    }
                },
                LirOp::StorePtrWithOffset {
                    ptr,
                    size,
                } => {
                    let [offset, value]: [u128; 2] = stack.pop();
                    let ptr = self.get_ptr(*ptr, offset as usize);
                    unsafe {
                        match size {
                            DataSize::Byte => ptr.write(value as u8),
                            DataSize::Word => (ptr as *mut u16).write_unaligned(value as u16),
                            DataSize::Dword => (ptr as *mut u32).write_unaligned(value as u32),
                            DataSize::Qword => (ptr as *mut u64).write_unaligned(value as u64),
                            DataSize::F80 => (ptr as *mut Float80).write_unaligned(Float80::from_bits(value)),
                            DataSize::Oword => (ptr as *mut u128).write_unaligned(value),
                        }
                    }
                },
                LirOp::SetExceptionWithCode {
                    exception,
                } => {
                    let code = stack.pop::<u128>();
                    self.ctx.result = Err(exception.with_code_from_u32(code as u32)).into()
                },
                LirOp::SetHandler {
                    id,
                } => {
                    let args: [u128; 2] = stack.pop();
                    self.ctx.result = Ok(ExecResult::InvokeHandler {
                        id: *id,
                        args: [args[0] as u32, args[1] as u32],
                    })
                    .into()
                },
                LirOp::ReadMemory {
                    num_bytes,
                } => {
                    let addr = stack.pop::<u128>();
                    let area = MemArea::Protected {
                        addr: addr as u32,
                        len: *num_bytes,
                    };
                    match area.read_from_mem_as_u128(
                        self.ctx.memory,
                        self.cpu.is_userspace(),
                        &mut self.ctx.mmio_ctx.hw.mmio(&mut self.ctx.mmio_ctx.icache),
                    ) {
                        Ok(val) => {
                            trace_memory(TracedAccess {
                                addr: area.start_addr().as_u64(),
                                len: *num_bytes,
                                is_write: false,
                                data: val,
                            });
                            stack.push(1);
                            stack.push(val);
                        },
                        Err(err) => {
                            self.ctx.result = Err(err).into();
                            stack.push(0);
                            stack.push(0);
                        },
                    }
                },
                LirOp::WriteMemory {
                    num_bytes,
                } => {
                    let [addr, value] = stack.pop();
                    let area = MemArea::Protected {
                        addr: addr as u32,
                        len: *num_bytes,
                    };
                    match area.write_u128_to_mem(
                        self.ctx.memory,
                        self.cpu.is_userspace(),
                        &mut self.ctx.mmio_ctx.hw.mmio(&mut self.ctx.mmio_ctx.icache),
                        value,
                    ) {
                        Ok(_) => {
                            trace_memory(TracedAccess {
                                addr: area.start_addr().as_u64(),
                                len: *num_bytes,
                                is_write: true,
                                data: value,
                            });

                            stack.push(1)
                        },
                        Err(err) => {
                            self.ctx.result = Err(err).into();
                            stack.push(0);
                        },
                    }
                },
                LirOp::PortOut {
                    len,
                } => {
                    let [port, data] = stack.pop();
                    match self.ctx.port_out(self.cpu, port as u16, *len, data as u32) {
                        Ok(()) => stack.push(1),
                        Err(err) => {
                            self.ctx.result = Err(err).into();
                            stack.push(0);
                        },
                    }
                },
                LirOp::PortIn {
                    len,
                } => {
                    let [port] = stack.pop();
                    match self.ctx.port_in(self.cpu, port as u16, *len) {
                        Ok(val) => {
                            stack.push(1);
                            stack.push(val as u128);
                        },
                        Err(err) => {
                            self.ctx.result = Err(err).into();
                            stack.push(0);
                            stack.push(0);
                        },
                    }
                },
                LirOp::ReadDescriptor {
                    force,
                    mark_accessed,
                } => {
                    let selector_val = stack.pop::<u128>();
                    match self
                        .ctx
                        .read_descriptor(self.cpu, *force, *mark_accessed, selector_val as u16)
                    {
                        Ok(result) => {
                            stack.push(1);
                            stack.push(result.ok as u128);
                            stack.push(result.base as u128);
                            stack.push(result.limit as u128);
                            stack.push(result.access_rights as u128);
                        },
                        Err(err) => {
                            self.ctx.result = Err(err).into();
                            stack.push(0);
                            stack.push(0);
                            stack.push(0);
                            stack.push(0);
                            stack.push(0);
                        },
                    }
                },
                LirOp::InstrLen => todo!(),
                LirOp::PartValues => todo!(),
            }
        }

        let result = match block.next() {
            Jump::Next(block_id) => Ok(*block_id),
            Jump::Cond {
                if_zero,
                if_nonzero,
            } => Ok(if stack.pop::<u128>() == 0 { *if_zero } else { *if_nonzero }),
            Jump::Exit {
                success,
                metadata,
                ..
            } => Err((*success, metadata.unwrap_or(0))),
            Jump::Unreachable => unreachable!(),
        };
        assert!(stack.is_empty());
        result
    }
}
