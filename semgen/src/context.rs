use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::iter::once;
use std::mem::swap;

use itertools::Itertools;
use liblisa::arch::Arch;
use liblisa::encoding::bitpattern::{Bit, Part};
use liblisa::encoding::dataflows::{AddrSize, AddrTerm, AddrTermSize, AddressComputation, MemoryAccess, MemoryAccesses};
use liblisa::encoding::prefixes::{EquivalentPrefixes, PrefixSequence, SubstitutionSequence};
use liblisa::encoding::{Encoding, IgnoredMetadata, ParLoc, UnsizedParLoc};
use liblisa::state::Size;
use log::trace;
use sem86_core::arch::intel386::{GpReg, Intel386};
use sem86_core::il::part_values::PackingStructure;
use sem86_core::il::{Cmd, Commands, Jump, MiniSem, Val};
use sem86_core::system::Db;

use crate::builder::{Builder, SemSpec};

#[derive(Copy, Clone, Debug)]
pub enum Mode {
    RealOrProtected16,
    Protected32,
}

#[derive(Clone, Debug)]
pub struct Context {
    mode: Mode,
    stack_address_size: Db,
    bits: Vec<Bit>,
    parts: Vec<Part<Intel386>>,
    accesses: Vec<MemoryAccess<Intel386>>,
    is_wide: bool,
    is_sign_extend: bool,
    name: String,
    memory_size_override: Option<usize>,
    segment_override: Option<ParLoc<Intel386>>,
    override_wide_operand_size: bool,
    override_address_size: bool,
    override_rm_sizing_mode: Option<Mode>,
    allow_high_byte: bool,
    stack_adjustment: i64,
    lockable: bool,
    reppable: bool,
    segment_override_used: bool,
    operand_size_override_used: Cell<bool>,
    address_size_override_used: Cell<bool>,
    next_temp_var: usize,
    is_rep: bool,
}

impl Context {
    pub fn new(mode: Mode, stack_address_size: Db) -> Self {
        Self {
            mode,
            bits: Vec::new(),
            parts: Vec::new(),
            accesses: Vec::new(),
            is_wide: false,
            is_sign_extend: false,
            name: String::new(),
            memory_size_override: None,
            segment_override: None,
            override_wide_operand_size: false,
            override_address_size: false,
            override_rm_sizing_mode: None,
            allow_high_byte: true,
            stack_address_size,
            stack_adjustment: 0,
            segment_override_used: false,
            lockable: false,
            reppable: false,
            operand_size_override_used: Cell::new(false),
            address_size_override_used: Cell::new(false),
            next_temp_var: 0,
            is_rep: false,
        }
    }

    pub fn memory_reg_and_addr_size(&self) -> (AddrTermSize, AddrSize) {
        self.address_size_override_used.set(true);
        match (self.mode, self.override_address_size) {
            (Mode::RealOrProtected16, false) | (Mode::Protected32, true) => (AddrTermSize::U16, AddrSize::Addr16),
            (Mode::Protected32, false) | (Mode::RealOrProtected16, true) => (AddrTermSize::U32, AddrSize::Addr32),
        }
    }

    pub fn segment_calculation(&self) -> AddressComputation {
        let (reg_size, addr_size) = self.memory_reg_and_addr_size();

        AddressComputation::from_iter(
            [AddrTerm::single(AddrTermSize::U32, 0, 1), AddrTerm::single(reg_size, 0, 1)].into_iter(),
            0,
        )
        .with_addr_size(addr_size)
    }

    pub fn stack_segment_calculation(&self) -> AddressComputation {
        let (reg_size, addr_size) = match self.stack_address_size {
            Db::Protected16 => (AddrTermSize::U16, AddrSize::Addr16),
            Db::Protected32 => (AddrTermSize::U32, AddrSize::Addr32),
        };

        AddressComputation::from_iter(
            [AddrTerm::single(AddrTermSize::U32, 0, 1), AddrTerm::single(reg_size, 0, 1)].into_iter(),
            0,
        )
        .with_addr_size(addr_size)
    }

    pub fn add_bit(&mut self, bit: Bit) {
        self.bits.push(bit);
    }

    pub fn add_access(&mut self, access: MemoryAccess<Intel386>) -> ParLoc<Intel386> {
        let size = Size::from_bytes(access.size.end as usize);
        let index = self.accesses.len();
        self.accesses.push(access);

        ParLoc {
            loc: UnsizedParLoc::Mem(index),
            size,
        }
    }

