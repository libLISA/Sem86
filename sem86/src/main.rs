use std::ffi::OsString;
use std::fs::File;
use std::io::{BufReader, Cursor, Read, Write};
use std::path::PathBuf;
use std::process::exit;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;
use std::time::Instant;

use clap::Parser;
use fatfs::{FileSystem, FormatVolumeOptions, FsOptions, format_volume};
use image::{ImageBuffer, Rgba};
use liblisa::arch::CpuState;
use log::info;
use lz4_flex::frame::FrameDecoder;
use mbrs::{Mbr, PartInfo};
use sem86::App;
use sem86_arch::mem::{Mem32, Shm};
use sem86_core::arch::intel386::{GpReg, State};
use sem86_core::decoder::PackedInstrSem;
use sem86_core::emulator::EmulatorContext;
use sem86_core::hw::Hw;
use sem86_core::hw::storage::{CowDiskData, DiskData, FileDiskData};
use sem86_core::time::EmulatorClock;
use sem86_core::tracefile::TraceEntryReader;
use winit::event_loop::EventLoop;

pub fn hex(s: &str) -> Result<u64, String> {
    u64::from_str_radix(s, 16).map_err(|s| s.to_string())
}

pub fn hex_allowed(s: &str) -> Result<u64, String> {
    if let Some(s) = s.strip_prefix("0x") {
        u64::from_str_radix(s, 16).map_err(|s| s.to_string())
    } else {
        u64::from_str(s).map_err(|s| s.to_string())
    }
}

#[derive(Clone, Debug)]
struct Rom {
    addr: Option<u64>,
    path: PathBuf,
}

impl FromStr for Rom {
    type Err = <PathBuf as FromStr>::Err;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let m = s.split(':').collect::<Vec<_>>();
        Ok(match *m {
            [path] => Rom {
                addr: None,
                path: FromStr::from_str(path)?,
            },
            [addr, path] => Rom {
                addr: Some(hex(addr).expect("address should be valid hexadecimal number")),
                path: FromStr::from_str(path)?,
            },
            _ => panic!("unable to decipher: {s:?}"),
        })
    }
}

/// Sem86 - a full system emulator without hardcoded semantics.
#[derive(Clone, Debug, Parser)]
pub struct Args {
    /// Allows placing additional ROMs in memory.
    /// Specify a ROM as '<hex address>:<path>' to place it at a specific address.
    /// Leaving out the address will let Sem86 place it at a suitable address.
    #[clap(long = "rom")]
    roms: Vec<Rom>,

    /// The video BIOS.
    /// Should be Bochs' VGABIOS-lgpl
    #[clap(long)]
    vgabios: PathBuf,

    /// The entry point.
    /// You should typically not set this, as changing this value will cause BIOS initialization to be skipped.
    #[clap(long, value_parser=hex)]
    entry: Option<u64>,

    /// The x86 instruction semantics that will be emulated.
    #[clap(long)]
    semantics: PathBuf,

    /// Provides the semantics to which --switch-semantics-at will switch.
    #[clap(long, requires = "switch_semantics_at")]
    switch_semantics_to: Option<PathBuf>,

    /// Switches semantics after the specified number of instructions have been executed.
    #[clap(long, requires = "switch_semantics_to")]
    switch_semantics_at: Option<u64>,

    /// A floppy disk image.
    #[clap(long)]
    fdd: Option<PathBuf>,

    /// The disk image for the IDE0:0 disk.
    #[clap(long)]
    ide_0_0: Option<PathBuf>,

    #[clap(long)]
    ide_0_0_writable: bool,

    /// The disk image for the IDE1:0 disk.
    #[clap(long)]
    ide_1_0: Option<PathBuf>,

    /// Sets the number of instructions that should be executed.
    #[clap(long)]
    num: Option<u64>,

    /// Checks instruction execution against a pre-recorded tracefile.
    #[clap(long)]
    trace: Option<PathBuf>,

    /// Terminates trace checking after the specified number of instructions have been executed.
    #[clap(long, requires = "trace")]
    trace_limit: Option<u64>,

    /// Prints instruction-level execution information at specific points.
    /// Can be provided multiple times.
    #[clap(long)]
    print_at: Vec<u64>,

