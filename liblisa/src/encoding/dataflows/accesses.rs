use std::fmt::{Debug, Display};
use std::ops::{Index, IndexMut};

use bitcode::{Decode, Encode};
use itertools::Itertools;
use serde::{Deserialize, Serialize};

use super::AddrSize;
use crate::arch::{Arch, Register};
use crate::encoding::bitpattern::FlowValueLocation;
use crate::encoding::dataflows::{AddressComputation, Inputs};
use crate::encoding::{ParLoc, UnsizedParLoc};
use crate::instr::Instruction;
use crate::state::{Addr, Area, Size, SystemState, UnsizedLoc};

/// A collection of memory accesses.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "mem_dbg", derive(mem_dbg::MemSize))]
#[cfg_attr(
    feature = "schemars",
    schemars(bound = "A: schemars::JsonSchema, A::Reg: schemars::JsonSchema")
)]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
#[serde(bound = "")]
pub struct MemoryAccesses<A: Arch> {
    /// The set of memory accesses performed by `instr`.
    pub memory: Vec<MemoryAccess<A>>,

    /// Whether the trap flag should be used to observe `instr`.
    /// Memory access analysis will detect when an instruction can jump, and if so, set this field to true.
    pub use_trap_flag: bool,
}

impl<'a, A: Arch> arbitrary::Arbitrary<'a> for MemoryAccesses<A> {
    fn arbitrary(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        let instr = u.arbitrary::<Instruction>()?;
        let mut memory = vec![MemoryAccess::<A> {
            kind: AccessKind::Executable,
            inputs: Inputs::unsorted(vec![ParLoc {
                loc: UnsizedParLoc::Reg(A::reg(A::PC)),
                size: Size::qword(),
            }]),
            size: MemorySizeRange::new(instr.byte_len() as u64, instr.byte_len() as u64),
            calculation: AddressComputation::unscaled_sum(1, AddrSize::Addr64).into(),
            alignment: 1,
        }];

        for _ in 0..u.int_in_range(0..=63)? {
            let size = u.int_in_range(1..=32)?;
            let calculation = u.arbitrary::<AddressComputation>()?;
            let max_alignment_bits = match size {
                1 => 0,
                2 => 1,
                3..=4 => 2,
                5..=8 => 3,
                9..=16 => 4,
                17..=32 => 5,
                _ => 6,
            };
            let alignment = if max_alignment_bits > 0 {
                1 << u.int_in_range(1..=max_alignment_bits)?
            } else {
                1
            };

            let mut inputs_choices = A::iter_gpregs().filter(|reg| !reg.is_flags()).collect::<Vec<_>>();

            let inputs = (0..calculation.num_terms())
                .map(|_| -> arbitrary::Result<_> {
                    let index = u.int_in_range(0..=inputs_choices.len() - 1)?;
                    Ok(ParLoc {
                        loc: UnsizedParLoc::Reg(A::reg(inputs_choices.remove(index))),
                        size: Size::qword(),
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;

            assert_eq!(inputs.len(), calculation.num_terms());

            memory.push(MemoryAccess {
                kind: AccessKind::InputOutput,
                size: MemorySizeRange::new(size, size),
                calculation: calculation.into(),
                inputs: Inputs::sorted(inputs),
                alignment,
            })
        }

        Ok(MemoryAccesses {
            // instr,
            memory,
            use_trap_flag: u.arbitrary::<bool>()?,
        })
    }
}

impl<A: Arch> Index<usize> for MemoryAccesses<A> {
    type Output = MemoryAccess<A>;

    #[inline]
    fn index(&self, index: usize) -> &MemoryAccess<A> {
        &self.memory[index]
    }
}

impl<A: Arch> IndexMut<usize> for MemoryAccesses<A> {
    #[inline]
    fn index_mut(&mut self, index: usize) -> &mut MemoryAccess<A> {
        &mut self.memory[index]
    }
}

/// The type of access that is performed.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "mem_dbg", derive(mem_dbg::MemSize))]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
pub enum AccessKind {
    /// The memory is only read.
    Input,

    /// The memory is written (and potentially also read).
    InputOutput,

    /// The memory contains the instruction that is executed.
    Executable,
}

#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "mem_dbg", derive(mem_dbg::MemSize))]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
pub struct MemorySizeRange {
    pub start: u64,
    pub end: u64,
}

impl MemorySizeRange {
    pub fn new(start: u64, end: u64) -> Self {
        Self {
            start,
            end,
        }
    }

    pub fn single(len: u64) -> Self {
        Self {
            start: len,
            end: len,
        }
    }
}

/// A memory access.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "mem_dbg", derive(mem_dbg::MemSize))]
#[cfg_attr(
    feature = "schemars",
    schemars(bound = "A: schemars::JsonSchema, A::Reg: schemars::JsonSchema")
)]
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
#[serde(bound = "")]
pub struct MemoryAccess<A: Arch> {
    /// The type of access that is performed.
    pub kind: AccessKind,

    /// The inputs for `calculation`.
    pub inputs: Inputs<A>,

    /// The determined size range of the access.
    /// The lower bound is the largest number of bytes that could be observed as being acesssed.
    /// The upper bound is set to one below the smallest byte index for which we could observe that it was not accessed.
    pub size: MemorySizeRange,

    /// A simple expression for the calculation of the address of the form i1 + i2 + .. + i_k * c + i_k+1 + i_k+1 .. i_n + c.
    /// That is, all inputs are summed, one input can be multiplied by a certain factor, which is then offset by a constant value c.
    /// This allows for the computation of most common addresses. It speeds up enumeration by ~20% on average up to ~40% in extreme
    /// cases. Obviously the speedup gets bigger if the amount of memory accesses increases or if the number of randomize_new() and
    /// adapt() calls is greater.
    pub calculation: ParameterizedComputation,

    /// An alignment of 1 means that every address is OK. An alignment of 2 means that only addresses of the form 2n are ok.
    /// An alignment of 4 means that addresses of the form 4n are OK. Etc.
    /// NOTE: Only powers of 2 are acceptable values. Any other value will produce unspecified behavior.
    pub alignment: usize,
}

#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "mem_dbg", derive(mem_dbg::MemSize))]
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
pub enum ParameterizedComputation {
    FromPart(usize),
    Calculation(AddressComputation),
}

impl From<AddressComputation> for ParameterizedComputation {
    fn from(value: AddressComputation) -> Self {
        Self::Calculation(value)
    }
}

impl ParameterizedComputation {
    #[inline]
    pub fn unwrap_calculation(&self) -> &AddressComputation {
        if let Self::Calculation(calc) = self {
            calc
        } else {
            panic!("unable to unwrap AddressComputation that has been parameterized: {self:?}")
        }
    }

    #[inline]
    pub fn unwrap_mut_calculaton(&mut self) -> &mut AddressComputation {
        if let Self::Calculation(calc) = self {
            calc
        } else {
            panic!("unable to unwrap AddressComputation that has been parameterized: {self:?}")
        }
    }
}

impl<A: Arch> Debug for MemoryAccess<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut f = f.debug_struct("MemoryAccess");
        f.field("kind", &self.kind)
            .field("inputs", &self.inputs)
            .field("size", &self.size);

        match &self.calculation {
            ParameterizedComputation::Calculation(c) => {
                f.field("calculation", &c.display(&["A", "B", "C", "D"][0..self.inputs.len()]))
            },
            ParameterizedComputation::FromPart(part) => f.field("calculation", &format!("Part[{part}]")),
        };

        f.field("alignment", &self.alignment).finish()
    }
}

