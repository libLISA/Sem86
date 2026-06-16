use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::scope;

use arrayvec::ArrayVec;
use clap::Parser;
use itertools::Itertools;
use liblisa::Instruction;
use liblisa::encoding::{Encoding, IgnoredMetadata, InstructionSpace};
use liblisa::instr::{InstructionMap, LookupResult};
use log::info;
use lz4_flex::frame::FrameDecoder;
use rayon::prelude::*;
use sem86_arch::addr::PhysFrameIndex;
use sem86_core::SegmentSizes;
use sem86_core::arch::intel386::Intel386;
use sem86_core::codegen::backends::Backend;
use sem86_core::codegen::backends::inkwell::{InkwellBackend, InkwellContext};
use sem86_core::codegen::lir::MirToLir;
use sem86_core::codegen::mir::{EncodingEntry, MirBuilder};
use sem86_core::codegen::mm::Object;
use sem86_core::codegen::page::{PageInstr, PageJit, PageJitRequest, PageJitRequestData};
use sem86_core::decoder::{Decoder, EncodingLookup, InstrMaps, InstrSem, PackedInstrSem};
use sem86_core::il::{BorrowEncoding, MakeEncoding, MiniSem};
use sem86_core::system::Db;
use semgen::context::Mode;
use semgen::{Config, into_encodings};
use xmas_elf::sections::ShType;

pub fn hex(s: &str) -> Result<u16, String> {
    u16::from_str_radix(s, 16).map_err(|s| s.to_string())
}

#[derive(Clone, Debug, Parser)]
pub enum Args {
    Generate {
        output: PathBuf,

        #[clap(flatten)]
        config: Config,
    },
    Lookup {
        semantics: PathBuf,
        instr: Instruction,
    },
    Compile {
        semantics: PathBuf,

        instr: Instruction,

        #[clap(long, value_parser = clap::value_parser!(SegmentSizes))]
        segment_sizes: SegmentSizes,

        #[clap(long)]
        instantiate: bool,

        #[clap(long)]
        protected_mode_memory_accesses: bool,
    },
    CompileChain {
        semantics: PathBuf,

        #[clap(long, num_args(1..))]
        instrs: Vec<Instruction>,

        #[clap(long, value_parser = clap::value_parser!(SegmentSizes))]
        segment_sizes: SegmentSizes,

        #[clap(long)]
        protected_mode_memory_accesses: bool,
    },
    CompilePage {
        semantics: PathBuf,

        #[clap(long, num_args(1..))]
        instrs: Vec<Instruction>,

        #[clap(long, value_parser = clap::value_parser!(SegmentSizes))]
        segment_sizes: SegmentSizes,

        #[clap(long)]
        protected_mode_memory_accesses: bool,

        #[clap(long, value_parser = hex)]
        entry: Vec<u16>,
    },
    Dump {
        semantics: PathBuf,

        #[clap(long)]
        index: Option<usize>,

        #[clap(long)]
        name: Option<String>,
    },
    Stats {
        semantics: PathBuf,
    },
    ShowDifferences {
        semantics: PathBuf,
    },
    CountParts {
        semantics: PathBuf,
    },
}

pub fn build_encoding_map(encodings: &[Encoding<Intel386, MiniSem<Intel386>, IgnoredMetadata>]) -> InstructionMap<usize> {
    info!("Building map...");
    let map = encodings
        .par_iter()
        .enumerate()
        .map(|(index, encoding)| {
            let graph = encoding.as_graph();
            graph.map_typechange(|_| index)
        })
        .fold(InstructionMap::new, |mut a, b| {
            a.union_with(&b);
            a
        })
        .reduce(InstructionMap::new, |mut a, b| {
            a.union_with(&b);
            a
        });

    info!("Built a lookup graph with {} nodes", map.num_nodes());
    map
}

fn print_memory_statistics(loc: &str) {
    println!("Memory usage {loc}:");
    println!("{}", std::fs::read_to_string("/proc/self/smaps_rollup").unwrap());
}