    pub fn access_mut(&mut self, mem_index: usize) -> &mut MemoryAccess<Intel386> {
        &mut self.accesses[mem_index]
    }

    pub fn override_mem_size(&mut self, size: usize) {
        self.memory_size_override = Some(size)
    }

    pub fn last_access(&mut self) -> &MemoryAccess<Intel386> {
        self.accesses.last().unwrap()
    }

    pub fn pop_access(&mut self) -> MemoryAccess<Intel386> {
        self.accesses.pop().unwrap()
    }

    pub fn parts(&self) -> &[Part<Intel386>] {
        &self.parts
    }

    pub fn segment_override(&mut self) -> Option<ParLoc<Intel386>> {
        self.segment_override_used = true;
        self.segment_override
    }

    pub fn set_segment_override(&mut self, seg: ParLoc<Intel386>) {
        self.segment_override = Some(seg);
    }

    pub fn is_segment_op(&self) -> bool {
        false
    }

    pub fn set_name(&mut self, name: &'static str) {
        self.name = name.to_string();
    }

    pub fn set_wide(&mut self, is_wide: bool) {
        self.is_wide = is_wide;
    }

    pub fn is_wide_op(&self) -> bool {
        self.is_wide
    }

    pub fn set_allow_high_reg_byte(&mut self, allow: bool) {
        self.allow_high_byte = allow;
    }

    pub fn allow_high_reg_byte(&self) -> bool {
        self.allow_high_byte
    }

    pub fn set_sign_extend(&mut self, is_sign_extend: bool) {
        self.is_sign_extend = is_sign_extend;
    }