impl<A: Arch> MemoryAccess<A> {
    /// The inputs of the address calculation.
    #[inline(always)]
    pub fn inputs(&self) -> &Inputs<A> {
        &self.inputs
    }

    /// Computes the address that this memory access will access, given the provided CPU state.
    #[inline]
    pub fn compute_address(&self, state: &SystemState<A>) -> Addr {
        Addr::new(self.calculation.unwrap_calculation().compute(&self.inputs, state))
    }

    /// Computes the address that this memory access will access, given the provided CPU state.
    #[inline]
    pub fn compute_address_from_cpu_state(&self, state: &A::CpuState, instr_len: u64) -> Addr {
        Addr::new(
            self.calculation
                .unwrap_calculation()
                .compute_from_cpustate(&self.inputs, instr_len, state),
        )
    }

    /// Returns true if the memory address is fixed or only dependent on immediate values in the instruction.
    pub fn has_fixed_addr(&self) -> bool {
        self.inputs.iter().all(|source| {
            matches!(
                source.loc,
                UnsizedParLoc::Part(_) | UnsizedParLoc::Const(_) | UnsizedParLoc::InstrLen
            )
        })
    }

    /// Computes the fixed address for this access.
    /// Only returns a valid value if [`Self::has_fixed_addr`] returns true.
    #[inline]
    pub fn compute_fixed_addr(&self) -> Addr {
        self.compute_address(&SystemState::<A>::default())
    }
}

