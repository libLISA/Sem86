use std::collections::{HashMap, HashSet};
use std::fmt::Display;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arrayvec::ArrayVec;
use bitcode::{Decode, Encode};
use crossbeam_channel::{SendError, Sender, bounded};
use itertools::Itertools;
use liblisa::utils::bitmap::GrowingBitmap;
use log::{debug, info, trace, warn};
use mem_dbg::{CopyType, MemSize, SizeFlags, True};
use sem86_arch::addr::PhysFrameIndex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::SegmentSizes;
use crate::codegen::backends::inkwell::{InkwellBackend, InkwellContext, InkwellFunction};
use crate::codegen::backends::{BackendFn, JitExecutionResult, LirBlock, NextInstr, NextOnPage};
use crate::codegen::components::StronglyConnectedComponents;
use crate::codegen::lir::MirToLir;
use crate::codegen::mir::{EncodingEntry, MirBuilder};
use crate::codegen::mm::Object;
use crate::codegen::mm::bump::BumpCodeAlloc;
use crate::codegen::page::roots::Roots;
use crate::decoder::{EncodingLookup, PackedInstrSem};
use crate::emulator::Emulator;
use crate::icache::entry::CacheEntryId;
use crate::il::part_values::PartValues;
use crate::il::{MakeEncoding, NextIp};
use crate::util::{DebugInstrs, DisplayByteSize};

mod roots;

#[derive(Copy, Clone)]
pub struct PageFunction {
    function: InkwellFunction,
}

#[derive(Debug)]
pub struct PageExitToken(u64);

impl PageExitToken {
    pub fn new_unchecked(val: u64) -> Self {
        Self(val)
    }
}

#[derive(Clone)]
pub struct PageCode<'tag> {
    ids: Arc<[CacheEntryId<'tag>]>,
    function: PageFunction,
}

impl<'tag> PageCode<'tag> {
    pub fn from_result(
        result: &PageJitResult, ids: &[CacheEntryId<'tag>], bump: &mut BumpCodeAlloc,
    ) -> impl Iterator<Item = (String, Self)> {
        let ids = Arc::<[_]>::from(ids.to_vec());
        assert_eq!(ids.len(), result.instrs.len());
        trace!("IDs for page: {ids:?}");
        bump.alloc(&result.object)
            .sorted_by_key(|(name, _)| name.clone())
            .map(move |(name, ptr)| {
                (
                    name,
                    Self {
                        ids: ids.clone(),
                        function: PageFunction {
                            // TODO: Should we somehow verify symbol name corresponds with correct function?
                            function: unsafe { InkwellFunction::from_ptr(ptr) },
                        },
                    },
                )
            })
    }

    #[inline(always)]
    pub fn function(&self) -> PageFunction {
        self.function
    }

    #[inline(always)]
    pub fn resolve_exit_token(&self, token: PageExitToken) -> CacheEntryId<'tag> {
        self.ids[token.0 as usize]
    }

    pub fn ids(&self) -> &[CacheEntryId<'tag>] {
        &self.ids
    }
}

impl PageFunction {
    pub fn dispatch(&self, emulator: &mut Emulator) -> (JitExecutionResult, PageExitToken) {
        let (result, last_executed) = self.function.execute(emulator, |_| ());
        (result, PageExitToken(last_executed))
    }

    pub fn as_fptr(&self) -> fn(&mut Emulator) -> u64 {
        self.function.as_fptr()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash, Encode, Decode)]
pub struct PageInstr {
    pub is_entry: bool,
    pub offset: u16,
    pub encoding_index: u32,
    pub part_values: PartValues,
    pub instr_len: u8,
    pub protected_mode: bool,
    pub segment_sizes: SegmentSizes,

    /// Offsets within this page where the next instruction might be.
    /// These can be speculative; the offset will be double-checked before jumping to it.
    pub next: ArrayVec<u16, 2>,
}

impl MemSize for PageInstr {
    fn mem_size(&self, _flags: mem_dbg::SizeFlags) -> usize {
        size_of::<Self>()
    }
}

impl CopyType for PageInstr {
    type Copy = True;
}

pub struct PageJitRequest {
    pub phys_frame_index: PhysFrameIndex,
    pub data: PageJitRequestData,
}