    /// The memory size, in megabytes.
    #[clap(long, short, default_value = "256")]
    memory: usize,

    /// Terminates when the number of instructions specified by --num have been executed.
    #[clap(long)]
    exit_when_done: bool,

    /// Makes a screenshot of the video output when the number of instructions specified by --num have been executed.
    #[clap(long, requires = "exit_when_done")]
    screenshot_when_done: Option<PathBuf>,

    /// Tries to avoid grabbing focus when the window opens.
    #[clap(long)]
    start_unfocussed: bool,

    /// Measures the execution times of individual encodings.
    #[clap(long)]
    measure_single_encoding_execution: bool,

    /// Enables profiling.
    #[clap(long)]
    enable_profiling: bool,

    /// Verifies icache consistency after every operation.
    #[clap(long)]
    verify_icache_consistency: bool,

    /// Regularly prints memory statistics.
    #[clap(long)]
    show_memory_statistics: bool,

    /// Disables page-wide JIT code generation.
    #[clap(long)]
    disable_page_jit: bool,

    #[clap(long)]
    generic_debugging_limit: Option<u64>,

    /// Resumes from a snapshot.
    #[clap(long)]
    resume_from_snapshot: Option<PathBuf>,

    /// Creates a snapshot when the number of instructions specified with --num have been executed.
    #[clap(long, requires = "num")]
    snapshot_when_done: Option<PathBuf>,

    /// Skips all trace differences before a certain amount of instructions have been executed.
    #[clap(long, requires = "trace")]
    skip_trace_differences_before: Option<u64>,

    /// Regularly prints the value of the specified logical memory address.
    #[clap(long, value_parser=hex)]
    trace_mem_at: Option<u64>,

    /// Regularly prints the value of the specified physical memory address.
    #[clap(long, value_parser=hex)]
    trace_phys_mem_at: Option<u64>,

    /// Enables a deterministic clock, that is updated synchronously with instruction execution.
    /// This makes it possible to bisect execution that is timing-dependent.
    #[clap(long)]
    synchronous_clock: bool,

    /// Disables all interrupts.
    #[clap(long)]
    no_interrupts: bool,

    /// Disables the ES1370 PCI card for audio.
    #[clap(long)]
    disable_es1370: bool,
}

