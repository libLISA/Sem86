use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fmt::Debug;
use std::sync::Arc;

use fxhash::FxHashMap;
use liblisa::Instruction;
use liblisa::encoding::bitpattern::{PackedBit, Part};
use liblisa::encoding::dataflows::MemoryAccesses;
use liblisa::encoding::prefixes::{BaseInstrError, EquivalentPrefixes};
use liblisa::encoding::{Encoding, EncodingRef, IgnoredMetadata};
use liblisa::instr::{InstructionMap, LookupResult};
use log::trace;
use mem_dbg::{MemSize, SizeFlags};
use serde::{Deserialize, Serialize};

use crate::SegmentSizes;
use crate::arch::intel386::Intel386;
use crate::il::part_values::PackingStructure;
use crate::il::{BorrowEncoding, Commands, MiniSem, MiniSemRef};

pub trait EncodingLookup {
    fn get(&self, index: usize) -> EncodingRef<'_, Intel386, MiniSemRef<'_, Intel386>, IgnoredMetadata>;

    fn maps(&self) -> &InstrMaps;

    fn len(&self) -> usize;
}

impl EncodingLookup for () {
    fn get(&self, _index: usize) -> EncodingRef<'_, Intel386, MiniSemRef<'_, Intel386>, IgnoredMetadata> {
        unimplemented!()
    }

    fn maps(&self) -> &InstrMaps {
        unimplemented!()
    }