fn main() {
    env_logger::init();

    let args = Args::parse();

    match args {
        Args::Lookup {
            semantics,
            instr,
        } => {
            let f = BufReader::new(File::open(&semantics).unwrap());
            let f = FrameDecoder::new(f);
            let instr_semantics: PackedInstrSem = pot::from_reader(f).unwrap();
            let instr_semantics = Arc::new(instr_semantics);
            let mut decoder = Decoder::new(instr_semantics.clone());

            for segment_sizes in [
                SegmentSizes::Cs16Ss16,
                SegmentSizes::Cs16Ss32,
                SegmentSizes::Cs32Ss16,
                SegmentSizes::Cs32Ss32,
            ] {
                match decoder.lookup(instr, segment_sizes) {
                    Ok(index) => {
                        let e = instr_semantics.get(index);
                        let owned = e.make_encoding();
                        println!("Checking (segment sizes={segment_sizes:?}) = encoding #{index}");
                        println!("{owned}");

                        let part_values = e.semantics.part_packing.pack(&e.extract_parts(&instr));
                        let next_ips = e.semantics.jump.try_determine_next_ips(
                            part_values,
                            e.semantics.part_packing,
                            instr.byte_len(),
                            segment_sizes.is_cs32(),
                        );
                        println!("Expected next IPs: {next_ips:X?}")
                    },
                    Err(e) => println!("{segment_sizes:?}: {e:?}"),
                }
            }
        },
        Args::Compile {
            semantics,
            instr,
            segment_sizes,
            instantiate,
            protected_mode_memory_accesses,
        } => {
            let f = BufReader::new(File::open(&semantics).unwrap());
            let f = FrameDecoder::new(f);
            let instr_semantics: PackedInstrSem = pot::from_reader(f).unwrap();
            let instr_semantics = Arc::new(instr_semantics.unpack());
            let mut decoder = Decoder::new(instr_semantics.clone());

            match decoder.lookup(instr, segment_sizes) {
                Ok(index) => {
                    let encoding = &instr_semantics.encodings[index];

                    println!("Compiling {encoding}");
                    let mir = if instantiate {
                        let part_values = encoding.semantics.part_packing.pack(&encoding.extract_parts(&instr));
                        MirBuilder::build_from_sequence(
                            protected_mode_memory_accesses,
                            &[EncodingEntry {
                                instr: Some(instr),
                                instr_len: instr.byte_len(),
                                encoding: encoding.borrow_encoding(),
                                part_values,
                                metadata: None,
                                is_cs32: segment_sizes.is_cs32(),
                            }],
                        )
                    } else {
                        MirBuilder::build_from_uninstantiated_encoding(
                            encoding.borrow_encoding(),
                            protected_mode_memory_accesses,
                            segment_sizes.is_cs32(),
                        )
                    };

                    println!("MIR:\n{mir}");
                    let lir = MirToLir::new(&mir).build();

                    println!("LIR:\n{lir:#?}");

                    let ctx = InkwellContext::new();
                    let mut inkwell = InkwellBackend::new(&ctx);
                    inkwell.codegen_lir_object(&lir).unwrap();

                    let (ir, asm) = inkwell.codegen_ir_and_asm(&lir);

                    println!("Inkwell IR:\n{ir}");
                    println!("Inkwell assembly:\n{asm}");
                    print_elf_info(inkwell.codegen_object(&lir));
                },
                Err(e) => println!("Decoding failed: {e:?}"),
            }
        },
        Args::CompileChain {
            semantics,
            segment_sizes,
            instrs,
            protected_mode_memory_accesses,
        } => {
            let f = BufReader::new(File::open(&semantics).unwrap());
            let f = FrameDecoder::new(f);
            let instr_semantics: PackedInstrSem = pot::from_reader(f).unwrap();
            let instr_semantics = Arc::new(instr_semantics);
            let mut decoder = Decoder::new(instr_semantics.clone());

            let v = instrs
                .iter()
                .enumerate()
                .map(|(index, &instr)| {
                    let encoding = instr_semantics.get(decoder.lookup(instr, segment_sizes).unwrap());
                    EncodingEntry {
                        instr: Some(instr),
                        instr_len: instr.byte_len(),
                        encoding,
                        part_values: encoding.semantics.part_packing.pack(&encoding.extract_parts(&instr)),
                        metadata: Some(index as u64),
                        is_cs32: segment_sizes.is_cs32(),
                    }
                })
                .collect::<Vec<_>>();

            print_memory_statistics("before MIR");

            let mir = MirBuilder::build_from_sequence(protected_mode_memory_accesses, &v);

            println!("MIR (debug):\n{mir:?}");
            println!("MIR:\n{mir}");
            print_memory_statistics("before LIR");
            let lir = MirToLir::new(&mir).build();
            drop(mir);

            println!("LIR:\n{lir:#?}");

            print_memory_statistics("before inkwell compilation");
            let ctx = InkwellContext::new();
            let mut inkwell = InkwellBackend::new(&ctx);
            let (ir, asm) = inkwell.codegen_ir_and_asm(&lir);

            println!("Inkwell IR:\n{ir}");
            println!("Inkwell assembly:\n{asm}");

            print_elf_info(inkwell.codegen_object(&lir));

            print_memory_statistics("after compilation");
            print_memory_statistics("after dropping inkwell context");
            // let alloc_size = (f1.as_fptr() as *const u8 as usize).abs_diff(f2.as_fptr() as *const u8 as usize);
            // println!("Distance between two functions: 0x{alloc_size:X}");
            // let min = (f1.as_fptr() as *const u8).min(f2.as_fptr() as *const u8);
            // let data = unsafe { std::slice::from_raw_parts(min, alloc_size) };
            // println!("Memory data: {:02X}", data.iter().format(""));
            // println!("{}", std::fs::read_to_string("/proc/self/maps").unwrap());
        },
        Args::CompilePage {
            semantics,
            segment_sizes,
            instrs,
            protected_mode_memory_accesses,
            entry,
        } => {
            let f = BufReader::new(File::open(&semantics).unwrap());
            let f = FrameDecoder::new(f);
            let instr_semantics: PackedInstrSem = pot::from_reader(f).unwrap();
            let instr_semantics = Arc::new(instr_semantics);
            let mut decoder = Decoder::new(instr_semantics.clone());

            let compiler = PageJit::new(instr_semantics.clone());
            let mut offset = 0u16;
            compiler.request_compilation(PageJitRequest {
                phys_frame_index: PhysFrameIndex::new(0),
                data: PageJitRequestData {
                    instrs: instrs
                        .iter()
                        .map(|&instr| {
                            let encoding_index = decoder.lookup(instr, segment_sizes).unwrap();
                            let encoding = instr_semantics.get(encoding_index);
                            let part_values = encoding.extract_parts(&instr);
                            let part_values = encoding.semantics.part_packing.pack(&part_values);

                            PageInstr {
                                is_entry: entry.contains(&offset),
                                offset: {
                                    let result = offset;
                                    offset += instr.byte_len() as u16;
                                    result
                                },
                                encoding_index: encoding_index as u32,
                                part_values,
                                instr_len: instr.byte_len() as u8,
                                protected_mode: protected_mode_memory_accesses,
                                segment_sizes,
                                next: ArrayVec::new(),
                            }
                        })
                        .collect::<Vec<_>>(),
                },
            });

            let obj = compiler.recv_blocking();
            print_elf_info(obj.object);
        },
        Args::Dump {
            semantics,
            index,
            name,
        } => {
            let f = BufReader::new(File::open(&semantics).unwrap());
            let f = FrameDecoder::new(f);
            let instr_semantics: PackedInstrSem = pot::from_reader(f).unwrap();
            let instr_semantics = instr_semantics.unpack();

            if let Some(index) = index {
                let e = &instr_semantics.encodings[index];
                println!("{e}");
            } else {
                for e in instr_semantics.encodings.iter() {
                    if let Some(name) = &name
                        && e.semantics.name != *name
                    {
                        continue
                    }

                    println!("{e}");
                }
            }
        },
        Args::ShowDifferences {
            semantics,
        } => {
            let f = BufReader::new(File::open(&semantics).unwrap());
            let f = FrameDecoder::new(f);
            let instr_semantics: PackedInstrSem = pot::from_reader(f).unwrap();
            let instr_semantics = instr_semantics.unpack();
            let mut rng = rand::rng();

            for &encoding_index in instr_semantics.maps.c32s32.values() {
                let encoding = &instr_semantics.encodings[encoding_index];
                let instr = encoding
                    .random_instrs(&vec![None; encoding.parts.len()], &mut rng)
                    .next()
                    .unwrap();
                if let LookupResult::Found(&other_index) = instr_semantics.maps.c32s16.get(instr)
                    && other_index != encoding_index
                {
                    let other_encoding = &instr_semantics.encodings[other_index];
                    println!("Encoding difference: {encoding}\n\n{other_encoding}");
                }
            }
        },
        Args::Generate {
            output,
            config,
        } => {
            let semantics = scope(|s| {
                let c16s16 = s.spawn(|| generate_encodings(Mode::RealOrProtected16, Db::Protected16, config));
                let c32s16 = s.spawn(|| generate_encodings(Mode::Protected32, Db::Protected16, config));
                let c16s32 = s.spawn(|| generate_encodings(Mode::RealOrProtected16, Db::Protected32, config));
                let c32s32 = s.spawn(|| generate_encodings(Mode::Protected32, Db::Protected32, config));

                let (c16s16_encodings, c16s16_map) = c16s16.join().unwrap();
                let (c16s32_encodings, c16s32_map) = c16s32.join().unwrap();
                let (c32s16_encodings, c32s16_map) = c32s16.join().unwrap();
                let (c32s32_encodings, c32s32_map) = c32s32.join().unwrap();

                println!("Merging encodings into a single list...");

                let mut all_encodings = Vec::new();
                let mut encoding_map = HashMap::new();
                let [c16s16, c16s32, c32s16, c32s32] = [
                    (c16s16_map, c16s16_encodings),
                    (c16s32_map, c16s32_encodings),
                    (c32s16_map, c32s16_encodings),
                    (c32s32_map, c32s32_encodings),
                ]
                .map(|(mut map, encodings)| {
                    let new_encoding_indices = encodings
                        .into_iter()
                        .map(|encoding| {
                            *encoding_map.entry(encoding.clone()).or_insert_with(|| {
                                let index = all_encodings.len();
                                all_encodings.push(encoding);
                                index
                            })
                        })
                        .collect::<Vec<_>>();

                    map.map(|index| new_encoding_indices[index]);
                    map
                });

                println!("Final semantics contain {} encodings", all_encodings.len());

                InstrSem {
                    maps: InstrMaps {
                        c16s16,
                        c16s32,
                        c32s16,
                        c32s32,
                    },
                    encodings: all_encodings,
                }
            });

            println!("Packing semantics...");
            let semantics = semantics.pack();

            println!("Compressing and writing result...");

            let w = BufWriter::new(File::create(output).unwrap());
            let w = lz4_flex::frame::FrameEncoder::new(w);
            pot::to_writer(&semantics, w.auto_finish()).unwrap();
        },
        Args::CountParts {
            semantics,
        } => {
            let f = BufReader::new(File::open(&semantics).unwrap());
            let f = FrameDecoder::new(f);
            let instr_semantics: PackedInstrSem = pot::from_reader(f).unwrap();
            let instr_semantics = instr_semantics.unpack();
            let mut max = 0;
            for encoding in instr_semantics.encodings.iter() {
                max = max.max(encoding.parts.len());
            }

            println!("Maximum amount of parts: {max}");
        },
        Args::Stats {
            semantics,
        } => {
            let f = BufReader::new(File::open(&semantics).unwrap());
            let f = FrameDecoder::new(f);
            let instr_semantics: PackedInstrSem = pot::from_reader(f).unwrap();

            println!(
                "{} encodings ({:.1} MiB)",
                instr_semantics.num_encodings(),
                instr_semantics.encodings_mem_size() as f64 / (1 << 20) as f64
            );
            println!(
                "{} names ({:.1} MiB)",
                instr_semantics.num_names(),
                instr_semantics.names_mem_size() as f64 / (1 << 20) as f64
            );
            println!(
                "{} equivalent prefixes ({:.1} MiB)",
                instr_semantics.num_equivalent_prefixes(),
                instr_semantics.equivalent_prefixes_mem_size() as f64 / (1 << 20) as f64
            );
            println!(
                "{} parts ({:.1} MiB)",
                instr_semantics.num_parts(),
                instr_semantics.parts_size() as f64 / (1 << 20) as f64
            );
            println!(
                "{} addresses ({:.1} MiB)",
                instr_semantics.num_addresses(),
                instr_semantics.addresses_size() as f64 / (1 << 20) as f64
            );
            println!(
                "{} commands ({:.1} MiB)",
                instr_semantics.num_commands(),
                instr_semantics.commands_mem_size() as f64 / (1 << 20) as f64
            );
            println!(
                "{} part packings ({:.1} MiB)",
                instr_semantics.num_part_packings(),
                instr_semantics.part_packings_mem_size() as f64 / (1 << 20) as f64
            );

            let instr_semantics = instr_semantics.unpack();

            let mut num = HashMap::new();
            for encoding in instr_semantics.encodings.iter() {
                *num.entry(encoding.semantics.name.clone()).or_insert(0) += 1;
            }

            let max_part_count = instr_semantics.encodings.iter().map(|e| e.parts.len()).max().unwrap();
            let max_part_bits = instr_semantics
                .encodings
                .iter()
                .map(|e| e.parts.iter().map(|p| p.size).sum::<usize>())
                .max()
                .unwrap();

            for (name, num) in num.iter().sorted_by_key(|&(_, n)| n) {
                println!("{num:>4}x - {name}");
            }

            println!("Total encodings: {}", instr_semantics.encodings.len());
            println!("Max part count: {max_part_count}");
            println!("Max part bits: {max_part_bits}");
        },
    }
}

