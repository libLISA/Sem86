//! Types representing the dataflows in an [`Encoding`](super::Encoding).

use std::fmt::Debug;
use std::ops::{Index, IndexMut};

use bitcode::{Decode, Encode};
use itertools::Itertools;
use serde::{Deserialize, Serialize};

use crate::arch::{Arch, Register};
use crate::encoding::UnsizedParLoc;
use crate::encoding::bitpattern::{FlowInputLocation, FlowOutputLocation, FlowValueLocation};
use crate::instr::Instruction;
use crate::semantics::Computation;
use crate::utils::bitmap::Bitmap;

mod accesses;
mod address_computation;
mod inputs;

pub use accesses::*;
pub use address_computation::*;
pub use inputs::*;

use super::{ParLoc, Semantics, WriteOrdering};

/// A collection of dataflows and memory accesses.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(
    feature = "schemars",
    schemars(bound = "A: schemars::JsonSchema, A::Reg: schemars::JsonSchema, C: schemars::JsonSchema")
)]
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
#[serde(bound(serialize = "C: Serialize", deserialize = "C: Deserialize<'de>"))]
pub struct Dataflows<A: Arch, C> {
    /// The memory accesses of these dataflows.
    pub addresses: MemoryAccesses<A>,

    /// The outputs of the dataflows.
    pub outputs: Vec<Dataflow<A, C>>,

    /// Whether any dependent bytes were found during Dataflow Analysis.
    pub found_dependent_bytes: bool,

    #[serde(default)]
    /// Describes the ordering in which outputs must be written in case overlapping outputs are possible.
    /// If there are multiple applicable orderings, they should all be applied.
    /// If multiple orderings apply, they may not conflict.
    pub write_ordering: Vec<WriteOrdering>,
}

fn none<T>() -> Option<T> {
    None
}

/// A single dataflow.
/// Has one target (destination), and zero or more inputs (sources).
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(
    feature = "schemars",
    schemars(bound = "A: schemars::JsonSchema, A::Reg: schemars::JsonSchema, C: schemars::JsonSchema")
)]
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
#[serde(bound(serialize = "C: Serialize", deserialize = "C: Deserialize<'de>"))]
pub struct Dataflow<A: Arch, C> {
    /// The storage location in which the result of the computation of this dataflow is saved.
    pub target: ParLoc<A>,

    /// The sources of the dataflow.
    pub inputs: Inputs<A>,

    /// The computation applied to the inputs to compute the value that is written to `target`.
    #[serde(default = "none::<C>")]
    pub computation: Option<C>,

    /// Whether this dataflow has unobservable external inputs.
    /// If true, `computation` must be `None` and `inputs` should be empty.
    #[serde(default)]
    pub unobservable_external_inputs: bool,
}

impl<A: Arch, C> Dataflow<A, C> {
    /// Returns the inputs of the dataflow.
    #[inline(always)]
    pub fn inputs(&self) -> &Inputs<A> {
        &self.inputs
    }

    /// Returns the storage locatin to which the result of the computation is written.
    #[inline(always)]
    pub fn target(&self) -> &ParLoc<A> {
        &self.target
    }
}

impl<A: Arch, C> Index<usize> for Dataflows<A, C> {
    type Output = Dataflow<A, C>;

    fn index(&self, index: usize) -> &Dataflow<A, C> {
        &self.outputs[index]
    }
}

impl<A: Arch, C> IndexMut<usize> for Dataflows<A, C> {
    fn index_mut(&mut self, index: usize) -> &mut Dataflow<A, C> {
        &mut self.outputs[index]
    }
}

impl<A: Arch, C> Debug for Dataflow<A, C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{:?}] <{}= {{ ",
            self.target,
            if self.unobservable_external_inputs { "*?" } else { "" }
        )?;

        for input in self.inputs.iter() {
            write!(f, "{input:?} ")?;
        }

        write!(f, "}}")?;

        Ok(())
    }
}

impl<A: Arch, C> Dataflows<A, C> {
    /// Iterates over all dataflows.
    pub fn output_dataflows(&self) -> impl Iterator<Item = &Dataflow<A, C>> {
        self.outputs.iter()
    }

    /// Returns the dataflow at position `index`.
    pub fn output_dataflow(&self, index: usize) -> &Dataflow<A, C> {
        &self.outputs[index]
    }

