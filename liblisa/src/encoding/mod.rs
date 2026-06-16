//! The main component of libLISA's semantics is an [`Encoding`].
//! An encoding represents a group of instructions with similar semantics.
//!
//! An encoding consists of two components: [a bitpattern](bitpattern) (for grouping instructions) and [Dataflows](dataflows).

use std::cmp::Ordering;
use std::fmt::{Debug, Display};
use std::hash::Hash;
use std::iter::repeat_with;

use arrayvec::ArrayVec;
use bitcode::{Decode, Encode};
use bitpattern::*;
use dataflows::*;
use log::*;
use prefixes::{BaseInstrError, EquivalentPrefixes};
use rand::Rng;
use rand::seq::IteratorRandom;
use serde::{Deserialize, Serialize};

use crate::arch::Arch;
use crate::instr::{Instruction, InstructionSet};

pub mod bitpattern;
pub mod dataflows;

mod display;
mod locs;
pub mod prefixes;

pub use locs::*;

pub const MAX_NUM_PARTS: usize = 32;

/// An [`Encoding`] with pre-computed InstructionSet.
#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
#[serde(bound(
    serialize = "S: Serialize, M: Serialize",
    deserialize = "S: Deserialize<'de>, M: Deserialize<'de>"
))]
pub struct EncodingWithGraph<A: Arch, S, M> {
    /// The encoding.
    pub encoding: Encoding<A, S, M>,

    /// The precomputed value of `encoding.filters()`.
    pub graph: InstructionSet,
}

impl<A: Arch, S: Display, M> Display for EncodingWithGraph<A, S, M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Display::fmt(&self.encoding, f)
    }
}

impl<A: Arch, S, M> InstructionSpace for EncodingWithGraph<A, S, M> {
    fn as_graph(&self) -> InstructionSet {
        self.graph.clone()
    }
}

impl<A: Arch, S: PartialEq, M: Metadata> PartialEq for EncodingWithGraph<A, S, M> {
    fn eq(&self, other: &Self) -> bool {
        self.encoding == other.encoding
    }
}

impl<A: Arch, S, M> EncodingWithGraph<A, S, M> {
    /// Pre-computes the filters for `encoding`, and returns an [`EncodingWithGraph`].
    pub fn new(encoding: Encoding<A, S, M>) -> Self
    where
        Encoding<A, S, M>: InstructionSpace,
    {
        Self {
            graph: encoding.as_graph(),
            encoding,
        }
    }
}

/// Any type that represents information about a group of bitstrings.
///
/// This can be both semantics ([`Encoding`]), or the lack of valid instructions ([`Uncoding`], [`Errcoding`]).
pub trait InstructionSpace {
    /// Returns a [`InstructionSet`] that describes the space that this type covers.
    fn as_graph(&self) -> InstructionSet;
}

impl InstructionSpace for InstructionSet {
    fn as_graph(&self) -> InstructionSet {
        self.clone()
    }
}

impl InstructionSpace for &InstructionSet {
    fn as_graph(&self) -> InstructionSet {
        (*self).clone()
    }
}

/// [`Dataflows`] and semantics for a group of similar instructions.
/// An encoding matches at least one instruction.
/// If an encoding matches an instruction, it can be *instantiated* for that instruction.
///
/// Instantiation computes the [`Dataflows`] for a specific instruction.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(
    feature = "schemars",
    schemars(
        bound = "A: schemars::JsonSchema, A::Reg: schemars::JsonSchema, Semantics: schemars::JsonSchema, Metadata: schemars::JsonSchema"
    )
)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash, Encode, Decode)]
#[serde(bound(
    serialize = "Semantics: Serialize, Metadata: Serialize",
    deserialize = "Semantics: Deserialize<'de>, Metadata: Deserialize<'de>"
))]
pub struct Encoding<A: Arch, Semantics, Metadata> {
    /// The bitpattern.
    ///
    /// Bits are ordered right-to-left.
    pub bits: Vec<PackedBit>,

    /// Describes the prefixes that can be inserted into the bitstring without changing the semantics of this encoding.
    pub equivalent_prefixes: EquivalentPrefixes,

    /// A part mapping that maps the value of parts to registers/memory computations or immediate values that can be filled in the `dataflows`.
    pub parts: Vec<Part<A>>,

    /// The semantics of the encoding.
    pub semantics: Semantics,

    /// Metadata for the encoding.
    /// This can be any type implementing [`Metadata`].
    #[serde(default)]
    pub metadata: Option<Metadata>,
}