fn print_elf_info(obj: Object) {
    let elf = xmas_elf::ElfFile::new(obj.bytes()).unwrap();
    for section in elf.section_iter() {
        // println!("Section type: {:?}", section.type_());
        if section.get_type() != Ok(ShType::Null) {
            // println!("Section {}: {section:?}", section.get_name(&elf).unwrap());
            if section.get_name(&elf).unwrap() == ".text" {
                println!("Code size: {:.2} KiB", section.size() as f64 / 1024.0);
            }
        }
    }
}

fn generate_encodings(
    mode: Mode, stack_addr_size: Db, config: Config,
) -> (
    Vec<Encoding<Intel386, MiniSem<Intel386>, IgnoredMetadata>>,
    InstructionMap<usize>,
) {
    println!("Generating encodings for {mode:?}");

    let encodings = into_encodings(mode, stack_addr_size, semgen::instrs::all(config));
    println!("Generated {} encodings for {mode:?}", encodings.len());

    let max_mem_access_count = encodings.iter().map(|e| e.semantics.addresses.len()).max().unwrap();

    println!("Building map for {mode:?}...");
    let map = build_encoding_map(&encodings);

    println!(
        "Finished generating encodings for {mode:?} (map has {} nodes) -- maximum memory access count: {max_mem_access_count}",
        map.num_nodes()
    );

    (encodings, map)
}
