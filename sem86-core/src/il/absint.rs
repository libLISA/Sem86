use arrayvec::ArrayVec;
use liblisa::arch::Register;
use liblisa::encoding::bitpattern::PartMapping;
use liblisa::encoding::{EncodingRef, IgnoredMetadata, UnsizedParLoc};
use liblisa::state::Size;
use liblisa::utils::bitmask_u128;
use log::trace;

use crate::arch::intel386::{Intel386, State};
use crate::il::part_values::PartValues;
use crate::il::{BinOp, Cmd, Commands, MAX_TEMP_VARS, MiniSemRef, Op, UnOp, Val};

pub trait EncodingExtensions {
    fn compute_next_pc(&self, part_values: &[u64], instr_len: usize, cpu: &State);
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IpDelta {
    Absolute(u128),
    Offset(i128),
}

#[derive(Clone, Debug, Default)]
enum ValueSet {
    Choices(ArrayVec<Value, 2>),
    #[default]
    Any,
}

impl From<Value> for ValueSet {
    fn from(value: Value) -> Self {
        Self::Choices([value].into_iter().collect())
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Value {
    Num(u128),
    IpRelative(i128),
}

impl Value {
    fn as_relative(&self) -> IpDelta {
        match self {
            Value::Num(n) => IpDelta::Absolute(*n),
            Value::IpRelative(offset) => IpDelta::Offset(*offset),
        }
    }
}

impl ValueSet {
    fn as_relative(&self) -> Option<ArrayVec<IpDelta, 2>> {
        match self {
            ValueSet::Choices(choices) => Some(choices.iter().map(|c| c.as_relative()).collect()),
            ValueSet::Any => None,
        }
    }

    fn as_nums(&self) -> Option<ArrayVec<u128, 2>> {
        match self {
            ValueSet::Choices(choices) => {
                let mut result = ArrayVec::new();
                for value in choices.iter() {
                    match value {
                        Value::Num(n) => result.push(*n),
                        Value::IpRelative(_) => return None,
                    }
                }

                Some(result)
            },
            ValueSet::Any => None,
        }
    }

    fn map_choices<T: Default>(&self, mut map: impl FnMut(&ArrayVec<Value, 2>) -> T) -> T {
        match self {
            ValueSet::Choices(array_vec) => map(array_vec),
            ValueSet::Any => T::default(),
        }
    }

    fn map_nums(&self, mut f: impl FnMut(u128) -> u128) -> ValueSet {
        self.map_choices(|choices| {
            let mut result = choices.clone();
            for choice in result.iter_mut() {
                match choice {
                    Value::Num(n) => *n = f(*n),
                    Value::IpRelative(_) => return ValueSet::Any,
                }
            }

            ValueSet::Choices(result)
        })
    }

    fn powerset(&self, other: &Self, mut f: impl FnMut(Value, Value) -> ValueSet) -> ValueSet {
        match (self, other) {
            (ValueSet::Choices(lhs), ValueSet::Choices(rhs)) => {
                let mut result = ValueSet::Choices(ArrayVec::new());
                for &a in lhs.iter() {
                    for &b in rhs.iter() {
                        result = result.union(&f(a, b));
                        if let ValueSet::Any = result {
                            return result
                        }
                    }
                }

                result
            },
            (_, ValueSet::Any) | (ValueSet::Any, _) => ValueSet::Any,
        }
    }

    fn powerset_nums(&self, other: &Self, mut f: impl FnMut(u128, u128) -> u128) -> ValueSet {
        match (self, other) {
            (ValueSet::Choices(lhs), ValueSet::Choices(rhs)) => {
                let mut result = ArrayVec::new();
                for a in lhs.iter() {
                    for b in rhs.iter() {
                        if let Value::Num(a) = a
                            && let Value::Num(b) = b
                        {
                            let new = Value::Num(f(*a, *b));
                            if !result.contains(&new) {
                                match result.try_push(new) {
                                    Ok(_) => (),
                                    Err(_) => return ValueSet::Any,
                                }
                            }
                        } else {
                            return ValueSet::Any
                        }
                    }
                }

                ValueSet::Choices(result)
            },
            (_, ValueSet::Any) | (ValueSet::Any, _) => ValueSet::Any,
        }
    }

    fn set_sized(&mut self, size: Size, max_bits: usize, value: ValueSet) {
        if size.start_byte() == 0 && (size.end_byte() + 1) * 8 >= max_bits {
            *self = value;
        } else {
            *self = self.powerset_nums(&value, |a, b| {
                let mask = bitmask_u128(size.num_bytes() as u32 * 8) << (size.start_byte() * 8);
                (a & !mask) | (b & mask)
            });
        }
    }

    fn try_from_iterator(items: impl Iterator<Item = Value>) -> Option<Self> {
        let mut values = ArrayVec::new();
        for item in items {
            if !values.contains(&item) {
                values.try_push(item).ok()?;
            }
        }

        Some(Self::Choices(values))
    }

    fn union(&self, other: &ValueSet) -> ValueSet {
        match (self, other) {
            (ValueSet::Any, _) | (_, ValueSet::Any) => ValueSet::Any,
            (ValueSet::Choices(lhs), ValueSet::Choices(rhs)) => {
                if let Some(combined) = Self::try_from_iterator(lhs.iter().chain(rhs.iter()).cloned()) {
                    combined
                } else {
                    ValueSet::Any
                }
            },
        }
    }

    fn add(lhs: ValueSet, rhs: ValueSet) -> ValueSet {
        lhs.powerset(&rhs, |lhs, rhs| match (lhs, rhs) {
            (Value::Num(lhs), Value::Num(rhs)) => Value::Num(lhs.wrapping_add(rhs)).into(),
            (Value::Num(num), Value::IpRelative(offset)) | (Value::IpRelative(offset), Value::Num(num)) => {
                Value::IpRelative(offset.wrapping_add(num as i128)).into()
            },
            (Value::IpRelative(_), Value::IpRelative(_)) => ValueSet::Any,
        })
    }
}

#[derive(Clone, Debug)]
struct AbsIntState {
    ip: ValueSet,
    tmp: [ValueSet; MAX_TEMP_VARS],
}

impl AbsIntState {
    pub fn union(self, other: Self) -> Self {
        Self {
            ip: self.ip.union(&other.ip),
            tmp: std::array::from_fn(|n| self.tmp[n].union(&other.tmp[n])),
        }
    }
}

#[derive(Debug)]
pub struct UnusedAbsInt<'a> {
    part_values: PartValues,
    encoding: EncodingRef<'a, Intel386, MiniSemRef<'a, Intel386>, IgnoredMetadata>,
    instr_len: usize,
}

impl<'a> UnusedAbsInt<'a> {
    pub fn new(
        part_values: PartValues, encoding: EncodingRef<'a, Intel386, MiniSemRef<'a, Intel386>, IgnoredMetadata>, instr_len: usize,
    ) -> Self {
        Self {
            part_values,
            encoding,
            instr_len,
        }
    }