    fn len(&self) -> usize {
        unimplemented!()
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct InstrMaps {
    pub c16s16: InstructionMap<usize>,
    pub c32s16: InstructionMap<usize>,
    pub c16s32: InstructionMap<usize>,
    pub c32s32: InstructionMap<usize>,
}

#[derive(Serialize, Deserialize)]
pub struct InstrSem {
    pub maps: InstrMaps,
    pub encodings: Vec<Encoding<Intel386, MiniSem<Intel386>, IgnoredMetadata>>,
}

impl EncodingLookup for InstrSem {
    fn get(&self, index: usize) -> EncodingRef<'_, Intel386, MiniSemRef<'_, Intel386>, IgnoredMetadata> {
        self.encodings[index].borrow_encoding()
    }

    fn maps(&self) -> &InstrMaps {
        &self.maps
    }

    fn len(&self) -> usize {
        self.encodings.len()
    }
}

impl InstrSem {
    pub fn pack(self) -> PackedInstrSem {
        let mut addresses = Vec::new();
        let mut addresses_index = HashMap::new();
        let mut commands = Vec::new();
        let mut commands_index = HashMap::new();
        let mut equivalent_prefixes = Vec::new();
        let mut equivalent_prefixes_index = HashMap::new();
        let mut parts = Vec::new();
        let mut parts_index = HashMap::new();
        let mut part_packing = Vec::new();
        let mut part_packing_index = HashMap::new();
        let mut names = Vec::new();
        let mut names_index = HashMap::new();
        let mut jumps = Vec::new();
        let mut jumps_index = HashMap::new();

        let encodings = self
            .encodings
            .into_iter()
            .map(|e| PackedEncoding {
                is_rep: e.semantics.is_rep,
                name: u16::try_from(*names_index.entry(e.semantics.name.clone()).or_insert_with(|| {
                    let index: usize = names.len();
                    names.push(e.semantics.name.clone());
                    index
                }))
                .unwrap(),
                bits: e.bits,
                equivalent_prefixes: u16::try_from(
                    *equivalent_prefixes_index
                        .entry(e.equivalent_prefixes.clone())
                        .or_insert_with(|| {
                            let index = equivalent_prefixes.len();
                            equivalent_prefixes.push(e.equivalent_prefixes);
                            index
                        }),
                )
                .unwrap(),
                parts: u16::try_from(*parts_index.entry(e.parts.clone()).or_insert_with(|| {
                    let index = parts.len();
                    parts.push(e.parts);
                    index
                }))
                .unwrap(),
                accesses: u16::try_from(*addresses_index.entry(e.semantics.addresses.clone()).or_insert_with(|| {
                    let index = addresses.len();
                    addresses.push(e.semantics.addresses);
                    index
                }))
                .unwrap(),
                commands: u16::try_from(*commands_index.entry(e.semantics.commands.clone()).or_insert_with(|| {
                    let index = commands.len();
                    commands.push(e.semantics.commands);
                    index
                }))
                .unwrap(),
                part_packing: u16::try_from(
                    *part_packing_index.entry(e.semantics.part_packing.clone()).or_insert_with(|| {
                        let index = part_packing.len();
                        part_packing.push(e.semantics.part_packing);
                        index
                    }),
                )
                .unwrap(),
                jump: u16::try_from(*jumps_index.entry(e.semantics.jump.clone()).or_insert_with(|| {
                    let index = jumps.len();
                    jumps.push(e.semantics.jump);
                    index
                }))
                .unwrap(),
            })
            .collect::<Vec<_>>();

        PackedInstrSem {
            maps: self.maps,
            names,
            equivalent_prefixes,
            parts,
            encodings,
            addresses,
            commands,
            part_packing,
            jumps,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, mem_dbg::MemSize)]
struct PackedEncoding {
    bits: Vec<PackedBit>,
    equivalent_prefixes: u16,
    parts: u16,
    name: u16,
    accesses: u16,
    commands: u16,
    part_packing: u16,
    jump: u16,
    is_rep: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PackedInstrSem {
    maps: InstrMaps,
    names: Vec<String>,
    encodings: Vec<PackedEncoding>,
    equivalent_prefixes: Vec<EquivalentPrefixes>,
    parts: Vec<Vec<Part<Intel386>>>,
    addresses: Vec<MemoryAccesses<Intel386>>,
    commands: Vec<Commands<Intel386>>,
    part_packing: Vec<PackingStructure>,
    jumps: Vec<crate::il::Jump<Intel386>>,
}

impl EncodingLookup for PackedInstrSem {
    #[inline(always)]
    fn get(&self, index: usize) -> EncodingRef<'_, Intel386, MiniSemRef<'_, Intel386>, IgnoredMetadata> {
        let e = &self.encodings[index];
        EncodingRef {
            bits: &e.bits,
            equivalent_prefixes: &self.equivalent_prefixes[e.equivalent_prefixes as usize],
            parts: &self.parts[e.parts as usize],
            semantics: MiniSemRef {
                name: &self.names[e.name as usize],
                addresses: &self.addresses[e.accesses as usize],
                commands: &self.commands[e.commands as usize],
                part_packing: &self.part_packing[e.part_packing as usize],
                jump: &self.jumps[e.jump as usize],
                is_rep: e.is_rep,
            },
            metadata: &None,
        }
    }

    fn maps(&self) -> &InstrMaps {
        &self.maps
    }

    fn len(&self) -> usize {
        self.encodings.len()
    }
}

impl PackedInstrSem {
    pub fn unpack(self) -> InstrSem {
        let encodings = self
            .encodings
            .into_iter()
            .map(|e| Encoding {
                bits: e.bits,
                equivalent_prefixes: self.equivalent_prefixes[e.equivalent_prefixes as usize].clone(),
                parts: self.parts[e.parts as usize].clone(),
                semantics: MiniSem {
                    name: self.names[e.name as usize].clone(),
                    addresses: self.addresses[e.accesses as usize].clone(),
                    commands: self.commands[e.commands as usize].clone(),
                    part_packing: self.part_packing[e.part_packing as usize].clone(),
                    jump: self.jumps[e.jump as usize].clone(),
                    is_rep: e.is_rep,
                }
                .to_owned(),
                metadata: None,
            })
            .collect::<Vec<_>>();

        InstrSem {
            maps: self.maps,
            encodings,
        }
    }

    pub fn num_encodings(&self) -> usize {
        self.encodings.len()
    }

    pub fn num_names(&self) -> usize {
        self.names.len()
    }

    pub fn num_equivalent_prefixes(&self) -> usize {
        self.equivalent_prefixes.len()
    }

    pub fn num_parts(&self) -> usize {
        self.parts.len()
    }

    pub fn num_addresses(&self) -> usize {
        self.addresses.len()
    }

    pub fn num_commands(&self) -> usize {
        self.commands.len()
    }

    pub fn num_part_packings(&self) -> usize {
        self.part_packing.len()
    }

    pub fn encodings_mem_size(&self) -> usize {
        self.encodings.mem_size(SizeFlags::CAPACITY)
    }

    pub fn names_mem_size(&self) -> usize {
        self.names.mem_size(SizeFlags::CAPACITY)
    }

    pub fn equivalent_prefixes_mem_size(&self) -> usize {
        self.equivalent_prefixes.mem_size(SizeFlags::CAPACITY)
    }

    pub fn addresses_size(&self) -> usize {
        self.addresses.mem_size(SizeFlags::CAPACITY)
    }

    pub fn parts_size(&self) -> usize {
        self.parts.mem_size(SizeFlags::CAPACITY)
    }

    pub fn part_packings_mem_size(&self) -> usize {
        self.part_packing.mem_size(SizeFlags::CAPACITY)
    }

    pub fn commands_mem_size(&self) -> usize {
        self.commands.mem_size(SizeFlags::CAPACITY)
    }

    pub fn empty() -> PackedInstrSem {
        Self {
            maps: InstrMaps {
                c16s16: InstructionMap::new(),
                c32s16: InstructionMap::new(),
                c16s32: InstructionMap::new(),
                c32s32: InstructionMap::new(),
            },
            names: Vec::new(),
            encodings: Vec::new(),
            equivalent_prefixes: Vec::new(),
            parts: Vec::new(),
            addresses: Vec::new(),
            commands: Vec::new(),
            part_packing: Vec::new(),
            jumps: Vec::new(),
        }
    }
}

#[derive(Clone)]
pub struct Decoder<L> {
    semantics: Arc<L>,
    cache: [FxHashMap<Instruction, usize>; 4],
    cache_too_short: [FxHashMap<Instruction, u8>; 4],
}

impl<L: EncodingLookup> Decoder<L> {
    pub fn lookup(&mut self, instr: Instruction, segment_sizes: SegmentSizes) -> Result<usize, BaseInstrError> {
        let (encodings, cache, too_short) = match segment_sizes {
            SegmentSizes::Cs16Ss16 => (
                &self.semantics.maps().c16s16,
                &mut self.cache[0],
                &mut self.cache_too_short[0],
            ),
            SegmentSizes::Cs16Ss32 => (
                &self.semantics.maps().c16s32,
                &mut self.cache[1],
                &mut self.cache_too_short[1],
            ),
            SegmentSizes::Cs32Ss16 => (
                &self.semantics.maps().c32s16,
                &mut self.cache[2],
                &mut self.cache_too_short[2],
            ),
            SegmentSizes::Cs32Ss32 => (
                &self.semantics.maps().c32s32,
                &mut self.cache[3],
                &mut self.cache_too_short[3],
            ),
        };

        if let Some(n) = too_short.get(&instr) {
            return Err(BaseInstrError::NeedMoreBytes(*n as usize))
        }

        match cache.entry(instr) {
            Entry::Occupied(occupied_entry) => {
                let entry = occupied_entry.into_mut();
                Ok(*entry)
            },
            Entry::Vacant(vacant_entry) => {
                if let LookupResult::Found(&encoding_index) = encodings.get(instr) {
                    let encoding = &self.semantics.get(encoding_index);
                    match encoding.try_extract_base_instr(instr) {
                        Ok(_) => (),
                        Err(e @ BaseInstrError::NeedMoreBytes(n)) => {
                            too_short.insert(instr, n as u8);
                            return Err(e)
                        },
                        Err(e) => return Err(e),
                    }

                    trace!("Found encoding: {}", encoding.to_owned());

                    vacant_entry.insert(encoding_index);
                    Ok(encoding_index)
                } else {
                    Err(BaseInstrError::NoMatch)
                }
            },
        }
    }

    pub fn lookup_index(
        &mut self, instr: Instruction, segment_sizes: SegmentSizes,
    ) -> Result<(Instruction, usize), BaseInstrError> {
        let encodings = match segment_sizes {
            SegmentSizes::Cs16Ss16 => &self.semantics.maps().c16s16,
            SegmentSizes::Cs16Ss32 => &self.semantics.maps().c16s32,
            SegmentSizes::Cs32Ss16 => &self.semantics.maps().c32s16,
            SegmentSizes::Cs32Ss32 => &self.semantics.maps().c32s32,
        };

        if let LookupResult::Found(&encoding_index) = encodings.get(instr) {
            let encoding = &self.semantics.get(encoding_index);
            let base_instr = encoding.try_extract_base_instr(instr)?;

            trace!("Found encoding: {}", encoding.to_owned());

            Ok((base_instr, encoding_index))
        } else {
            Err(BaseInstrError::NoMatch)
        }
    }

    pub fn lookup_iteratively<E>(
        &mut self, mut next_byte: impl FnMut() -> Result<u8, E>, segment_sizes: SegmentSizes,
    ) -> (
        Result<Option<(usize, EncodingRef<'_, Intel386, MiniSemRef<'_, Intel386>, IgnoredMetadata>)>, E>,
        Instruction,
    ) {
        let (encodings, cache, too_short) = match segment_sizes {
            SegmentSizes::Cs16Ss16 => (
                &self.semantics.maps().c16s16,
                &mut self.cache[0],
                &mut self.cache_too_short[0],
            ),
            SegmentSizes::Cs16Ss32 => (
                &self.semantics.maps().c16s32,
                &mut self.cache[1],
                &mut self.cache_too_short[1],
            ),
            SegmentSizes::Cs32Ss16 => (
                &self.semantics.maps().c32s16,
                &mut self.cache[2],
                &mut self.cache_too_short[2],
            ),
            SegmentSizes::Cs32Ss32 => (
                &self.semantics.maps().c32s32,
                &mut self.cache[3],
                &mut self.cache_too_short[3],
            ),
        };

        let mut buf = [0; 15];
        let mut n = 0;
        let mut min_bytes_needed = 1;

        loop {
            buf[n] = match next_byte() {
                Ok(b) => b,
                Err(e) => return (Err(e), Instruction::new(&buf[..n])),
            };
            n += 1;

            if n >= min_bytes_needed {
                let instr = Instruction::new(&buf[..n]);
                if let Some(extra) = too_short.get(&instr) {
                    min_bytes_needed = n + *extra as usize;
                } else if cache.contains_key(&instr) {
                    let entry = cache[&instr];
                    return (Ok(Some((entry, self.semantics.get(entry)))), instr)
                } else if let LookupResult::Found(&encoding_index) = encodings.get(instr) {
                    let encoding = self.semantics.get(encoding_index);
                    match encoding.try_extract_base_instr(instr) {
                        Ok(_) => (),
                        Err(BaseInstrError::NeedMoreBytes(extra)) => {
                            min_bytes_needed = n + extra;
                            too_short.insert(instr, extra as u8);
                            continue
                        },
                        Err(BaseInstrError::NoMatch) => return (Ok(None), instr),
                        Err(BaseInstrError::TooManyBytes(n)) => unreachable!("instruction {instr:X} has {n} too many bytes"),
                    }

                    trace!("Found encoding: {}", encoding.to_owned());

                    let Entry::Vacant(vacant_entry) = cache.entry(instr) else {
                        unreachable!()
                    };
                    vacant_entry.insert(encoding_index);

                    return (Ok(Some((encoding_index, encoding))), instr)
                } else if n == 15 {
                    return (Ok(None), instr)
                }
            }
        }
    }

    #[inline(always)]
    pub fn get_encoding(&self, encoding_index: usize) -> EncodingRef<'_, Intel386, MiniSemRef<'_, Intel386>, IgnoredMetadata> {
        self.semantics.get(encoding_index)
    }

    #[inline(always)]
    pub fn encodings(&self) -> &L {
        &self.semantics
    }

    #[inline(always)]
    pub fn encodings_arc(&self) -> &Arc<L> {
        &self.semantics
    }
}

impl<L> Decoder<L> {
    pub fn new(semantics: Arc<L>) -> Self {
        Decoder {
            semantics,
            cache: [(); 4].map(|_| Default::default()),
            cache_too_short: [(); 4].map(|_| Default::default()),
        }
    }
}