/// Borrowed variant of [`Encoding`].
///
/// Allows each component of the encoding to be borrowed separately.
/// This allows deduplication of components.
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct EncodingRef<'r, A: Arch, Semantics, Metadata> {
    /// The bitpattern.
    ///
    /// Bits are ordered right-to-left.
    pub bits: &'r [PackedBit],

    /// Describes the prefixes that can be inserted into the bitstring without changing the semantics of this encoding.
    pub equivalent_prefixes: &'r EquivalentPrefixes,

    /// A part mapping that maps the value of parts to registers/memory computations or immediate values that can be filled in the `dataflows`.
    pub parts: &'r [Part<A>],

    /// The semantics of the encoding.
    pub semantics: Semantics,

    /// Metadata for the encoding.
    /// This can be any type.
    pub metadata: &'r Option<Metadata>,
}

impl<A: Arch, S: Clone, M> Clone for EncodingRef<'_, A, S, M> {
    fn clone(&self) -> Self {
        Self {
            bits: self.bits,
            equivalent_prefixes: self.equivalent_prefixes,
            parts: self.parts,
            semantics: self.semantics.clone(),
            metadata: self.metadata,
        }
    }
}

impl<A: Arch, S: Copy, M> Copy for EncodingRef<'_, A, S, M> {}

pub trait Semantics<A: Arch>: Debug {
    /// Returns true if the part is used in the computation of storage location values.
    /// Returns false if the part is only used to calculate the memory address.
    fn is_part_used_in_computation(&self, part_index: usize) -> bool;

    /// Remaps all part indices such that part N is remapped to `map[N]`.
    fn map_parts(&mut self, map: &[ParLoc<A>]) {
        self.foreach_loc(|loc| {
            if let ParLoc {
                loc: UnsizedParLoc::Part(n),
                ..
            } = loc
            {
                *loc = map[*n];
            }
        })
    }

    /// Calls `f` on each `ParLoc` used in the semantics.
    /// For example, this could be all `ParLoc`s used to compute memory addresses and all dataflow sources and destinations.
    fn foreach_loc(&mut self, f: impl FnMut(&mut ParLoc<A>));

    fn map(
        &self, instr: Instruction, part_values: &[Option<u64>], map_flows: impl FnMut(bool, &ParLoc<A>) -> Option<ParLoc<A>>,
        map_address_computations: impl FnMut(usize, &ParameterizedComputation) -> Option<ParameterizedComputation>,
    ) -> Self;
}

pub trait Metadata:
    Clone + Debug + Default + PartialEq + Eq + Hash + Send + Sync + Serialize + for<'de> Deserialize<'de>
{
}

impl Metadata for () {}

/// Provides a Deserialize implementation that accepts any type.
/// This allows this type to be used to deserialize and discard any metadata.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default)]
pub struct IgnoredMetadata;

impl PartialEq for IgnoredMetadata {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for IgnoredMetadata {}

impl Hash for IgnoredMetadata {
    fn hash<H: std::hash::Hasher>(&self, _state: &mut H) {}
}

impl From<()> for IgnoredMetadata {
    fn from(_value: ()) -> Self {
        Self::default()
    }
}

impl Metadata for IgnoredMetadata {}

impl From<IgnoredMetadata> for AnalysisMetadata {
    fn from(_: IgnoredMetadata) -> Self {
        Default::default()
    }
}

impl Serialize for IgnoredMetadata {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_none()
    }
}

impl<'de> Deserialize<'de> for IgnoredMetadata {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = IgnoredMetadata;

            fn expecting(&self, _formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                unreachable!()
            }