#[allow(unused)]
fn create_empty_disk(size: usize) -> Vec<u8> {
    let mut partition_data = vec![0; size];
    let options = FormatVolumeOptions::new()
        .volume_label(*b"VIRT-DISK01")
        .fat_type(fatfs::FatType::Fat12)
        .bytes_per_sector(512);
    let mut fat_data = Cursor::new(&mut partition_data);
    format_volume(&mut fat_data, options).unwrap();

    drop(FileSystem::new(fat_data, FsOptions::new()).unwrap());

    let mut mbr = Mbr::try_from_bytes(&partition_data[..512].try_into().unwrap()).expect("should be able to parse MBR");

    mbr.partition_table.entries[0] = Some(
        PartInfo::try_from_lba(
            true,
            1,
            (size / 512 - 1) as u32,
            mbrs::PartType::Fat12 {
                visible: true,
            },
        )
        .unwrap(),
    );
    mbr.bootloader = [
        0xfa, 0x33, 0xc0, 0x8e, 0xd0, 0xbc, 0x00, 0x7c, 0x8b, 0xf4, 0x50, 0x07, 0x50, 0x1f, 0xfb, 0xfc, 0xbf, 0x00, 0x06, 0xb9,
        0x00, 0x01, 0xf3, 0xa5, 0xea, 0x1d, 0x06, 0x00, 0x00, 0xbe, 0xbe, 0x07, 0xb3, 0x04, 0x80, 0x3c, 0x80, 0x74, 0x0e, 0x80,
        0x3c, 0x00, 0x75, 0x1c, 0x83, 0xc6, 0x10, 0xfe, 0xcb, 0x75, 0xef, 0xcd, 0x18, 0x8b, 0x14, 0x8b, 0x4c, 0x02, 0x8b, 0xee,
        0x83, 0xc6, 0x10, 0xfe, 0xcb, 0x74, 0x1a, 0x80, 0x3c, 0x00, 0x74, 0xf4, 0xbe, 0x8b, 0x06, 0xac, 0x3c, 0x00, 0x74, 0x0b,
        0x56, 0xbb, 0x07, 0x00, 0xb4, 0x0e, 0xcd, 0x10, 0x5e, 0xeb, 0xf0, 0xeb, 0xfe, 0xbf, 0x05, 0x00, 0xbb, 0x00, 0x7c, 0xb8,
        0x01, 0x02, 0x57, 0xcd, 0x13, 0x5f, 0x73, 0x0c, 0x33, 0xc0, 0xcd, 0x13, 0x4f, 0x75, 0xed, 0xbe, 0xa3, 0x06, 0xeb, 0xd3,
        0xbe, 0xc2, 0x06, 0xbf, 0xfe, 0x7d, 0x81, 0x3d, 0x55, 0xaa, 0x75, 0xc7, 0x8b, 0xf5, 0xea, 0x00, 0x7c, 0x00, 0x00, 0x49,
        0x6e, 0x76, 0x61, 0x6c, 0x69, 0x64, 0x20, 0x70, 0x61, 0x72, 0x74, 0x69, 0x74, 0x69, 0x6f, 0x6e, 0x20, 0x74, 0x61, 0x62,
        0x6c, 0x65, 0x00, 0x45, 0x72, 0x72, 0x6f, 0x72, 0x20, 0x6c, 0x6f, 0x61, 0x64, 0x69, 0x6e, 0x67, 0x20, 0x6f, 0x70, 0x65,
        0x72, 0x61, 0x74, 0x69, 0x6e, 0x67, 0x20, 0x73, 0x79, 0x73, 0x74, 0x65, 0x6d, 0x00, 0x4d, 0x69, 0x73, 0x73, 0x69, 0x6e,
        0x67, 0x20, 0x6f, 0x70, 0x65, 0x72, 0x61, 0x74, 0x69, 0x6e, 0x67, 0x20, 0x73, 0x79, 0x73, 0x74, 0x65, 0x6d, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];

    info!("Created MBR: {mbr:?}");

    partition_data[..512].copy_from_slice(&<[u8; 512]>::try_from(&mbr).unwrap());

    partition_data
}

struct ExitIfDropped(bool);

impl ExitIfDropped {
    pub fn new() -> Self {
        Self(true)
    }

    pub fn cancel(mut self) {
        self.0 = false;
    }
}

impl Drop for ExitIfDropped {
    fn drop(&mut self) {
        if self.0 {
            exit(7);
        }
    }
}

impl Args {
    fn print_memory_statistics(&self, loc: &str) {
        if self.show_memory_statistics {
            println!("Memory usage {loc}:");
            println!("{}", std::fs::read_to_string("/proc/self/smaps_rollup").unwrap());
        }
    }
}

