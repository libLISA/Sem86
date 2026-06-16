use std::ops::Index;

use arrayvec::ArrayVec;
use bitcode::{Decode, Encode};
use liblisa::utils::bitmask_u64;
use mem_dbg::True;
use serde::{Deserialize, Serialize};

#[derive(Copy, Clone, Debug, Serialize, Deserialize, Encode, Decode, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct PartValues(u128);

impl PartValues {
    pub const ALL_ZERO: Self = Self(0);

    pub fn get(&self, ps: &PackingStructure, index: usize) -> u64 {
        let part = &ps.part_values[index];
        (self.0 >> part.offset) as u64 & bitmask_u64(part.len as u32)
    }

    pub fn unpack(&self, ps: &PackingStructure) -> impl Iterator<Item = u64> {
        (0..ps.part_values.len()).map(|index| self.get(ps, index))
    }

    pub fn as_u128(&self) -> u128 {
        self.0
    }

    pub fn from_u128(val: u128) -> PartValues {
        Self(val)
    }
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize, Encode, Decode, PartialEq, Eq, Hash)]
pub struct PackedPart {
    offset: u8,
    len: u8,
}

impl PackedPart {
    pub fn new(offset: u8, len: u8) -> Self {
        Self {
            offset,
            len,
        }
    }

    pub fn offset(&self) -> u8 {
        self.offset
    }

    pub fn len(&self) -> u8 {
        self.len
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Encode, Decode, PartialEq, Eq, Hash)]
pub struct PackingStructure {
    part_values: ArrayVec<PackedPart, 8>,
}

impl mem_dbg::MemSize for PackingStructure {
    fn mem_size(&self, _flags: mem_dbg::SizeFlags) -> usize {
        self.part_values.capacity() * size_of::<PackedPart>()
    }
}

impl mem_dbg::CopyType for PackingStructure {
    type Copy = True;
}

impl PackingStructure {
    pub fn new(parts: impl Iterator<Item = PackedPart>) -> Self {
        Self {
            part_values: parts.collect(),
        }
    }

    pub fn from_part_sizes(sizes: impl IntoIterator<Item = usize>) -> Self {
        let mut offset = 0;
        Self::new(sizes.into_iter().map(|len| {
            let result = PackedPart::new(offset as u8, len as u8);
            offset += len;
            result
        }))
    }

    pub fn pack(&self, parts: &[u64]) -> PartValues {
        PartValues(
            self.part_values
                .iter()
                .zip(parts)
                .map(|(part, &val)| (val as u128) << part.offset)
                .reduce(|a, b| a | b)
                .unwrap_or(0),
        )
    }

    pub fn bits_used(&self) -> usize {
        let Some(last) = self.part_values.last() else { return 0 };

        (last.offset + last.len) as usize
    }
}

impl Index<usize> for PackingStructure {
    type Output = PackedPart;

    fn index(&self, index: usize) -> &Self::Output {
        &self.part_values[index]
    }
}