            fn visit_bool<E>(self, _v: bool) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(IgnoredMetadata)
            }

            fn visit_i64<E>(self, _v: i64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(IgnoredMetadata)
            }

            fn visit_i128<E>(self, _v: i128) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(IgnoredMetadata)
            }

            fn visit_u64<E>(self, _v: u64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(IgnoredMetadata)
            }

            fn visit_u128<E>(self, _v: u128) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(IgnoredMetadata)
            }

            fn visit_f64<E>(self, _v: f64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(IgnoredMetadata)
            }

            fn visit_str<E>(self, _v: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(IgnoredMetadata)
            }

            fn visit_bytes<E>(self, _v: &[u8]) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(IgnoredMetadata)
            }

            fn visit_none<E>(self) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(IgnoredMetadata)
            }

            fn visit_some<D>(self, _deserializer: D) -> Result<Self::Value, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                Ok(IgnoredMetadata)
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(IgnoredMetadata)
            }

            fn visit_newtype_struct<D>(self, _deserializer: D) -> Result<Self::Value, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                Ok(IgnoredMetadata)
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                // Read all elements
                while let Ok(Some(_)) = seq.next_element::<serde_json::Value>() {}
                Ok(IgnoredMetadata)
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                // Read all entries
                while let Ok(Some(_)) = map.next_entry::<serde_json::Value, serde_json::Value>() {}
                Ok(IgnoredMetadata)
            }

            fn visit_enum<A>(self, _data: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::EnumAccess<'de>,
            {
                Ok(IgnoredMetadata)
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AnalysisMetadata {
    pub encoding_analysis_ms_taken: u128,
    pub synthesis_ms_taken: u128,
}

impl PartialEq for AnalysisMetadata {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for AnalysisMetadata {}

impl Hash for AnalysisMetadata {
    fn hash<H: std::hash::Hasher>(&self, _state: &mut H) {}
}

impl From<()> for AnalysisMetadata {
    fn from(_value: ()) -> Self {
        Self::default()
    }
}

impl Metadata for AnalysisMetadata {}

/// Describes the order in which outputs should be written, if the part_values match.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode, Hash)]
pub struct WriteOrdering {
    /// The ordering is applicable when the part values for an instruction match these part values.
    /// A value of `None` means that any part value matches.
    pub part_values: Vec<Option<u64>>,

    /// The order that needs to be applied if the part values match.
    pub output_index_order: Vec<usize>,
}

impl<A: Arch, S, M> InstructionSpace for Encoding<A, S, M> {
    fn as_graph(&self) -> InstructionSet {
        InstructionSet::create_from_encoding(self, ())
    }
}

#[derive(Clone, Debug)]
pub enum ExtractPartsError {
    PrefixesDontMatch,
    LengthMismatch {
        instr: Instruction,
        expected: Instruction,
        original_instr: Instruction,
    },
    FixedBitMismatch {
        bit_index: usize,
        bit: Bit,
        instr: Instruction,
        expected: Instruction,
    },
    InvalidPartValue {
        part_index: usize,
        value: u64,
    },
}

impl Display for ExtractPartsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExtractPartsError::PrefixesDontMatch => write!(f, "prefixes don't match"),
            ExtractPartsError::LengthMismatch {
                original_instr,
                instr,
                expected,
            } => write!(
                f,
                "instruction length mismatch {} vs expected {}: {instr:X} (original: {original_instr:X}) vs {expected:X}",
                instr.byte_len(),
                expected.byte_len()
            ),
            ExtractPartsError::FixedBitMismatch {
                bit_index,
                bit,
                instr,
                expected,
            } => write!(
                f,
                "fixed bit {bit:?} at index {bit_index} doesn't match: {instr:X} (bit is {}) vs expected {expected:X} (bit is {})",
                instr.nth_bit_from_right(*bit_index),
                expected.nth_bit_from_right(*bit_index)
            ),
            ExtractPartsError::InvalidPartValue {
                part_index,
                value,
            } => write!(f, "invalid value for part {part_index}: {value:X}"),
        }
    }
}