#[derive(Clone, Debug, Serialize, Deserialize, MemSize)]
pub struct PageJitRequestData {
    pub instrs: Vec<PageInstr>,
}

pub struct PageJit {
    sender: Sender<PageJitRequest>,
    num_pending: Arc<AtomicUsize>,
    result_receiver: crossbeam_channel::Receiver<PageJitResult>,
}

pub struct PageJitResult {
    pub phys_frame_index: PhysFrameIndex,
    pub instrs: Vec<PageInstr>,
    pub object: Object,
}

#[derive(Clone, MemSize)]
struct ChainCacheEntry {
    function: Object,
    instrs: Vec<PageInstr>,
}

#[derive(Clone, Debug)]
struct PageInstrWithEdges {
    preds: Vec<usize>,
    succs: Vec<usize>,
    page_instr: PageInstr,
    make_split_point: bool,
    block_index: Option<usize>,
    may_jump_to_out_of_page_instr: bool,
    may_jump_to_unknown_offset: bool,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, MemSize)]
struct HashKey([u8; 32]);

impl Display for HashKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:02x}", self.0.iter().format(""))
    }
}

const MAX_REQUESTS: usize = 4096;

// TODO: Run this in a separate process, so we do not take down the entire emulator when codegen OOMs
impl PageJit {
    pub fn new(semantics: Arc<PackedInstrSem>) -> Self {
        let (sender, receiver) = bounded::<PageJitRequest>(MAX_REQUESTS);
        let (result_sender, result_receiver) = bounded::<PageJitResult>(MAX_REQUESTS);
        let cache = Arc::new(Mutex::new(HashMap::<HashKey, ChainCacheEntry>::new()));
        let num_pending = Arc::new(AtomicUsize::new(0));

        let allowed_hashes = if std::fs::exists("allowed-page-jit-hashes.txt").unwrap() {
            Some(
                std::fs::read_to_string("allowed-page-jit-hashes.txt")
                    .unwrap()
                    .lines()
                    .map(|line| HashKey(hex::decode(line.trim()).unwrap().try_into().unwrap()))
                    .collect::<HashSet<_>>(),
            )
        } else {
            None
        };

        for n in 0..3 {
            let num_pending = num_pending.clone();
            let receiver = receiver.clone();
            let cache = cache.clone();
            let semantics = semantics.clone();
            let result_sender = result_sender.clone();
            let allowed_hashes = allowed_hashes.clone();

            std::thread::Builder::new()
                .name(format!("chain-jit-compiler-{n}"))
                .spawn(move || while let Ok(mut request) = receiver.recv() {
                    let num_pending = num_pending.fetch_sub(1, Ordering::SeqCst);

                    let hash = HashKey(Sha256::digest(pot::to_vec(&request.data).unwrap()).into());
                    {
                        let cache = cache.lock().unwrap();
                        let cache_size = cache.values().map(|v| v.function.size()).sum::<usize>();
                        assert!(cache_size <= 1 << 30, "page JIT cache should be less than a gigabyte in size");
                        if let Some(entry) = cache.get(&hash) {
                            result_sender.send(PageJitResult {
                                instrs: entry.instrs.clone(),
                                phys_frame_index: request.phys_frame_index,
                                object: entry.function.clone(),
                            }).unwrap();
                            continue
                        }
                    }

                    if let Some(h) = &allowed_hashes && !h.contains(&hash) {
                        continue
                    }

                    let start = Instant::now();

                    let instrs = request.data.instrs.iter().map(|item| {
                        let e = semantics.get(item.encoding_index as usize).make_encoding();
                        (item.offset as u32, e.part_values_to_instr(&item.part_values.unpack(&e.semantics.part_packing).collect::<Vec<_>>()))
                    }).collect::<Vec<_>>();
                    info!("Compiling page: {:?}", DebugInstrs(&instrs));

                    // Identify all predecessors and successors
                    // TODO: Insert an entry point somewhere in each loop
                    let mut edges = request.data.instrs.iter()
                        .map(|instr| PageInstrWithEdges {
                            make_split_point: instr.is_entry,
                            preds: Vec::new(),
                            succs: Vec::new(),
                            page_instr: instr.clone(),
                            block_index: None,
                            may_jump_to_out_of_page_instr: false,
                            may_jump_to_unknown_offset: false,
                        }).collect::<Vec<_>>();
                    for index in 0..edges.len() {
                        let item = &edges[index];
                        let encoding = semantics.get(item.page_instr.encoding_index as usize);
                        let next_ips = encoding.semantics.jump.try_determine_next_ips(item.page_instr.part_values, encoding.semantics.part_packing, item.page_instr.instr_len as usize, item.page_instr.segment_sizes.is_cs32());

                        let mut next_offsets = Vec::new();
                        let mut all_offsets_known = true;
                        let mut all_offsets_reachable = true;
                        if let Some(next_ips) = next_ips {
                            for next in next_ips.iter() {
                                if let NextIp::Relative(offset) = next {
                                    let next_offset = (item.page_instr.offset as i128).wrapping_add(*offset);
                                    if (0..4096).contains(&next_offset) {
                                        next_offsets.push(next_offset as u16);
                                    } else {
                                        all_offsets_reachable = false;
                                    }
                                } else {
                                    all_offsets_known = false;
                                    all_offsets_reachable = false;
                                }
                            }
                        } else {
                            all_offsets_known = false;
                            all_offsets_reachable = false;
                        }

                        if !all_offsets_known {
                            next_offsets.extend(item.page_instr.next.iter().copied());
                        }

                        // Handlers always exit to the emulator loop, so they should always be the last instruction in a chain.
                        let invokes_handler = encoding.semantics.commands.invokes_handler();
                        for &offset in next_offsets.iter() {
                            let Some(next_index) = edges.iter().position(|next| next.page_instr.offset == offset) else {
                                all_offsets_reachable = false;
                                continue
                            };

                            if invokes_handler {
                                edges[next_index].make_split_point = true;
                            } else {
                                if !edges[next_index].preds.contains(&index) {
                                    edges[next_index].preds.push(index);
                                }

                                edges[index].succs.push(next_index);
                            }
                        }

                        edges[index].may_jump_to_out_of_page_instr = !all_offsets_reachable;
                        edges[index].may_jump_to_unknown_offset = !all_offsets_known;

                        // TODO: Do not automatically make every branch a split point. Instead we should try to extract self-contained groups of chains, and compile these to a single LIR with multiple exit points. 
                        if edges[index].succs.len() > 1 || !all_offsets_reachable {
                            for index in edges[index].succs.clone().into_iter() {
                                edges[index].make_split_point = true;
                            }
                        }
                    }

                    let roots = Roots::of(&edges);

                    // Identify blocks
                    let mut blocks = Vec::new();
                    let mut pending = Vec::new();
                    let mut block_index = 0;
                    for (index, instr) in edges.iter_mut().enumerate() {
                        let encoding = semantics.get(instr.page_instr.encoding_index as usize);
                        let next_ips = encoding.semantics.jump.try_determine_next_ips(instr.page_instr.part_values, encoding.semantics.part_packing, instr.page_instr.instr_len as usize, instr.page_instr.segment_sizes.is_cs32());

                        trace!("Instruction #{index} (root={:?}): {:X} preds={:?}, succs={:?}, may_jump_to_out_of_page_instr={}, may_jump_to_unknown_offset={} (next_ips={next_ips:X?})", roots[index], encoding.instr(), instr.preds, instr.succs, instr.may_jump_to_out_of_page_instr, instr.may_jump_to_unknown_offset);
                        if instr.make_split_point || instr.preds.len() > 1 {
                            trace!("Making instruction #{index} an entry: {} predecessors", instr.preds.len());
                            instr.block_index = Some(block_index);
                            block_index += 1;
                            pending.push(index);
                        }
                    }

                    for index in pending {
                        let first_instr = &edges[index];
                        trace!("Processing entry (instruction #{index}) at 0x{:03X}", first_instr.page_instr.offset);
                        let (entries, last_instr) = {
                            let mut cur = first_instr;
                            let mut cur_index = index;
                            let mut is_first = true;
                            let mut last_instr = cur;
                            let mut instrs = Vec::new();
                            loop {
                                if cur.make_split_point && !is_first {
                                    break
                                }

                                trace!("Now processing instruction #{cur_index}");
                                is_first = false;
                                last_instr = cur;
                                instrs.push(EncodingEntry {
                                    instr: None,
                                    instr_len: cur.page_instr.instr_len as usize,
                                    encoding: semantics.get(cur.page_instr.encoding_index as usize),
                                    part_values: cur.page_instr.part_values,
                                    metadata: Some(cur_index as u64),
                                    is_cs32: cur.page_instr.segment_sizes.is_cs32(),
                                });

                                let encoding = semantics.get(cur.page_instr.encoding_index as usize);
                                let next_ips = encoding.semantics.jump.try_determine_next_ips(cur.page_instr.part_values, encoding.semantics.part_packing, cur.page_instr.instr_len as usize, cur.page_instr.segment_sizes.is_cs32());

                                // `succs` can contain one successor even for instructions that in reality have multiple successors.
                                // For example, the successor might be on another page -- or might never be certain, for example, in the case of an indirect call.
                                // We must make sure the successor is guaranteed to be the only possible next instruction.
                                // To do this, we inspect the next IPs from the jump.
                                if let Some(next_ips) = next_ips
                                    && next_ips.len() == 1
                                    && let NextIp::Relative(offset) = next_ips[0]
                                    && cur.succs.len() == 1 {
                                    let expected_next_offset = cur.page_instr.offset as i128 + offset;
                                    assert_eq!(edges[cur.succs[0]].page_instr.offset as i128, expected_next_offset);
                                    cur_index = cur.succs[0];
                                    cur = &edges[cur_index];
                                } else {
                                    break
                                }
                            }

                            (instrs, last_instr)
                        };

                        let next = {
                            let encoding = semantics.get(last_instr.page_instr.encoding_index as usize);
                            let next_ips = encoding.semantics.jump.try_determine_next_ips(last_instr.page_instr.part_values, encoding.semantics.part_packing, last_instr.page_instr.instr_len as usize, last_instr.page_instr.segment_sizes.is_cs32());
                            trace!("next_ips = {next_ips:X?}");
                            if encoding.semantics.commands.invokes_handler() {
                                // We need to break chains when we encounter instructions that invoke handlers
                                NextOnPage::Speculative(ArrayVec::new())
                            } else if let Some(next_ips) = next_ips
                                && next_ips.len() == 1
                                && let NextIp::Relative(offset) = next_ips[0] {
                                let next_offset = (last_instr.page_instr.offset as i128).wrapping_add(offset) as u128;
                                if next_offset < 4096 && let Some(index) = edges.iter().position(|x| x.page_instr.offset == next_offset as u16) {
                                    NextOnPage::Certain(NextInstr {
                                        offset: edges[index].page_instr.offset,
                                        block_index: if let Some(index) = edges[index].block_index {
                                            index
                                        } else {
                                            panic!("Instruction #{index} should have been made an entry, but block index is missing")
                                        },
                                    })
                                } else {
                                    NextOnPage::Speculative(ArrayVec::new())
                                }
                            } else if let Some((next_if_zero, next_if_nonzero)) = encoding.semantics.jump.try_derive_ips_from_condition(last_instr.page_instr.part_values, encoding.semantics.part_packing, last_instr.page_instr.instr_len as usize, last_instr.page_instr.segment_sizes.is_cs32())
                                && let NextIp::Relative(offset_if_zero) = next_if_zero
                                && let NextIp::Relative(offset_if_nonzero) = next_if_nonzero {
                                let next_offset_if_zero = (last_instr.page_instr.offset as i128).wrapping_add(offset_if_zero) as u128;
                                let next_if_zero = edges.iter().position(|x| x.page_instr.offset as u128 == next_offset_if_zero).map(|index| NextInstr {
                                    offset: edges[index].page_instr.offset,
                                    block_index: if let Some(index) = edges[index].block_index {
                                        index
                                    } else {
                                        panic!("Instruction #{index} should have been made an entry, but block index is missing")
                                    },
                                });

                                let next_offset_if_nonzero = (last_instr.page_instr.offset as i128).wrapping_add(offset_if_nonzero) as u128;
                                let next_if_nonzero = edges.iter().position(|x| x.page_instr.offset as u128 == next_offset_if_nonzero).map(|index| NextInstr {
                                    offset: edges[index].page_instr.offset,
                                    block_index: if let Some(index) = edges[index].block_index {
                                        index
                                    } else {
                                        panic!("Instruction #{index} should have been made an entry, but block index is missing")
                                    },
                                });

                                if next_if_zero.is_some() || next_if_nonzero.is_some() {
                                    NextOnPage::FromCondition {
                                        condition_nonzero: next_if_nonzero,
                                        condition_zero: next_if_zero,
                                    }
                                } else {
                                    NextOnPage::Speculative(ArrayVec::new())
                                }
                            } else {
                                NextOnPage::Speculative(last_instr.succs.iter().map(|&index| NextInstr {
                                    offset: edges[index].page_instr.offset,
                                    block_index: if let Some(index) = edges[index].block_index {
                                        index
                                    } else {
                                        panic!("Instruction #{index} should have been made an entry, but block index is missing")
                                    },
                                }).collect())
                            }
                        };

                        debug!("Compiling chain at 0x{:X}: {:?} next={next:X?}", first_instr.page_instr.offset, entries.iter().map(|item| {
                            let e = item.encoding.make_encoding();
                            e.part_values_to_instr(&item.part_values.unpack(&e.semantics.part_packing).collect::<Vec<_>>())
                        }).format(" "));
                        let mir = MirBuilder::build_from_sequence(first_instr.page_instr.protected_mode, &entries);
                        trace!("MIR: {mir}");

                        let mir_time = start.elapsed().as_millis();
                        debug!("MIR built in {mir_time}ms");

                        let lir_start = Instant::now();
                        let lir = MirToLir::new(&mir).build();

                        let lir_time = lir_start.elapsed().as_millis();
                        debug!("LIR built in {mir_time}ms");

                        let instrs = entries.iter().map(|item| {
                            let e = item.encoding.make_encoding();
                            e.part_values_to_instr(&item.part_values.unpack(&e.semantics.part_packing).collect::<Vec<_>>())
                        }).collect::<Vec<_>>();
                        if start.elapsed() >= Duration::from_millis(100) {
                            warn!("Compilation of chain {:#X?} took {}ms -- {mir_time}ms in MIR, {lir_time}ms in LIR", instrs, start.elapsed().as_millis());
                        }

                        trace!("Created block with id {index} at offset 0x{:X}, containing {} instrs: {:X} -- next={next:X?}", first_instr.page_instr.offset, instrs.len(), instrs.iter().format(" "));
                        assert_eq!(first_instr.block_index.unwrap(), blocks.len());
                        blocks.push(LirBlock {
                            id: index as u32,
                            export: first_instr.page_instr.is_entry,
                            offset: first_instr.page_instr.offset,
                            // TODO: Would it improve code generation if we performed these checks at the start of a function instead of at the end? (so there is no extra state to save, and instead we can just return immediately)
                            check_intr: lir.performs_io() && !next.is_empty(),
                            lir,
                            next: {
                                let mut m = HashMap::new();
                                m.insert(entries.last().unwrap().metadata.unwrap(), next);
                                m
                            },
                        });
                    }

                    Self::break_loops(&mut blocks);

                    for block in blocks.iter_mut() {
                        if block.export {
                            request.data.instrs.iter_mut()
                                .find(|instr| instr.offset == block.offset)
                                .unwrap()
                                .is_entry = true;
                        }
                    }

                    // TODO: We could merge blocks that aren't entries into branching blocks.

                    debug!("JITing in backend...");
                    // Compile each block as a function with tailcalls to the next block.
                    let jit_start = Instant::now();
                    let compiled_function = {
                        // thread_local! {
                        //     static COMPILER: RefCell<InkwellBackend<'static>> = RefCell::new(InkwellBackend::new(Box::leak(Box::new(InkwellContext::new()))));
                        // }

                        let context = InkwellContext::new();
                        let mut c = InkwellBackend::new(&context);
                        c.codegen_page(&blocks).unwrap()

                        // COMPILER.with(|c|
                            // c.borrow_mut().codegen_page(&blocks)
                        // ).unwrap()
                    };

                    let jit_time = jit_start.elapsed().as_millis();
                    if start.elapsed() >= Duration::from_millis(250) {
                        warn!("Compilation of page took {}ms -- {jit_time}ms in JIT", start.elapsed().as_millis());
                    }
                    debug!("JITed in backend in {jit_time}ms");

                    match result_sender.send(PageJitResult {
                        instrs: request.data.instrs.clone(),
                        phys_frame_index: request.phys_frame_index,
                        object: compiled_function.clone(),
                    }) {
                        Ok(_) => (),
                        Err(SendError(_)) => return,
                    }

                    {
                        let mut cache = cache.lock().unwrap();
                        cache.insert(hash, ChainCacheEntry {
                            function: compiled_function,
                            instrs: request.data.instrs,
                        });
                    }

                    debug!("JITed page@ phys={}, hash={hash} ({num_pending} more pending): {:?}", request.phys_frame_index, DebugInstrs(&instrs));
                    debug!("Cache size: {}", DisplayByteSize((*cache.lock().unwrap()).mem_size(SizeFlags::FOLLOW_REFS | SizeFlags::CAPACITY)))
                }).unwrap();
        }

        Self {
            sender,
            result_receiver,
            num_pending,
        }
    }