impl<A: Arch> MemoryAccesses<A> {
    /// Returns the largest number of bytes that can be in the provided storage location.
    pub fn max_size_of(&self, location: &UnsizedLoc<A>) -> usize {
        match location {
            UnsizedLoc::Reg(reg) => reg.byte_size(),
            UnsizedLoc::Memory(index) => self.memory[*index].size.end as usize,
        }
    }

    /// Slices the memory accesses to only include the first `length` accesses.
    pub fn slice(&self, length: usize) -> MemoryAccesses<A> {
        MemoryAccesses {
            // instr: self.instr,
            memory: self.memory[..length].to_vec(),
            use_trap_flag: self.use_trap_flag,
        }
    }

    /// Iterates over all accesses.
    pub fn iter(&self) -> impl Iterator<Item = &MemoryAccess<A>> {
        self.memory.iter()
    }

    /// Returns the number of accesses.
    #[must_use]
    pub fn len(&self) -> usize {
        self.memory.len()
    }

    /// Returns true if there are no accesses.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<A: Arch> MemoryAccesses<A> {
    /// Returns the memory areas read or written by these dataflows.
    pub fn extract_memory_areas<'a>(&'a self, state: &'a SystemState<A>) -> impl Iterator<Item = Area> + 'a {
        self.iter()
            .map(|access| Area::new(access.compute_address(state), access.size.end))
    }

    /// Individually maps all computations and all inputs of all memory accesses to new values.
    pub fn map(
        &self, mut f: impl FnMut(FlowValueLocation, &ParLoc<A>) -> Option<ParLoc<A>>,
        mut map_address_computations: impl FnMut(usize, &ParameterizedComputation) -> Option<ParameterizedComputation>,
    ) -> MemoryAccesses<A> {
        MemoryAccesses {
            memory: self
                .memory
                .iter()
                .enumerate()
                .map(|(memory_index, ma)| {
                    let inputs = Inputs::unsorted(
                        ma.inputs
                            .iter()
                            .enumerate()
                            .flat_map(|(input_index, input)| {
                                f(
                                    FlowValueLocation::MemoryAddress {
                                        memory_index,
                                        input_index,
                                    },
                                    input,
                                )
                            })
                            .collect::<Vec<_>>(),
                    );

                    MemoryAccess {
                        kind: ma.kind,
                        size: ma.size,
                        // We keep the calculation if the number of inputs remains the same
                        calculation: map_address_computations(memory_index, &ma.calculation).unwrap_or(ma.calculation.clone()),
                        inputs,
                        alignment: ma.alignment,
                    }
                })
                .collect::<Vec<_>>(),
            use_trap_flag: self.use_trap_flag,
        }
    }
}

impl Display for AccessKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AccessKind::Input => f.write_str("input"),
            AccessKind::InputOutput => f.write_str("input/output"),
            AccessKind::Executable => f.write_str("executable"),
        }
    }
}

impl<A: Arch> Display for MemoryAccesses<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[")?;

        for (index, access) in self.memory.iter().enumerate() {
            write!(f, "{access}")?;
            if index != self.memory.len() - 1 {
                write!(f, ", ")?;
            }
        }

        write!(f, "]")?;

        Ok(())
    }
}

impl<A: Arch> Display for MemoryAccess<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}[size {}..{}] = ", self.kind, self.size.start, self.size.end)?;

        let input_names = self.inputs.iter().map(|i| format!("{i}")).collect::<Vec<_>>();

        match &self.calculation {
            ParameterizedComputation::Calculation(c) => write!(f, "{}", c.display(&input_names))?,
            ParameterizedComputation::FromPart(part) => {
                write!(f, "Part[{part}] with inputs {}", input_names.iter().format(", "))?
            },
        }

        Ok(())
    }
}