impl<A: Arch, S, M> Encoding<A, S, M> {
    pub fn as_ref(&self) -> EncodingRef<'_, A, &S, M> {
        EncodingRef {
            bits: &self.bits,
            equivalent_prefixes: &self.equivalent_prefixes,
            parts: &self.parts,
            semantics: &self.semantics,
            metadata: &self.metadata,
        }
    }

    pub fn map_metadata<N>(self, map: impl FnOnce(Option<M>) -> N) -> Encoding<A, S, N> {
        Encoding {
            metadata: Some(map(self.metadata)),
            bits: self.bits,
            equivalent_prefixes: self.equivalent_prefixes,
            parts: self.parts,
            semantics: self.semantics,
        }
    }

    /// Computes the [`Instruction`] that corresponds to the part values provided.
    /// When a part value is `None`, the current part value of the Encoding is picked.
    /// You must ensure that the part values are valid for this encoding.
    /// Passing invalid part values produces an [`Instruction`] that is not covered by the encoding.
    pub fn part_values_to_instr<T: Copy + Debug + Into<Option<u64>>>(&self, part_values: &[T]) -> Instruction {
        let (new_instr, shift_values) = self.part_values_to_instr_internal(part_values);

        if shift_values.iter().flatten().any(|&v| v != 0) {
            panic!(
                "Part values out of range: {part_values:X?} for parts {:?} in bitpattern {:?}",
                self.parts, self.bits
            );
        }

        new_instr
    }

    /// Computes the [`Instruction`] that corresponds to the part values provided.
    /// When a part value is `None`, the current part value of the Encoding is picked.
    /// Invalid values are inserted into the instruction as if they were valid.
    pub fn part_values_to_instr_unchecked<T: Copy + Debug + Into<Option<u64>>>(&self, part_values: &[T]) -> Instruction {
        self.part_values_to_instr_internal(part_values).0
    }

    fn part_values_to_instr_internal<T: Copy + Debug + Into<Option<u64>>>(
        &self, part_values: &[T],
    ) -> (Instruction, ArrayVec<Option<u64>, 32>) {
        let mut new_instr = self.instr();
        let mut shift_values = part_values
            .iter()
            .map(|&t| -> Option<u64> { t.into() })
            .collect::<ArrayVec<_, MAX_PARTS>>();
        let mut index = 0;
        while index < self.bits.len() {
            let kind = self.bits[index];

            if let Bit::Part(n) = kind.into()
                && let Some(part_value) = &mut shift_values[n as usize]
            {
                if index.is_multiple_of(8) {
                    let num_matching = &self.bits[index + 1..].iter().take_while(|&&k| k == kind).count() + 1;

                    if num_matching >= 8 {
                        let num_matching = num_matching / 8 * 8;

                        new_instr.set_multiple_bits_from_right(index, *part_value, num_matching);
                        index += num_matching;

                        match num_matching.cmp(&64) {
                            Ordering::Less => *part_value >>= num_matching,
                            Ordering::Equal => *part_value = 0,
                            Ordering::Greater => panic!("A part {n} is larger than 64 bits"),
                        }

                        continue
                    }
                }

                new_instr.set_nth_bit_from_right(index, *part_value as u8 & 1);
                *part_value >>= 1;
            }

            index += 1;
        }
        (new_instr, shift_values)
    }

    /// Computes the [`Instruction`] that corresponds to the part values provided.
    /// You must ensure that the part values are valid for this encoding.
    /// Passing invalid part values produces an [`Instruction`] that is not covered by the encoding.
    pub fn all_part_values_to_instr(&self, part_values: &[u64]) -> Instruction {
        let mut new_instr: Instruction = self.instr();
        let mut shift_values = part_values.iter().copied().collect::<ArrayVec<_, MAX_PARTS>>();
        let mut index = 0;
        while index < self.bits.len() {
            let kind = self.bits[index];

            if let Bit::Part(n) = kind.into() {
                let part_value = &mut shift_values[n as usize];
                if index.is_multiple_of(8) {
                    let num_matching = &self.bits[index + 1..].iter().take_while(|&&k| k == kind).count() + 1;

                    if num_matching >= 8 {
                        let num_matching = num_matching / 8 * 8;

                        new_instr.set_multiple_bits_from_right(index, *part_value, num_matching);
                        index += num_matching;

                        match num_matching.cmp(&64) {
                            Ordering::Less => *part_value >>= num_matching,
                            Ordering::Equal => *part_value = 0,
                            Ordering::Greater => panic!("A part {n} is larger than 64 bits"),
                        }

                        continue
                    }
                }

                new_instr.set_nth_bit_from_right(index, *part_value as u8 & 1);
                *part_value >>= 1;
            }

            index += 1;
        }

        if shift_values.iter().any(|&v| v != 0) {
            panic!("Part values out of range: {part_values:X?}");
        }

        new_instr
    }

    /// Returns the current [`Instruction`] of the encoding.
    /// While encodings cover a group of instructions, the dataflows and memory accesses are always instantiated for a specific instruction.
    /// The [`Encoding::canonicalize`] function changes the current instruction to the instruction where all part have the lowest valid value.
    /// This means that the [`Instruction`] of an encoding typically has mostly 0s for the bits that are parts or DontCare bits.
    pub fn instr(&self) -> Instruction {
        self.as_ref().instr()
    }

    /// Parses the part values in `instr` according to the bitpattern.
    ///
    /// Panicks if the provided instruction `instr` is not covered by the encoding,
    /// or if a part value is not valid according to the part mapping.
    pub fn extract_parts(&self, instr: &Instruction) -> Vec<u64> {
        self.as_ref().extract_parts(instr)
    }

    /// Parses the part values in `instr` according to the bitpattern.
    ///
    /// Returns `None` if the provided instruction `instr` is not covered by the encoding,
    /// or if a part value is not valid according to the part mapping.
    pub fn try_extract_parts(&self, original_instr: &Instruction) -> Result<Vec<u64>, ExtractPartsError> {
        self.as_ref().try_extract_parts(original_instr)
    }

    /// Extracts a base instruction, by replacing all prefixes in `instr` with an equivalent substitute that matches the bitpattern.
    /// The instruction can then be parsed by [`Self::extract_parts`] or [`Self::try_extract_parts`].
    ///
    /// This function does not verify whether the parts in the instruction have valid values.
    /// To verify this, the resulting instruction can be passed to [`Self::try_extract_parts`].
    pub fn try_extract_base_instr(&self, instr: Instruction) -> Result<Instruction, BaseInstrError> {
        self.as_ref().try_extract_base_instr(instr)
    }
}