    /// Runs abstract interpretation, considering only paths that execute successfully (i.e., no exceptions and no handlers).
    pub fn run_from_known_ip(&self, ip: u32) -> Option<ArrayVec<u32, 2>> {
        let mut state = AbsIntState {
            ip: Value::Num(ip as u128).into(),
            tmp: std::array::from_fn(|_| ValueSet::Any),
        };

        self.absint_cmds(self.encoding.semantics.commands, &mut state)?;

        state.ip.as_relative().map(|ns| {
            ns.iter()
                .map(|&n| match n {
                    IpDelta::Absolute(ip) => ip as u32,
                    IpDelta::Offset(delta) => ip.wrapping_add(delta as u32),
                })
                .collect()
        })
    }

    /// Runs abstract interpretation, considering only paths that execute successfully (i.e., no exceptions and no handlers).
    pub fn run_relative(&self) -> Option<ArrayVec<IpDelta, 2>> {
        let mut state = AbsIntState {
            ip: Value::IpRelative(0).into(),
            tmp: std::array::from_fn(|_| ValueSet::Any),
        };

        self.absint_cmds(self.encoding.semantics.commands, &mut state)?;
        state.ip.as_relative()
    }

    fn absint_cmds(&self, cmds: &Commands<Intel386>, state: &mut AbsIntState) -> Option<()> {
        match cmds {
            Commands::Ops(ops) => {
                for cmd in ops.iter() {
                    if self.absint_cmd(cmd, state)? {
                        break
                    }
                }
            },
        }

        Some(())
    }

    fn absint_cmd(&self, cmd: &Cmd<Intel386>, state: &mut AbsIntState) -> Option<bool> {
        trace!("Interpreting {cmd:#?} with state {state:?}");
        match cmd {
            Cmd::Store {
                to,
                op,
            } => {
                self.store(state, to, self.eval_op(op, state));
            },
            Cmd::Handler {
                ..
            }
            | Cmd::Exception {
                ..
            } => return None,
            Cmd::If {
                if_zero,
                if_nonzero,
                ..
            } => {
                if if_zero.all_paths_terminate() {
                    self.absint_cmds(if_nonzero, state)?;
                } else if if_nonzero.all_paths_terminate() {
                    self.absint_cmds(if_zero, state)?;
                } else {
                    let mut state_if_zero = state.clone();
                    let mut state_if_nonzero = state.clone();
                    trace!("Evaluating if-zero branch");
                    self.absint_cmds(if_zero, &mut state_if_zero);

                    trace!("Evaluating if-nonzero branch");
                    self.absint_cmds(if_nonzero, &mut state_if_nonzero);

                    trace!("Resolving IF with undetermined condition: {cmd:#?}");
                    trace!("If-zero branch: {state_if_zero:?}");
                    trace!("If-nonzero branch: {state_if_nonzero:?}");
                    *state = state_if_zero.union(state_if_nonzero);

                    trace!("Resulting state: {state:?}");
                }
            },
            Cmd::ReadDescriptor {
                ok,
                base,
                limit,
                access_rights,
                ..
            } => {
                self.store(state, ok, ValueSet::Any);
                self.store(state, base, ValueSet::Any);
                self.store(state, limit, ValueSet::Any);
                self.store(state, access_rights, ValueSet::Any);
            },
            Cmd::Log {
                ..
            }
            | Cmd::Out {
                ..
            } => (),
            Cmd::In {
                data, ..
            } => {
                self.store(state, data, ValueSet::Any);
            },
            Cmd::WriteMemory {
                ..
            }
            | Cmd::ReadMemory {
                ..
            } => (),
            Cmd::StoreDynamicReg {
                ..
            } => todo!(),
            Cmd::LoadDynamicReg {
                ..
            } => todo!(),
        }

        Some(false)
    }