fn main() {
    env_logger::init();
    let (sender, recv) = channel();

    let args = Args::parse();
    let args = &args;

    println!("Loading semantics...");
    let start = Instant::now();
    args.print_memory_statistics("before loading semantics");
    let f = BufReader::new(File::open(&args.semantics).unwrap());
    let f = FrameDecoder::new(f);
    let instr_semantics: PackedInstrSem = pot::from_reader(f).unwrap();
    let instr_semantics = Arc::new(instr_semantics);

    let switch_semantics_to = args.switch_semantics_to.as_ref().map(|path| {
        let f = BufReader::new(File::open(path).unwrap());
        let f = FrameDecoder::new(f);
        let instr_semantics: PackedInstrSem = pot::from_reader(f).unwrap();
        Arc::new(instr_semantics)
    });

    args.print_memory_statistics("after loading semantics");
    // let mut decoder = Decoder::new(&instr_semantics);

    println!("Loading semantics took {}ms", start.elapsed().as_millis());

    let physical_memory = Arc::new(Shm::new("physical_memory", args.memory << 20));
    let v = physical_memory.view();
    args.print_memory_statistics("after creating physical memory");

    // Mimic Bochs' behavior.
    for addr in 0xc0000..0xe0000 {
        v.write_byte(addr, 0xff);
    }

    let vgabios_data = std::fs::read(&args.vgabios).unwrap();
    let vgabios = Arc::new(Shm::new("vgabios", (vgabios_data.len() + 0xfff) & !0xfff));
    vgabios.view().write_slice(0, &vgabios_data);
    args.print_memory_statistics("after loading vgabios");

    info!("Loaded VGABIOS of {} bytes", vgabios_data.len());

    args.print_memory_statistics("after writing ROMs");
    println!("Loading disks...");
    let disks = args
        .fdd
        .iter()
        .map(|path| {
            let file = File::open(path).unwrap();
            let file_backend = Box::new(FileDiskData::new(file));
            DiskData::new(Box::new(CowDiskData::new(file_backend)))
        })
        .collect::<Vec<_>>();

    let entry = args.entry.unwrap_or(0xffff0000);
    let cs = (entry >> 16) as u16;
    let ip = entry as u16;
    let mut state = State::create(|_, _| ());
    state.set_gpreg(GpReg::Ip, ip as u64);
    state.set_gpreg(GpReg::Cs, cs as u64);
    state.set_gpreg(GpReg::CsBase, (cs as u64) * 16);

    let (cga_mode_sender, cga_mode_recv) = channel();

    let memory = Arc::new(Mem32::new(physical_memory));
    generativity::make_guard!(guard);

    let running = AtomicBool::new(true);
    let running = Arc::new(running);
    let (video_memory_sender, video_memory_recv) = channel();

    let running_copy = running.clone();
    std::thread::scope(|s| {
        std::thread::Builder::new()
            .name("emulator-thread".into())
            // Require a big stack size for all stack-allocated structures
            .stack_size(128 << 20)
            .spawn_scoped(s, move || {
                info!("Setting up emulator..");
                let (clock, sclock) = if args.synchronous_clock {
                    let (clock, sclock) = EmulatorClock::new_synchronous();
                    (clock, Some(sclock))
                } else {
                    (EmulatorClock::new_asynchronous(), None)
                };

                let mut emu = EmulatorContext::new(
                    &memory,
                    instr_semantics,
                    state,
                    |intr| {
                        let mut hw = Hw::new(memory.clone(), disks, cga_mode_sender, recv, vgabios, intr, clock);
                        let video_memory = hw.video_memory();
                        video_memory_sender.send(video_memory).unwrap();
                        let memory = &memory;

                        info!("Mapping ROMs...");
                        {
                            for (index, rom) in args.roms.iter().enumerate() {
                                let data = std::fs::read(&rom.path).unwrap();
                                let shm = Arc::new(Shm::new("rom", (data.len() + 0xfff) & !0xfff));
                                shm.view().write_slice(0, &data);
                                let addr = rom.addr.unwrap_or_else(|| {
                                    if index == 0 {
                                        0x100000 - shm.len() as u64
                                    } else {
                                        panic!("rom {:?} should have an address", rom.path)
                                    }
                                });

                                println!("Placing ROM {:?} at 0x{addr:X}", rom.path);

                                // TODO: This shouldn't be writable
                                memory.map_physical_memory_to_shm(addr..addr + shm.len(), shm, None, 0, true);
                            }
                        }

                        if let Some(path) = &args.ide_0_0 {
                            hw.set_disk(
                                0,
                                0,
                                Some(if path.exists() {
                                    DiskData::new(if args.ide_0_0_writable {
                                        Box::new(FileDiskData::new(File::options().write(true).read(true).open(path).unwrap()))
                                    } else {
                                        Box::new(CowDiskData::new(Box::new(FileDiskData::new(File::open(path).unwrap()))))
                                    })
                                } else {
                                    panic!("{path:?} does not exist");
                                }),
                            )
                        }

                        if let Some(path) = &args.ide_1_0 {
                            hw.set_disk(
                                1,
                                0,
                                Some(
                                    if path.exists() {
                                        DiskData::new(Box::new(CowDiskData::new(Box::new(FileDiskData::new(
                                            File::open(path).unwrap(),
                                        )))))
                                    } else {
                                        panic!("{path:?} does not exist");
                                    }
                                    .with_is_cd(path.extension() == Some(&OsString::from("iso"))),
                                ),
                            )
                        }

                        args.print_memory_statistics("after loading disks");
                        hw
                    },
                    guard,
                );

                if let Some(clock) = sclock {
                    emu.set_synchronous_clock(clock);
                }

                args.print_memory_statistics("after creating emulator context");
                emu.set_print_at(args.print_at.clone());
                emu.set_measure_single_encoding_execution(args.measure_single_encoding_execution);
                emu.set_profiling(args.enable_profiling);

                if args.disable_page_jit {
                    emu.set_pagejit_enabled(false);
                }

                if let Some(num) = args.generic_debugging_limit {
                    emu.set_generic_debugging_limit(num);
                }

                if args.no_interrupts {
                    emu.disable_interrupts();
                }

                if args.disable_es1370 {
                    emu.disable_es1370();
                }

                emu.set_verify_icache_consistency(args.verify_icache_consistency);

                if let Some(trace_path) = &args.trace {
                    let r = File::open(trace_path).unwrap();
                    let r = BufReader::with_capacity(64 << 10, r);
                    let r = FrameDecoder::new(r);
                    let r = TraceEntryReader::new(r); // Box::new(r) as Box<dyn Read + Send + 'static>);
                    emu.set_trace(r, args.trace_limit.unwrap_or(u64::MAX));

                    if let Some(n) = args.skip_trace_differences_before {
                        emu.set_skip_trace_differences_before(n);
                    }
                }

                let ensure_exit_if_crash = ExitIfDropped::new();

                if let Some(path) = &args.resume_from_snapshot {
                    println!("Decompressing snapshot...");
                    let start = Instant::now();
                    let f = File::open(path).unwrap();
                    let mut f = lz4_flex::frame::FrameDecoder::new(f);
                    let mut data = Vec::new();
                    f.read_to_end(&mut data).unwrap();

                    println!("Deserializing snapshot...");
                    let snapshot = pot::from_slice(&data).unwrap();
                    drop(data);

                    println!("Restoring snapshot...");
                    emu.restore(snapshot);

                    println!("Snapshot restored in {:.2}s", start.elapsed().as_secs_f32());
                }

                if let Some(n) = args.trace_mem_at {
                    emu.set_trace_mem_at(n as u32);
                }

                if let Some(n) = args.trace_phys_mem_at {
                    emu.set_trace_pmem_at(n as u32);
                }

                args.print_memory_statistics("before starting emulation");

                println!("Running...");
                emu.run(args.switch_semantics_at.or(args.num));

                if let Some(to) = switch_semantics_to {
                    println!("Switching semantics at {}", emu.k());

                    emu.set_semantics(to);
                    emu.run(args.num);
                }

                println!("Done running!");

                ensure_exit_if_crash.cancel();

                if args.exit_when_done {
                    running_copy.store(false, Ordering::SeqCst);
                }

                if let Some(path) = &args.snapshot_when_done {
                    println!("Making snapshot...");
                    let snapshot = emu.snapshot();

                    println!("Serializing...");
                    let data = pot::to_vec(&snapshot).unwrap();

                    println!("Compressing and writing snapshot to disk...");
                    let start = Instant::now();
                    let f = File::create(path).unwrap();
                    let mut f = lz4_flex::frame::FrameEncoder::new(f).auto_finish();
                    f.write_all(&data).unwrap();

                    println!("Compression took {:.2}s", start.elapsed().as_secs_f64());
                }
            })
            .unwrap();

        let event_loop = EventLoop::new().unwrap();
        let video_memory = video_memory_recv.recv().unwrap();
        let mut app = App::new(
            args.start_unfocussed,
            sender,
            Some(cga_mode_recv),
            running.clone(),
            video_memory,
        );
        event_loop.run_app(&mut app).unwrap();

        if let Some(path) = &args.screenshot_when_done {
            let (width, height, capture) = app.capture();
            let capture = bgra_to_rgba(capture);
            let buffer = ImageBuffer::<Rgba<u8>, _>::from_raw(width, height, capture).unwrap();
            buffer.save(path).unwrap();
        }
    });
}

fn bgra_to_rgba(mut data: Vec<u8>) -> Vec<u8> {
    for chunk in data.chunks_exact_mut(4) {
        chunk.swap(0, 2);
    }

    data
}