impl<A: Arch, S: Semantics<A>, M> Encoding<A, S, M> {
    fn instantiate_semantics(
        &self, part_values: &[Option<u64>], new_indices: &[Option<usize>],
    ) -> Result<(Instruction, S), InstantiationError>
    where
        M: Metadata,
    {
        assert_eq!(
            self.parts.len(),
            part_values.len(),
            "A value must be specified for every part"
        );
        assert!(
            self.parts.len() <= MAX_PARTS,
            "We can't handle more than MAX_PARTS={MAX_PARTS} parts"
        );

        for (value, part) in part_values.iter().zip(self.parts.iter()) {
            let num_bits = part.size;
            if let Some(value) = value
                && num_bits < 64
            {
                if let PartMapping::Imm {
                    mapping: Some(MappingOrBitOrder::BitOrder(_bit_order)),
                    ..
                } = &part.mapping
                {
                    // TODO: This somehow seems to work okay-ish despite being incomplete?
                } else {
                    assert!(
                        *value <= 1 << num_bits,
                        "Part values: {part_values:X?} not valid for {self:?}; TODO: If this is a BitOrder, we should check if only bits present in the bit order are set instead..."
                    );
                }

                if !part.mapping.value_is_valid(*value) {
                    return Err(match part.mapping {
                        PartMapping::Imm {
                            ..
                        } => InstantiationError::MissingImmValueMapping,
                        PartMapping::MemoryComputation {
                            ..
                        } => InstantiationError::MissingMemoryComputationMapping,
                        PartMapping::Register {
                            ..
                        } => InstantiationError::MissingRegisterMapping,
                    })
                }
            }
        }

        // Make sure MAX_PARTS isn't increased beyond the assumptions we make below
        #[allow(clippy::assertions_on_constants)]
        {
            assert!(MAX_PARTS <= 64);
        }

        #[derive(Clone, Debug)]
        enum Interp<R> {
            Reg(R),
            Const { mem_value: u64, imm_value: u64 },
            Addr(AddressComputation),
            None,
        }

        let mut missing_register_mapping = false;
        let mut missing_imm_mapping = false;
        let mut missing_mem_mapping = false;
        let part_interp = self
            .parts
            .iter()
            .zip(part_values.iter().copied())
            .map(|(part, part_value)| {
                if let Some(part_value) = part_value {
                    match &part.mapping {
                        PartMapping::Register {
                            mapping, ..
                        } => {
                            let reg = if let Some(value) = &mapping[part_value as usize] {
                                *value
                            } else {
                                missing_register_mapping = true;
                                return Interp::None
                            };

                            Interp::Reg(reg)
                        },
                        PartMapping::Imm {
                            mapping,
                            bits,
                            ..
                        } => {
                            // Correct for immediate value bits that we might have removed with remove_bits
                            let imm_value = bits
                                .as_ref()
                                .map(|bits| bits.interpret_value(part_value))
                                .unwrap_or(part_value);

                            // TODO: Should this be `.unwrap_or(Some(imm_value))`?
                            if let Some(mem_value) = mapping
                                .as_ref()
                                .map(|mapping| mapping.compute(imm_value))
                                .unwrap_or(Some(part_value))
                            {
                                Interp::Const {
                                    mem_value,
                                    imm_value,
                                }
                            } else {
                                missing_imm_mapping = true;
                                Interp::None
                            }
                        },
                        PartMapping::MemoryComputation {
                            mapping, ..
                        } => {
                            if let Some(mapped_value) = &mapping[part_value as usize] {
                                Interp::Addr(mapped_value.clone())
                            } else {
                                missing_mem_mapping = true;
                                Interp::None
                            }
                        },
                    }
                } else {
                    Interp::None
                }
            })
            .collect::<ArrayVec<_, MAX_PARTS>>();

        if missing_register_mapping {
            return Err(InstantiationError::MissingRegisterMapping)
        }

        if missing_imm_mapping {
            return Err(InstantiationError::MissingImmValueMapping);
        }

        if missing_mem_mapping {
            return Err(InstantiationError::MissingMemoryComputationMapping);
        }

        let new_instr = self.part_values_to_instr(part_values);
        let dataflows = self.semantics.map(new_instr, part_values, |is_address, old| {
            let mut result = *old;

            if let UnsizedParLoc::Part(part_index) = old.loc {
                match part_interp[part_index] {
                    Interp::Reg(new_reg) => result.loc = UnsizedParLoc::Reg(new_reg),
                    Interp::Const { mem_value, imm_value, .. } => if is_address {
                        result = ParLoc { loc: UnsizedParLoc::Const(mem_value), size: old.size };
                    } else {
                        result = ParLoc { loc: UnsizedParLoc::Const(imm_value), size: old.size };
                    },
                    Interp::Addr(_) => unreachable!(),
                    Interp::None => if let Some(new_part_index) = new_indices[part_index] {
                        result.loc = UnsizedParLoc::Part(new_part_index)
                    } else {
                        // if new_indices[*n] is None, then it will be replaced with a Const or Reg in the match above
                        panic!("Encoding is not valid: {self:#?}; found {result:?} is_address={is_address} which is not being remapped to a new index");
                    }
                }
            }

            Some(result)
        }, |memory_index, old_computation| if let ParameterizedComputation::FromPart(part_index) = old_computation {
            match part_interp[*part_index] {
                Interp::Addr(ref addr) => Some(ParameterizedComputation::Calculation(addr.clone())),
                Interp::Const { .. } => unreachable!(),
                _ => if let Some(new_part_index) = new_indices[*part_index] {
                    Some(ParameterizedComputation::FromPart(new_part_index))
                } else {
                    panic!("Encoding is not valid: {self:#?}; found {old_computation:?} at memory access {memory_index} which is not being remapped to a new index -- part interp = {part_interp:#?}\nnew indices: {new_indices:?}");
                },
            }
        } else {
            None
        });

        Ok((new_instr, dataflows))
    }

