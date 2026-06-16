use std::collections::{HashMap, HashSet};

use itertools::Itertools;
use liblisa::arch::Register;
use liblisa::encoding::bitpattern::PartMapping;
use liblisa::encoding::{EncodingRef, IgnoredMetadata};
use serde::{Deserialize, Serialize};

use super::val::VarId;
use crate::arch::intel386::{Intel386, Reg, State};
use crate::codegen::mir::val::{ValBuilder, ValId};
use crate::codegen::{DataSize, Ptr};
use crate::il::{BinOp, MiniSemRef};

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum UncommittedLoc {
    Reg(Reg),
    Dynamic {
        offset: ValId,
        size: usize,
        lowest_index: usize,
    },
}

impl UncommittedLoc {
    fn sort_key(&self) -> usize {
        match *self {
            UncommittedLoc::Reg(reg) => State::byte_offset_of(reg),
            UncommittedLoc::Dynamic {
                lowest_index, ..
            } => 100_000 + lowest_index,
        }
    }
}

impl PartialOrd for UncommittedLoc {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for UncommittedLoc {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.sort_key().cmp(&other.sort_key())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct DynamicRegKey {
    choices: Vec<Option<Reg>>,
    index: ValId,
}

impl DynamicRegKey {
    pub fn new(choices: &[Option<Reg>], index: ValId) -> Self {
        let all_sizes_equal = choices
            .iter()
            .flatten()
            .map(|c| c.byte_size())
            .tuple_windows()
            .all(|(lhs, rhs)| lhs == rhs);
        assert!(
            all_sizes_equal,
            "all register sizes in {choices:?} must be equal, found multiple different sizes"
        );

        DynamicRegKey {
            choices: choices.to_vec(),
            index,
        }
    }
}

#[derive(Clone, Default)]
pub struct UncommittedState {
    fixed: HashMap<Reg, ValId>,
    dynamic: HashMap<DynamicRegKey, ValId>,
    k: Option<ValId>,
}

impl UncommittedState {
    pub fn get_part(
        &self, part_index: usize, value_tree_builder: &mut ValBuilder,
        encoding: &EncodingRef<'_, Intel386, MiniSemRef<'_, Intel386>, IgnoredMetadata>,
    ) -> ValId {
        let (mapping, index) = part_index_and_mapping(part_index, value_tree_builder, encoding);
        self.get_dynamic(mapping, index)
    }

    pub fn get_or_compute_part(
        &self, part_index: usize, value_tree_builder: &mut ValBuilder,
        encoding: &EncodingRef<'_, Intel386, MiniSemRef<'_, Intel386>, IgnoredMetadata>,
    ) -> ValId {
        let (mapping, index) = part_index_and_mapping(part_index, value_tree_builder, encoding);
        self.get_or_compute_dynamic(mapping, index, value_tree_builder)
    }

    pub fn store_part(
        &mut self, part_index: usize, value_tree_builder: &mut ValBuilder,
        encoding: &EncodingRef<'_, Intel386, MiniSemRef<'_, Intel386>, IgnoredMetadata>, val: ValId,
    ) {
        let (mapping, index) = part_index_and_mapping(part_index, value_tree_builder, encoding);
        self.store_dynamic(mapping, index, value_tree_builder, val)
    }

    pub fn iter(&self, value_tree_builder: &mut ValBuilder) -> impl Iterator<Item = (UncommittedLoc, ValId)> {
        self.fixed
            .iter()
            .map(|(k, v)| (UncommittedLoc::Reg(*k), *v))
            .chain(self.dynamic.iter().map(|(k, v)| {
                let offset = Self::compute_dynamic_reg_offset(k, value_tree_builder);

                (
                    UncommittedLoc::Dynamic {
                        offset,
                        size: k.choices.iter().flatten().next().unwrap().byte_size(),
                        lowest_index: k
                            .choices
                            .iter()
                            .flat_map(|choice| choice.map(State::byte_offset_of))
                            .min()
                            .unwrap(),
                    },
                    *v,
                )
            }))
    }