    pub fn request_compilation(&self, req: PageJitRequest) {
        self.num_pending.fetch_add(1, Ordering::SeqCst);
        self.sender
            .try_send(req)
            .expect("LIR compiler thread should be active and number of requests should be less than MAX_REQUESTS")
    }

    pub fn num_pending_requests(&self) -> usize {
        self.num_pending.load(Ordering::SeqCst)
    }

    pub fn recv(&self) -> Option<PageJitResult> {
        self.result_receiver.try_recv().ok()
    }

    pub fn recv_blocking(&self) -> PageJitResult {
        self.result_receiver.recv().unwrap()
    }

    /// Ensures that every loop:
    ///
    /// - Checks INTR on every iteration.
    /// - Generates an entry point somewhere within the loop in case the loop is resumed after an interrupt.
    fn break_loops(blocks: &mut [LirBlock]) {
        fn dfs(
            blocks: &mut [LirBlock], blocks_copy: &[LirBlock], current: usize, seen: &mut GrowingBitmap, path: &mut Vec<usize>,
        ) {
            let block = &blocks_copy[current];
            for choice in block.next.values().flat_map(|next| next.iter()) {
                if seen[choice.block_index] {
                    // We only need an INTR check if there isn't one in this loop.
                    if !path
                        .iter()
                        .rev()
                        .take_while(|&&index| index != current)
                        .any(|&n| blocks[n].check_intr)
                    {
                        blocks[choice.block_index].check_intr = true;
                    }

                    // We only need to export if no block has been exoprted in this loop.
                    if !path
                        .iter()
                        .rev()
                        .take_while(|&&index| index != current)
                        .any(|&n| blocks[n].export)
                    {
                        blocks[choice.block_index].export = true;
                    }
                } else if !blocks[choice.block_index].check_intr || !blocks[choice.block_index].export {
                    path.push(choice.block_index);
                    seen.set(choice.block_index);
                    dfs(blocks, blocks_copy, choice.block_index, seen, path);
                    seen.reset(choice.block_index);
                    assert_eq!(path.pop(), Some(choice.block_index));
                }
            }
        }

        let blocks_copy = blocks.to_vec();

        let mut seen = GrowingBitmap::new_all_ones(blocks.len());
        let mut path = Vec::new();
        StronglyConnectedComponents::iterate_with_roots(
            &&blocks_copy[..],
            blocks_copy.iter().enumerate().filter(|(_, b)| b.export).map(|(n, _)| n),
            |group| {
                for &index in group {
                    seen.reset(index);
                }

                for &index in group {
                    if blocks[index].export {
                        seen.set(index);
                        path.push(index);
                        dfs(blocks, &blocks_copy, index, &mut seen, &mut path);
                        seen.reset(index);
                        assert_eq!(path.pop(), Some(index));
                    }
                }

                for &index in group {
                    seen.set(index);
                }
            },
        );
    }
}