    fn compute_new_indices(part_values: &[Option<u64>]) -> ArrayVec<Option<usize>, MAX_PARTS> {
        let mut new_indices = ArrayVec::new();
        let mut n = 0;
        for part_value in part_values.iter() {
            if part_value.is_none() {
                new_indices.push(Some(n));
                n += 1;
            } else {
                new_indices.push(None);
            }
        }

        new_indices
    }

    /// Instantiates the encoding with the provided part values.
    /// Returns the dataflows for the instruction that corresponds to these part values.
    ///
    /// Use [`Self::extract_parts`] to convert a covered [`Instruction`] into part values.
    pub fn instantiate(&self, part_values: &[u64]) -> Result<S, InstantiationError>
    where
        M: Metadata,
    {
        let part_values = part_values.iter().map(|&v| Some(v)).collect::<ArrayVec<_, MAX_PARTS>>();
        let new_indices = Self::compute_new_indices(&part_values);

        self.instantiate_semantics(&part_values, &new_indices).map(|(_, x)| x)
    }

    /// Iterates over covered instructions randomly.
    /// May yield the same instruction multiple times.
    /// Does not terminate.
    ///
    /// `part_values` can be used to restrict the yielded instructions to have fixed part values.
    /// Even if all `part_values` are set, different instructions might be yielded if one or more bits are [`Bit::DontCare`].
    pub fn random_instrs<'a>(
        &'a self, part_value: &'a [Option<u64>], rng: &'a mut impl Rng,
    ) -> impl Iterator<Item = Instruction> + 'a {
        repeat_with(move || {
            let parts = self
                .parts
                .iter()
                .zip(part_value.iter())
                .map(|(p, fixed_value)| {
                    fixed_value.unwrap_or_else(|| match p.mapping.valid_values() {
                        Some(iter) => iter.choose(rng).unwrap() as u64,
                        _ => {
                            (match p.mapping {
                                PartMapping::Imm {
                                    ..
                                } => randomized_value(rng),
                                _ => rng.random::<u64>(),
                            }) & !(u64::MAX.checked_shl(p.size as u32).unwrap_or(0))
                        },
                    })
                })
                .collect::<ArrayVec<_, MAX_NUM_PARTS>>();
            let mut instr = self.all_part_values_to_instr(&parts);

            for (index, bit) in self.bits.iter().enumerate() {
                if let Bit::DontCare = bit.into() {
                    instr.set_nth_bit_from_right(index, rng.random_range(0..=1));
                }
            }

            instr
        })
    }
}