    fn compute_dynamic_reg_offset(key: &DynamicRegKey, value_tree_builder: &mut ValBuilder) -> ValId {
        compute_const_scale_offset(&key.choices)
            .map(|(const_scale, const_offset)| {
                let const_offset = value_tree_builder.imm(const_offset as u64 as u128);
                let const_scale = value_tree_builder.imm(const_scale as u128);
                let offset = value_tree_builder.binop(BinOp::Mul, [const_scale, key.index]);
                value_tree_builder.binop(BinOp::Add, [const_offset, offset])
            })
            .unwrap_or_else(|| {
                let mut offset = value_tree_builder.imm(0);
                for (index, mapping) in key.choices.iter().enumerate() {
                    let n = value_tree_builder.imm(index as u128);
                    let eq = value_tree_builder.binop(BinOp::CmpEq, [key.index, n]);

                    if let Some(reg) = mapping {
                        let reg_offset = State::byte_offset_of(*reg);
                        let reg_offset = value_tree_builder.imm(reg_offset as u128);
                        offset = value_tree_builder.ite(eq, offset, reg_offset);
                    }
                }

                offset
            })
    }

    pub fn k(&self) -> Option<ValId> {
        self.k
    }

    pub fn get_or_compute_k(&self, value_tree_builder: &mut ValBuilder) -> ValId {
        self.k.unwrap_or_else(|| value_tree_builder.load_ptr(Ptr::K, DataSize::Qword))
    }

    pub fn increment_k(&mut self, value_tree_builder: &mut ValBuilder) {
        let k = self.get_or_compute_k(value_tree_builder);
        let one = value_tree_builder.imm(1);
        let k = value_tree_builder.binop(BinOp::Add, [k, one]);
        self.k = Some(k);
    }

    /// Returns the value of the provided register.
    ///
    /// Panics if the location is not present.
    pub fn get(&self, reg: Reg) -> ValId {
        self.fixed[&reg]
    }

    /// Returns the value if it has been modified, or builds the value using the value_tree_builder.
    ///
    /// If the location does not exist in the state, it will not be added.
    pub fn get_or_compute(&self, reg: Reg, value_tree_builder: &mut ValBuilder) -> ValId {
        if reg.is_zero() {
            value_tree_builder.imm(0)
        } else if let Some(val) = self.fixed.get(&reg) {
            *val
        } else {
            let mut val = value_tree_builder.load_ptr_imm(
                Ptr::CpuState,
                reg.byte_size().try_into().unwrap(),
                State::byte_offset_of(reg).try_into().unwrap(),
            );

            // Check if any dynamic value should override the value of this register.
            for (key, &dynamic_val) in self.dynamic.iter() {
                for (index, &choice) in key.choices.iter().enumerate() {
                    if choice == Some(reg) {
                        let n = value_tree_builder.imm(index as u128);
                        let eq = value_tree_builder.binop(BinOp::CmpEq, [key.index, n]);

                        val = value_tree_builder.ite(eq, val, dynamic_val);
                    }
                }
            }

            val
        }
    }

    pub fn store(&mut self, reg: Reg, value_tree_builder: &mut ValBuilder, new_val: ValId) {
        self.fixed.insert(reg, new_val);

        // Check if we should also modify any dynamic registers
        for (key, dynamic_val) in self.dynamic.iter_mut() {
            for (index, &choice) in key.choices.iter().enumerate() {
                if choice == Some(reg) {
                    let n = value_tree_builder.imm(index as u128);
                    let eq = value_tree_builder.binop(BinOp::CmpEq, [key.index, n]);

                    *dynamic_val = value_tree_builder.ite(eq, *dynamic_val, new_val);
                }
            }
        }
    }

    pub fn get_dynamic(&self, choices: &[Option<Reg>], choice_index: ValId) -> ValId {
        let key = DynamicRegKey::new(choices, choice_index);
        self.dynamic[&key]
    }