    fn eval_op(&self, op: &Op<Intel386>, state: &AbsIntState) -> ValueSet {
        match op {
            Op::BinOp {
                args,
                op: BinOp::Add,
            } => {
                let [lhs, rhs] = args.map(|val| self.eval_val(&val, state));
                ValueSet::add(lhs, rhs)
            },
            Op::BinOp {
                args,
                op,
            } => {
                let args = args.map(|val| self.eval_val(&val, state));
                args[0].powerset_nums(&args[1], |lhs, rhs| op.execute(lhs, rhs))
            },
            Op::FpBinOp {
                ..
            }
            | Op::FpUnOp {
                ..
            } => ValueSet::Any,
            Op::UnOp {
                arg,
                op: UnOp::Id,
            } => self.eval_val(arg, state),
            Op::UnOp {
                arg,
                op,
            } => self.eval_val(arg, state).map_nums(|arg| op.execute(arg)),
            Op::Ite {
                cond,
                if_nonzero,
                if_zero,
            } => {
                let cond = self.eval_val(cond, state);
                let cond = cond.as_nums().and_then(|ns| {
                    if ns.iter().all(|&n| n != 0) {
                        Some(true)
                    } else if ns.iter().all(|&n| n == 0) {
                        Some(false)
                    } else {
                        None
                    }
                });

                match cond {
                    Some(is_nonzero) => {
                        if is_nonzero {
                            self.eval_val(if_nonzero, state)
                        } else {
                            self.eval_val(if_zero, state)
                        }
                    },
                    None => self.eval_val(if_nonzero, state).union(&self.eval_val(if_zero, state)),
                }
            },
        }
    }

    fn eval_val(&self, val: &Val<Intel386>, state: &AbsIntState) -> ValueSet {
        match *val {
            Val::Temp(n) => state.tmp[n].clone(),
            Val::Loc(par_loc) => match par_loc.loc {
                UnsizedParLoc::Reg(reg) => {
                    if reg.is_pc() {
                        state.ip.clone()
                    } else {
                        ValueSet::Any
                    }
                },
                UnsizedParLoc::Mem(_) => ValueSet::Any,
                UnsizedParLoc::Part(n) => match &self.encoding.parts[n].mapping {
                    PartMapping::Imm {
                        ..
                    } => Value::Num(self.part_values.get(self.encoding.semantics.part_packing, n) as u128).into(),
                    PartMapping::MemoryComputation {
                        ..
                    } => ValueSet::Any,
                    PartMapping::Register {
                        ..
                    } => ValueSet::Any,
                },
                UnsizedParLoc::InstrLen => Value::Num(self.instr_len as u128).into(),
                UnsizedParLoc::Const(val) => Value::Num(val as u128).into(),
            },
            Val::Conv {
                loc,
                source_bits,
                target_bits,
                sign_extend,
                swap_endianness,
            } => {
                let val = self.eval_val(&Val::Loc(loc), state);
                val.map_nums(|n| Val::<Intel386>::apply_conversion(n, source_bits, sign_extend, swap_endianness, target_bits))
            },
        }
    }

    fn store(&self, state: &mut AbsIntState, val: &Val<Intel386>, value: ValueSet) {
        trace!("Store {val:X?} <- {value:X?}");
        match val {
            Val::Temp(n) => state.tmp[*n] = value,
            Val::Loc(par_loc) => match par_loc.loc {
                UnsizedParLoc::Reg(reg) => {
                    if reg.is_pc() {
                        state.ip.set_sized(par_loc.size, reg.byte_size() * 4, value);
                    }
                },
                UnsizedParLoc::Mem(_) => (),
                UnsizedParLoc::Part(part_index) => {
                    if let PartMapping::Register {
                        mapping,
                    } = &self.encoding.parts[part_index].mapping
                        && mapping.iter().flatten().any(|reg| reg.is_pc())
                    {
                        state.ip = ValueSet::Any;
                    }
                },
                UnsizedParLoc::InstrLen | UnsizedParLoc::Const(_) => unreachable!(),
            },
            Val::Conv {
                ..
            } => unreachable!(),
        }
    }
}