/// Returns a randomized `u64` value.
pub fn randomized_value<R: Rng>(rng: &mut R) -> u64 {
    const TOPMOST_BIT: u64 = 0x8000_0000_0000_0000;
    let v = rng.random::<u64>();
    // Accept a bit of bias to avoid the overhead of random_range(..)
    let k = rng.random::<u16>() as u32 % (65 * 64);

    // The topmost bit in v is never used, so we can re-use it later on to avoid a call to rng.random().
    let leftover_rng_bit = (v & TOPMOST_BIT) != 0;
    // The next bit is likely shifted out as well,
    let mostly_leftover_rng_bit = (v & (TOPMOST_BIT >> 1)) != 0;
    let zeros = k % 64;
    let shift = k / 64;

    let v = v << zeros;

    // Always set the top bit, because we decide the number of prefixed zeroes separately
    // This makes the number of prefix zeroes nicely uniform
    let v = v | TOPMOST_BIT;

    let v = v.checked_shr(shift).unwrap_or(0);

    if leftover_rng_bit {
        if shift > 18 && mostly_leftover_rng_bit {
            // up to "48-bit" memory address that can actually be mapped
            // we're cropping to just 46 bits, because:
            // - bit 48 is equal to bits 49..64, so it has to be 0 to make the address mappable (a 1 in bit 48..64 is an address reserved for kernel use)
            // - bit 47 is 0, because the highest mappable address is 0x7fff_ffff_e000, not 0x7fff_ffff_f000. So it would be impossible to generate something mappable ending in .._ffff_ffff if bit 47 was set (and it would be set, because we're negating the number here).
            (!v) & 0x0000_3fff_ffff_ffff
        } else {
            // 64-bit negative number
            !v
        }
    } else {
        // (up to) 64-bit positive number
        v
    }
}

impl<A: Arch, S, M> EncodingRef<'_, A, S, M> {
    /// Returns the current [`Instruction`] of the encoding.
    /// While encodings cover a group of instructions, the dataflows and memory accesses are always instantiated for a specific instruction.
    /// The [`Encoding::canonicalize`] function changes the current instruction to the instruction where all part have the lowest valid value.
    /// This means that the [`Instruction`] of an encoding typically has mostly 0s for the bits that are parts or DontCare bits.
    pub fn instr(&self) -> Instruction {
        let mut k = [0; MAX_PARTS];
        let mut i = Instruction::new(&[0x00; 16][..self.bits.len() / 8]);
        for (index, bit) in self.bits.iter().enumerate() {
            i.set_nth_bit_from_right(
                index,
                match bit.into() {
                    bitpattern::Bit::Part(part_index) => {
                        let b = (self.parts[part_index as usize].value >> k[part_index as usize]) as u8 & 1;
                        k[part_index as usize] += 1;
                        b
                    },
                    bitpattern::Bit::DontCare => 0,
                    bitpattern::Bit::Fixed(val) => val,
                },
            );
        }

        i
    }

    /// Extracts a base instruction, by replacing all prefixes in `instr` with an equivalent substitute that matches the bitpattern.
    /// The instruction can then be parsed by [`Self::extract_parts`] or [`Self::try_extract_parts`].
    ///
    /// This function does not verify whether the parts in the instruction have valid values.
    /// To verify this, the resulting instruction can be passed to [`Self::try_extract_parts`].
    pub fn try_extract_base_instr(&self, instr: Instruction) -> Result<Instruction, BaseInstrError> {
        let base_instr = self.equivalent_prefixes.compute_base_instr(instr)?;
        let expected_len = self.bits.len() / 8;

        match base_instr.byte_len().cmp(&expected_len) {
            Ordering::Less => Err(BaseInstrError::NeedMoreBytes(expected_len - base_instr.byte_len())),
            Ordering::Equal => Ok(base_instr),
            Ordering::Greater => Err(BaseInstrError::TooManyBytes(base_instr.byte_len() - expected_len)),
        }
    }

    /// Parses the part values in `instr` according to the bitpattern.
    ///
    /// Panicks if the provided instruction `instr` is not covered by the encoding,
    /// or if a part value is not valid according to the part mapping.
    pub fn extract_parts(&self, instr: &Instruction) -> Vec<u64> {
        match self.try_extract_parts(instr) {
            Ok(parts) => parts,
            Err(e) => panic!("unable to extract parts from instruction {instr:X?}: {e}"),
        }
    }

    /// Parses the part values in `instr` according to the bitpattern.
    ///
    /// Returns `None` if the provided instruction `instr` is not covered by the encoding,
    /// or if a part value is not valid according to the part mapping.
    pub fn try_extract_parts(&self, original_instr: &Instruction) -> Result<Vec<u64>, ExtractPartsError> {
        let instr = self
            .equivalent_prefixes
            .canonicalize_instr(*original_instr)
            .ok_or(ExtractPartsError::PrefixesDontMatch)?;
        let instr = if instr.bit_len() > self.bits.len() {
            instr.resize(self.bits.len() / 8, 0)
        } else {
            instr
        };

        if instr.bit_len() != self.bits.len() {
            debug!(
                "Length mismatch in try_extract_parts: Instr: {instr:X?}, bits: {:?}",
                self.bits
            );
            return Err(ExtractPartsError::LengthMismatch {
                original_instr: *original_instr,
                instr,
                expected: self.instr(),
            })
        }

        let mut parts = vec![0u64; self.parts.len()];
        let mut part_indices = vec![0usize; self.parts.len()];
        for (bit_index, bit) in self.bits.iter().enumerate() {
            match bit.into() {
                Bit::Part(n) => {
                    parts[n as usize] |= (instr.nth_bit_from_right(bit_index) as u64) << part_indices[n as usize];
                    part_indices[n as usize] += 1;
                },
                Bit::Fixed(v) => {
                    if instr.nth_bit_from_right(bit_index) != v {
                        trace!(
                            "The instruction does not match the encoding; You cannot instantiate it: {instr:X} = {instr:?} vs {:?}; Bit {bit_index} is different",
                            self.bits
                        );
                        return Err(ExtractPartsError::FixedBitMismatch {
                            bit_index,
                            bit: bit.into(),
                            instr,
                            expected: self.instr(),
                        })
                    }
                },
                _ => (),
            }
        }

        for (part_index, (part, &value)) in self.parts.iter().zip(parts.iter()).enumerate() {
            if !part.mapping.value_is_valid(value) {
                return Err(ExtractPartsError::InvalidPartValue {
                    part_index,
                    value,
                })
            }
        }

        Ok(parts)
    }

    pub fn to_owned(&self) -> Encoding<A, S, M>
    where
        S: Clone,
        M: Clone,
    {
        Encoding {
            bits: self.bits.to_vec(),
            equivalent_prefixes: self.equivalent_prefixes.clone(),
            parts: self.parts.to_vec(),
            semantics: self.semantics.clone(),
            metadata: self.metadata.clone(),
        }
    }
}