    pub fn is_sign_extend(&self) -> bool {
        self.is_sign_extend
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn into_encoding(self, spec: SemSpec<Intel386>) -> Option<Encoding<Intel386, MiniSem<Intel386>, IgnoredMetadata>> {
        assert_eq!(self.bits.len() % 8, 0, "bits must be a multiple of 8 for context {self:#?}");
        assert!(self.bits.len() >= 8, "Too few bits in bitpattern: {self:#?}");

        let equivalent_prefixes = self.compute_prefixes()?;

        let mut commands = Commands::Ops(spec.commands);
        if !spec.manual_memory_accesses {
            commands.wrap_memory_accesses(0..self.accesses.len());
        }

        if !matches!(spec.jump, Jump::Far) {
            let mut written = HashSet::new();
            commands.collect_write_targets(&mut written);
            assert!(
                !written
                    .iter()
                    .any(|&t| t == GpReg::Ip || t == GpReg::Cs || t == GpReg::CsBase || t == GpReg::CsLimit),
                "encoding {} performs special jump without declaring it",
                self.name
            );
        }

        let part_packing = PackingStructure::from_part_sizes(self.parts.iter().map(|p| p.size));
        let encoding = Encoding {
            bits: self.bits.iter().copied().rev().map_into().collect(),
            equivalent_prefixes,
            parts: self.parts,
            semantics: MiniSem {
                name: self.name,
                addresses: MemoryAccesses {
                    memory: self.accesses,
                    use_trap_flag: true,
                },
                commands,
                part_packing,
                jump: spec.jump,
                is_rep: self.is_rep,
            },
            metadata: None,
        };

        Some(encoding)
    }

    fn compute_prefixes(&self) -> Option<EquivalentPrefixes> {
        // Since bits is a multiple of 8, we will have added one too many items in ignored_prefixes.
        let prefixes = [0x26, 0x2E, 0x36, 0x3E, 0x64, 0x65, 0x66, 0x67, 0xF0, 0xF2, 0xF3];

        #[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
        struct State {
            seg: Option<u8>,
            data: bool,
            addr: bool,
            lock: bool,
            rep: Option<u8>,
        }

        impl State {
            fn next(&self, prefix: u8, ctx: &Context) -> Self {
                match prefix {
                    0x26 | 0x2E | 0x36 | 0x3E | 0x64 | 0x65 => State {
                        seg: if ctx.segment_override_used { Some(prefix) } else { None },
                        ..*self
                    },
                    0x66 => State {
                        data: ctx.operand_size_override_used.get(),
                        ..*self
                    },
                    0x67 => State {
                        addr: true,
                        ..*self
                    },
                    0xF0 => State {
                        lock: ctx.is_lockable(),
                        ..*self
                    },
                    0xF2 | 0xF3 => State {
                        rep: if ctx.reppable { Some(prefix) } else { None },
                        ..*self
                    },
                    _ => unreachable!(),
                }
            }

            fn bytes(&self) -> impl Iterator<Item = u8> {
                self.seg
                    .iter()
                    .cloned()
                    .chain(once(0x66).take(self.data as usize))
                    .chain(once(0x67).take(self.addr as usize))
                    .chain(once(0xF0).take(self.lock as usize))
                    .chain(self.rep)
            }
        }

        let (node_list, node_index) = {
            let mut all_nodes = HashSet::new();
            let mut pending_nodes = Vec::new();
            let mut new_nodes = Vec::new();
            new_nodes.push(State::default());
            all_nodes.insert(State::default());
            while !new_nodes.is_empty() {
                swap(&mut new_nodes, &mut pending_nodes);
                for node in pending_nodes.iter() {
                    all_nodes.insert(*node);
                }

                for node in pending_nodes.drain(..) {
                    for prefix in prefixes {
                        let next = node.next(prefix, self);
                        if all_nodes.insert(next) {
                            new_nodes.push(next);
                        }
                    }
                }
            }

            let mut nodes = all_nodes.iter().cloned().collect::<Vec<_>>();
            nodes.sort_by_key(|&state| state != State::default());
            let node_index = nodes
                .iter()
                .cloned()
                .enumerate()
                .map(|(index, state)| (state, index))
                .collect::<HashMap<_, _>>();

            (nodes, node_index)
        };
        let node_index = &node_index;

        let possible_values_per_byte = self
            .bits
            .chunks(8)
            .map(|bits| {
                (0..=0xff)
                    .filter(|byte| {
                        bits.iter().rev().enumerate().all(|(index, &b)| {
                            let cur = (byte >> index) & 1;
                            match b {
                                Bit::Fixed(fixed) => cur == fixed,
                                _ => true,
                            }
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let num_bytes = possible_values_per_byte
            .iter()
            .take_while(|bytes| {
                let is_prefix = bytes.iter().all(|b| prefixes.contains(b));
                let is_not_prefix = bytes.iter().all(|b| !prefixes.contains(b));
                assert!(is_prefix || is_not_prefix);

                is_prefix
            })
            .count();
        let possible_prefixes = {
            let mut list = vec![PrefixSequence::empty()];
            let mut next_list = Vec::new();

            for bytes in possible_values_per_byte[..num_bytes].iter() {
                next_list.clear();
                next_list.extend(bytes.iter().flat_map(|&byte| list.iter().map(move |seq| seq.chain_one(byte))));

                swap(&mut next_list, &mut list);
            }

            list
        };

        assert!(possible_prefixes.iter().all(|seq| seq.len() == num_bytes));

        let prefix_state_map = {
            let mut map = HashMap::new();
            for seq in possible_prefixes.iter() {
                let mut s = State::default();
                for &b in seq.bytes() {
                    s = s.next(b, self);
                }

                map.entry(s).or_insert(seq.clone());
            }

            map
        };

        if !prefix_state_map.iter().all(|(state, seq)| seq.len() == state.bytes().count()) {
            trace!(
                "useless prefixes detected ({num_bytes} bytes)?: {prefix_state_map:#?} in {} -- {:#?}\nreppable={}, lockable={}, segment override used={}\nshortest prefixes: {:X?}",
                self.name,
                self.bits,
                self.reppable,
                self.is_lockable(),
                self.segment_override_used,
                prefix_state_map
                    .iter()
                    .map(|(state, seq)| (state.bytes().collect::<Vec<_>>(), seq))
                    .format(", ")
            );
            return None;
        }

        let nodes = node_list.iter().map(|state| match prefix_state_map.get(state) {
            Some(seq) => SubstitutionSequence::EquivalentTo(seq.clone()),
            None => SubstitutionSequence::NotEquivalent,
        });
        let edges = node_list
            .iter()
            .enumerate()
            .flat_map(|(index, &node)| prefixes.iter().map(move |&b| (index, b, node_index[&node.next(b, self)])));

        Some(EquivalentPrefixes::from_edges(num_bytes, nodes, edges))
    }

    pub fn add_part(&mut self, part: Part<Intel386>) -> usize {
        let index = self.parts.len();
        for _ in 0..part.size {
            self.add_bit(Bit::Part(index as u8));
        }

        self.parts.push(part);

        index
    }

    pub fn op_size(&self) -> usize {
        if self.is_wide_op() {
            self.operand_size_override_used.set(true);
            match (self.mode, self.override_wide_operand_size) {
                (Mode::RealOrProtected16, false) | (Mode::Protected32, true) => 2,
                (Mode::Protected32, false) | (Mode::RealOrProtected16, true) => 4,
            }
        } else {
            1
        }
    }

    pub fn op_size_ext(&self, mode: Mode, wide: bool, operand_size_override: bool) -> usize {
        if wide {
            match (mode, operand_size_override) {
                (Mode::RealOrProtected16, false) | (Mode::Protected32, true) => 2,
                (Mode::Protected32, false) | (Mode::RealOrProtected16, true) => 4,
            }
        } else {
            1
        }
    }

    pub fn addr_size(&self) -> usize {
        match self.memory_reg_and_addr_size().1 {
            AddrSize::Addr16 => 2,
            AddrSize::Addr32 => 4,
            _ => unreachable!(),
        }
    }

    pub fn size(&self) -> Size {
        Size::from_bytes(self.op_size())
    }

    pub fn sp_size(&self) -> Size {
        Size::from_bytes(match self.stack_address_size {
            Db::Protected16 => 2,
            Db::Protected32 => 4,
        })
    }

    pub fn memory_size(&self) -> usize {
        self.memory_size_override.unwrap_or(self.op_size())
    }

    pub fn override_wide_operand_size(&mut self) {
        self.override_wide_operand_size = true;
    }

    pub fn has_wide_operand_size_override(&self) -> bool {
        self.operand_size_override_used.set(true);
        self.override_wide_operand_size
    }

    pub fn override_address_size(&mut self) {
        self.override_address_size = true;
    }

    pub fn maximum_operand_size(&mut self) {
        self.is_wide = true;
        self.override_wide_operand_size = match self.mode {
            Mode::RealOrProtected16 => true,
            Mode::Protected32 => false,
        };
    }

    pub fn override_rm_sizing_mode(&mut self, mode: Mode) {
        self.override_rm_sizing_mode = Some(mode);
    }

    pub fn rm_sizing_mode_override(&self) -> Option<Mode> {
        self.override_rm_sizing_mode
    }

    pub fn append_name(&mut self, s: &str) {
        self.name.push_str(s)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn stack_adjustment(&self) -> i64 {
        self.stack_adjustment
    }

    pub fn set_stack_adjustment(&mut self, val: i64) {
        self.stack_adjustment = val;
    }

    pub fn mark_lockable(&mut self) {
        self.lockable = true;
    }

    pub fn is_lockable(&self) -> bool {
        self.lockable
    }

    pub fn set_reppable(&mut self, enable_rep: bool) {
        self.reppable = enable_rep;
    }

    pub fn fresh_temp_var(&mut self) -> Val<Intel386> {
        let var = Val::Temp(self.next_temp_var);
        self.next_temp_var += 1;
        var
    }

    pub fn num_accesses(&self) -> usize {
        self.accesses.len()
    }

    pub fn set_rep(&mut self, is_rep: bool) {
        self.is_rep = is_rep;
    }
}

pub trait BuildableFromContext {
    type Output;

    fn into(self) -> Self::Output;
}

impl<A: Arch> BuildableFromContext for Vec<Cmd<A>> {
    type Output = SemSpec<A>;

    fn into(self) -> Self::Output {
        SemSpec {
            manual_memory_accesses: false,
            commands: self,
            jump: Jump::Sequential,
        }
    }
}

impl<A: Arch> BuildableFromContext for SemSpec<A> {
    type Output = SemSpec<A>;

    fn into(self) -> Self::Output {
        self
    }
}

pub struct BuildFromContext<T, F: Fn(&mut Context) -> T>(F);

impl<T, F: Fn(&mut Context) -> T> BuildFromContext<T, F> {
    pub fn new(f: F) -> Self {
        Self(f)
    }
}

impl<T: BuildableFromContext, F: Fn(&mut Context) -> T> Builder for BuildFromContext<T, F> {
    type Output = T::Output;

    fn build(&self, mut ctx: Context, next: &mut dyn FnMut(Context, Self::Output)) {
        let result = self.0(&mut ctx);
        next(ctx, result.into())
    }
}