    pub fn get_or_compute_dynamic(
        &self, choices: &[Option<Reg>], choice_index: ValId, value_tree_builder: &mut ValBuilder,
    ) -> ValId {
        let key = DynamicRegKey::new(choices, choice_index);

        if let Some(&val) = self.dynamic.get(&key) {
            val
        } else {
            let offset = Self::compute_dynamic_reg_offset(&key, value_tree_builder);
            let size = DataSize::try_from_bytes(choices.iter().flatten().next().unwrap().byte_size()).unwrap();
            let mut val = value_tree_builder.load_ptr_offset(Ptr::CpuState, size, offset);

            // Check if any modifications to fixed registers should override this dynamic value
            for (&reg, &fixed_val) in self.fixed.iter() {
                for (index, &choice) in choices.iter().enumerate() {
                    if choice == Some(reg) {
                        let n = value_tree_builder.imm(index as u128);
                        let eq = value_tree_builder.binop(BinOp::CmpEq, [key.index, n]);

                        val = value_tree_builder.ite(eq, val, fixed_val);
                    }
                }
            }

            // Check if any other dynamic values should override this dynamic value
            for (key, &dynamic_val) in self.dynamic.iter() {
                Self::propagate_dynamic_val(
                    choices,
                    choice_index,
                    &key.choices,
                    key.index,
                    value_tree_builder,
                    |value_tree_builder, eq| {
                        val = value_tree_builder.ite(eq, val, dynamic_val);
                    },
                );
            }

            val
        }
    }

    pub fn store_dynamic(
        &mut self, choices: &[Option<Reg>], choice_index: ValId, value_tree_builder: &mut ValBuilder, new_val: ValId,
    ) {
        let key = DynamicRegKey::new(choices, choice_index);
        self.dynamic.insert(key.clone(), new_val);

        // Check if we should also modify any fixed registers
        for (&reg, fixed_val) in self.fixed.iter_mut() {
            for (index, &choice) in choices.iter().enumerate() {
                if choice == Some(reg) {
                    let n = value_tree_builder.imm(index as u128);
                    let eq = value_tree_builder.binop(BinOp::CmpEq, [key.index, n]);

                    *fixed_val = value_tree_builder.ite(eq, *fixed_val, new_val);
                }
            }
        }

        // Check if we should also modify any dynamic registers
        for (key, dynamic_val) in self.dynamic.iter_mut() {
            Self::propagate_dynamic_val(
                choices,
                choice_index,
                &key.choices,
                key.index,
                value_tree_builder,
                |value_tree_builder, eq| {
                    *dynamic_val = value_tree_builder.ite(eq, *dynamic_val, new_val);
                },
            );
        }
    }

    fn propagate_dynamic_val<'r>(
        lhs: &[Option<Reg>], lhs_index_val: ValId, rhs: &[Option<Reg>], rhs_index_val: ValId,
        value_tree_builder: &mut ValBuilder, mut propagate: impl FnMut(&mut ValBuilder, ValId),
    ) {
        if lhs == rhs {
            let eq = value_tree_builder.binop(BinOp::CmpEq, [lhs_index_val, rhs_index_val]);
            propagate(value_tree_builder, eq)
        } else {
            for (lhs_index, &lhs_choice) in lhs.iter().enumerate() {
                for (rhs_index, &rhs_choice) in rhs.iter().enumerate() {
                    if lhs_choice.is_some() && lhs_choice == rhs_choice {
                        let n = value_tree_builder.imm(lhs_index as u128);
                        let this_eq = value_tree_builder.binop(BinOp::CmpEq, [lhs_index_val, n]);

                        let n = value_tree_builder.imm(rhs_index as u128);
                        let other_eq = value_tree_builder.binop(BinOp::CmpEq, [rhs_index_val, n]);

                        let both_eq = value_tree_builder.binop(BinOp::And, [this_eq, other_eq]);

                        propagate(value_tree_builder, both_eq);
                    }
                }
            }
        }
    }