    fn iter_with_locations(&self) -> impl Iterator<Item = FlowGroup<'_, A>> {
        self.outputs.iter().enumerate().map(|(output_index, output)| FlowGroup {
            inputs: &output.inputs,
            target: Some(&output.target),
            location: FlowOutputLocation::Output(output_index),
        })
    }

    fn inputs(&self) -> impl Iterator<Item = (FlowInputLocation, &ParLoc<A>)> {
        self.iter_with_locations().flat_map(|g| g.iter_with_locations())
    }

    /// Returns all sources in these dataflows.
    pub fn values(&self) -> impl Iterator<Item = (FlowValueLocation, ParLoc<A>)> + '_ {
        self.inputs().map(|(loc, input)| (loc.into(), *input)).chain(
            self.iter_with_locations()
                .map(|g| (g.location(), g.target()))
                .filter_map(|x| match x {
                    (FlowOutputLocation::Output(output_index), Some(dest)) => {
                        Some((FlowValueLocation::Output(output_index), *dest))
                    },
                    (FlowOutputLocation::MemoryAccess(_), None) => None,
                    _ => unreachable!(),
                }),
        )
    }

    /// Returns the dataflow with `target` set to `loc`.
    pub fn get(&self, loc: &ParLoc<A>) -> Option<&Dataflow<A, C>> {
        self.outputs.iter().find(|flow| &flow.target == loc)
    }
}

impl<A: Arch, C: Clone + Debug> Semantics<A> for Dataflows<A, C> {
    fn is_part_used_in_computation(&self, part_index: usize) -> bool {
        self.outputs.iter().any(|df| {
            df.target.loc == UnsizedParLoc::Part(part_index)
                || df.inputs.iter().any(|input| input.loc == UnsizedParLoc::Part(part_index))
        })
    }

    fn foreach_loc(&mut self, mut f: impl FnMut(&mut ParLoc<A>)) {
        for access in self.addresses.memory.iter_mut() {
            for input in access.inputs.iter_mut() {
                f(input);
            }
        }

        for df in self.outputs.iter_mut() {
            f(&mut df.target);

            for input in df.inputs.iter_mut() {
                f(input);
            }
        }
    }

    /// Maps each source and destination in the dataflows and memory accesses to new values.
    fn map(
        &self, instr: Instruction, part_values: &[Option<u64>], mut map_flows: impl FnMut(bool, &ParLoc<A>) -> Option<ParLoc<A>>,
        map_address_computations: impl FnMut(usize, &ParameterizedComputation) -> Option<ParameterizedComputation>,
    ) -> Self {
        let mut addresses = self
            .addresses
            .map(|_: FlowValueLocation, val| map_flows(true, val), map_address_computations);
        addresses.memory[0].size = MemorySizeRange::new(instr.byte_len() as u64, instr.byte_len() as u64);
        let outputs = self
            .outputs
            .iter()
            .map(|flow| Dataflow {
                target: map_flows(false, &flow.target).unwrap(),
                inputs: Inputs::unsorted(
                    flow.inputs
                        .iter()
                        .flat_map(|input| map_flows(false, input))
                        .collect::<Vec<_>>(),
                ),
                unobservable_external_inputs: flow.unobservable_external_inputs,
                computation: flow.computation.clone(),
            })
            .collect::<Vec<_>>();

        let write_ordering = self
            .write_ordering
            .iter()
            // Make sure we only keep write orderings that are relevant for this set of part values.
            .filter(|wo| {
                wo.part_values
                    .iter()
                    .zip(part_values.iter())
                    .all(|(val, set)| match (val, set) {
                        (Some(val), Some(set)) => val == set,
                        _ => true,
                    })
            })
            .map(|wo| WriteOrdering {
                part_values: wo
                    .part_values
                    .iter()
                    .zip(part_values.iter())
                    .filter(|(_, set)| !set.is_some())
                    .map(|(&val, _)| val)
                    .collect(),
                output_index_order: wo.output_index_order.clone(),
            })
            .unique()
            .collect::<Vec<_>>();

        Dataflows {
            addresses,
            outputs,
            found_dependent_bytes: self.found_dependent_bytes,
            write_ordering,
        }
    }
}

#[derive(Debug)]
struct FlowGroup<'a, A: Arch> {
    inputs: &'a Inputs<A>,
    target: Option<&'a ParLoc<A>>,
    location: FlowOutputLocation,
}

impl<'a, A: Arch> FlowGroup<'a, A> {
    pub fn location(&self) -> FlowOutputLocation {
        self.location
    }

    pub fn iter_with_locations(&self) -> impl Iterator<Item = (FlowInputLocation, &'a ParLoc<A>)> + use<'a, A> {
        let location = self.location;
        self.inputs
            .iter()
            .enumerate()
            .map(move |(index, el)| (location.input(index), el))
    }

    pub fn target(&self) -> Option<&'a ParLoc<A>> {
        self.target
    }
}