/// Error returned by [`Encoding::integrity_check`].
#[derive(Clone, Debug, thiserror::Error)]
pub enum IntegrityError {
    /// The output contains a reference to an immediate value of a non-existant part
    #[error("a non-existant part {} is referenced in the semantics", .part_index)]
    UnknownPart {
        /// The index of the non-existant part.
        part_index: usize,
    },

    /// Part has no valid mapping.
    #[error("part {} has no valid mapping", .part_index)]
    PartHasNoValidMapping {
        /// The index of the part in the encoding.
        part_index: usize,
    },

    /// The part mapping has a number of entries that is not equal to the number it should have.
    /// A part mapping should have `2**N` entries.
    #[error("the mapping of part {} ({} entries) does not match the part's size ({} bits)", .part_index, .mapping_entries, .part_bits)]
    MappingDoesNotMatchPartSize {
        /// The index of the part in the encoding.
        part_index: usize,

        /// The number of entries in the part
        mapping_entries: usize,

        /// The number of `Bit::Part(part_index)` bits in `encoding.bits`.
        part_bits: usize,
    },

    /// The part size does not correspond to the number of `Bit::Part(..)` bits in `self.bits`.
    #[error("the size of part {part_index} does not match the number of bits marked as BitKind::Part({part_index})", part_index = .part_index)]
    PartSizeDoesNotMatchBits {
        /// The index of the part in the encoding.
        part_index: usize,
    },

    /// The currently selected value is bigger than the maximum value that can be encoded in the number of bits indicated by the part size `part.size`.
    #[error("the size of part {part_index} is not large enough to encode the value 0x{value:X}", part_index = .part_index, value = .value)]
    ValueDoesNotFitInPartSize {
        /// The index of the part in the encoding.
        part_index: usize,

        /// The value of the part.
        value: u64,
    },
    #[error(
        "the equivalent prefixes graph should match the first {} bytes of the encoding, but it matched the first {} instead",
        expected_len,
        found_len
    )]
    EquivalentPrefixMatchLengthDifference { expected_len: usize, found_len: usize },
}

/// An error returned when instantiating an [`Encoding`] fails.
#[derive(Debug, Clone, thiserror::Error)]
pub enum InstantiationError {
    /// An invalid part value was passed, that doesn't map to any register.
    #[error("missing register mapping")]
    MissingRegisterMapping,

    /// An invalid part value was passed, that doesn't map to any memory computation
    #[error("missing memory computation mapping")]
    MissingMemoryComputationMapping,

    #[error("part values invalid")]
    InvalidPartValue,

    #[error("failed to extract parts: {}", .0)]
    FailedToExtractParts(ExtractPartsError),

    /// An invalid part value was passed, that doesn't map to any valid immediate value
    #[error("missing imm value mapping")]
    MissingImmValueMapping,
}

/// An error returned when [`Encoding::restrict_to`] fails.
#[derive(Debug, Clone, thiserror::Error)]
pub enum RestrictError {
    /// The provided filter does not overlap with the encoding.
    #[error("no overlap between restriction filter and encoding")]
    NoOverlap,
}