    pub fn merge(
        lhs: UncommittedState, rhs: UncommittedState, value_tree_builder: &mut ValBuilder, alloc_var: impl FnMut() -> VarId,
    ) -> (UncommittedState, Vec<(VarId, ValId)>, Vec<(VarId, ValId)>) {
        let (state, stores) = Self::merge_n(&[&lhs, &rhs], value_tree_builder, alloc_var);
        let Ok([lhs_stores, rhs_stores]) = <[_; 2]>::try_from(stores) else {
            unreachable!();
        };

        (state, lhs_stores, rhs_stores)
    }

    pub fn merge_n(
        states: &[&UncommittedState], value_tree_builder: &mut ValBuilder, mut alloc_var: impl FnMut() -> VarId,
    ) -> (UncommittedState, Vec<Vec<(VarId, ValId)>>) {
        let all_regs = states
            .iter()
            .flat_map(|state| state.fixed.keys())
            .copied()
            .collect::<HashSet<_>>();
        let all_dyn = states
            .iter()
            .flat_map(|state| state.dynamic.keys())
            .cloned()
            .collect::<HashSet<_>>();

        let mut stores = vec![Vec::new(); states.len()];
        let mut state = Self::default();

        // We insert all values into the merged state one by one.
        // For values that do not exist on both sides, we compute an unchanged value.
        // This is typically just a memory load.
        // If the values are equal on both sides, we just store the value.
        // If there is a difference between the values, we introduce a variable.
        // The variable is assigned different values based on which branch was taken.
        // This is stored in the `lhs/rhs_stores` variable.
        // The caller is responsible for placing these stores at the right position.

        for reg in all_regs {
            let vals = states
                .iter()
                .map(|state| state.get_or_compute(reg, value_tree_builder))
                .collect::<Vec<_>>();

            state.fixed.insert(
                reg,
                if vals.iter().all_equal() {
                    vals[0]
                } else {
                    let var = alloc_var();
                    for (stores, val) in stores.iter_mut().zip(vals.iter()) {
                        stores.push((var, *val));
                    }
                    value_tree_builder.use_var(var)
                },
            );
        }

        for key in all_dyn {
            let vals = states
                .iter()
                .map(|state| state.get_or_compute_dynamic(&key.choices, key.index, value_tree_builder))
                .collect::<Vec<_>>();

            state.dynamic.insert(
                key,
                if vals.iter().all_equal() {
                    vals[0]
                } else {
                    let var = alloc_var();
                    for (stores, val) in stores.iter_mut().zip(vals.iter()) {
                        stores.push((var, *val));
                    }
                    value_tree_builder.use_var(var)
                },
            );
        }

        state.k = if states.is_empty() {
            None
        } else if states.iter().map(|state| state.k).all_equal() {
            states[0].k
        } else {
            let var = alloc_var();
            for (stores, val) in stores.iter_mut().zip(states.iter()) {
                stores.push((var, val.get_or_compute_k(value_tree_builder)));
            }

            Some(value_tree_builder.use_var(var))
        };

        (state, stores)
    }
}

fn part_index_and_mapping<'r>(
    part_index: usize, value_tree_builder: &mut ValBuilder,
    encoding: &EncodingRef<'r, Intel386, MiniSemRef<'_, Intel386>, IgnoredMetadata>,
) -> (&'r [Option<Reg>], ValId) {
    let part = &encoding.parts[part_index];
    let PartMapping::Register {
        mapping,
    } = &part.mapping
    else {
        unimplemented!("only register parts are supported")
    };

    let part_values = value_tree_builder.part_values();
    let packing = &encoding.semantics.part_packing[part_index];
    let index = value_tree_builder.extract(part_values, packing.offset(), packing.len());
    (mapping, index)
}

fn compute_const_scale_offset(mapping: &[Option<Reg>]) -> Option<(usize, i64)> {
    (1..64).find_map(|scale| {
        mapping
            .iter()
            .enumerate()
            .flat_map(|(index, reg)| reg.map(|reg| (index * scale, State::byte_offset_of(reg))))
            .map(|(a, b)| Some(b as i64 - a as i64))
            .reduce(|a, b| if a == b { a } else { None })
            .unwrap()
            .map(|offset| (scale, offset))
    })
}
