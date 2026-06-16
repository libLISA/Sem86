//! This benchmark aims to measure overall instruction execution performance.
//! All benchmarks are set up to measure average time-per-instruction.
//! The benchmarks run the full emulator loop.

use std::io::{BufReader, Cursor};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

use arbitrary_int::u2;
use criterion::{Criterion, criterion_group, criterion_main};
use generativity::make_guard;
use itertools::Itertools;
use liblisa::arch::CpuState;
use log::{debug, info, trace};
use lz4_flex::frame::FrameDecoder;
use sem86_arch::mem::{Mem32, Shm};
use sem86_core::arch::intel386::{GpReg, Intel386Flag, State};
use sem86_core::decoder::PackedInstrSem;
use sem86_core::emulator::EmulatorContext;
use sem86_core::hw::Hw;
use sem86_core::system::{CachedDescriptorAccessRights, Db, Descriptor, GateDescriptor, GateType};
use sem86_core::time::EmulatorClock;

fn run_bench(
    c: &mut Criterion, name: &str, addr: u32, bytes: &[u8], encodings: Arc<PackedInstrSem>, initial_state: State,
    use_pagejit: bool,
) {
    let shm = Arc::new(Shm::new("bench", 64 << 20)); // 64MiB
    let mem = Arc::new(Mem32::new(shm.clone()));
    for addr in (0..=u32::MAX).step_by(shm.len() as usize) {
        mem.map_physical_memory_to_shm(addr as u64..addr as u64 + shm.len(), shm.clone(), None, 0, true);
    }

    // Timer IDT entry so interrupts don't crash
    let timer_ide = GateDescriptor::new(0, 0x0008, GateType::InterruptGate32, u2::new(0), true, 0xfafb);
    mem.write_slice(0xfafa0040, &u64::from(timer_ide).to_le_bytes(), false, &mut ())
        .unwrap();
    mem.write_slice(
        0xfafb0000,
        &[
            0xcf, // iret
        ],
        false,
        &mut (),
    )
    .unwrap();

    // Default GDT entries
    mem.write_slice(0xfaf90000, &0u64.to_le_bytes(), false, &mut ()).unwrap();
    mem.write_slice(0xfaf90008, &0x00CF9A000000FFFFu64.to_le_bytes(), false, &mut ())
        .unwrap();
    mem.write_slice(0xfaf90010, &0x00CF92000000FFFFu64.to_le_bytes(), false, &mut ())
        .unwrap();

    make_guard!(guard);
    let cga_mode_channel = channel();
    let ev_channel = channel();
    mem.write_slice(addr, bytes, false, &mut ()).unwrap();
    mem.clean_all_phys_frames();
    mem.invalidate_all_pages();
    debug!("Wrote bytes: {:02X} to 0x{addr:X}", bytes.iter().format(""));
    let mut emulator_ctx = EmulatorContext::new(
        &mem,
        encodings.clone(),
        State::default(),
        |intr| {
            Hw::new(
                mem.clone(),
                Vec::new(),
                cga_mode_channel.0,
                ev_channel.1,
                Arc::new(Shm::new("vgabios", 16)),
                intr,
                EmulatorClock::new_asynchronous(),
            )
        },
        guard,
    );
    emulator_ctx.set_pagejit_enabled(use_pagejit);

    let mut warmed_up = false;
    c.bench_function(name, |b| {
        if !warmed_up {
            // Clear all interrupts
            std::thread::sleep(Duration::from_millis(150));
            while emulator_ctx.emulator().hw_mut().check_interrupt().is_some() {}

            trace!("Preparing with use_pagejit={use_pagejit}...");
            // Wait until all pages have been JITed, or 200M instructions have been executed.
            if use_pagejit {
                while emulator_ctx.emulator().ctx().k < 200_000_000
                    || !emulator_ctx.emulator().ctx().mmio_ctx.icache.all_code_pages_jitted()
                {
                    trace!("Still warming up at k={}", emulator_ctx.emulator().ctx().k);
                    *emulator_ctx.emulator().cpu_mut() = initial_state.clone();
                    emulator_ctx.emulator().flush_cached_cpu_state();
                    emulator_ctx.set_break_on_int_fe(true);
                    emulator_ctx.run(None);
                    emulator_ctx.emulator().ctx().mmio_ctx.icache.greedily_request_pagejit();
                }
            }

            warmed_up = true;
        }

        trace!("Running benchmark...");
        b.iter_custom(|num| {
            let start = Instant::now();
            emulator_ctx.reset_k();
            // TODO: emulator_ctx.emulator().ctx().mmio_ctx.icache.clear();
            while emulator_ctx.emulator().ctx().k < num {
                *emulator_ctx.emulator().cpu_mut() = initial_state.clone();
                emulator_ctx.emulator().flush_cached_cpu_state();
                emulator_ctx.set_break_on_int_fe(true);
                emulator_ctx.run(None);
            }

            start.elapsed()
        });
    });

    emulator_ctx.pause();
}

fn from_asm(path: impl AsRef<Path>) -> Vec<u8> {
    let output = Command::new("nasm")
        .arg(path.as_ref().canonicalize().unwrap())
        .arg("-o")
        .arg("/dev/stdout")
        .output()
        .unwrap();

    assert!(output.status.success(), "{}", std::str::from_utf8(&output.stderr).unwrap());

    output.stdout
}

fn run_benches(c: &mut Criterion) {
    test_log::env_logger::init();
    let f = BufReader::new(Cursor::new(include_bytes!("../../x86.semantics")));
    let f = FrameDecoder::new(f);
    let instr_semantics: PackedInstrSem = pot::from_reader(f).unwrap();
    let instr_semantics = Arc::new(instr_semantics);

    let initial_state = {
        let mut s = State::default();
        let desc = Descriptor::flat_data(Db::Protected32);
        let default_ar = CachedDescriptorAccessRights::from(desc);
        for (seg, ar, limit) in [
            (GpReg::Cs, GpReg::CsAr, GpReg::CsLimit),
            (GpReg::Es, GpReg::EsAr, GpReg::EsLimit),
            (GpReg::Ds, GpReg::DsAr, GpReg::DsLimit),
            (GpReg::Ss, GpReg::SsAr, GpReg::SsLimit),
            (GpReg::Fs, GpReg::FsAr, GpReg::FsLimit),
            (GpReg::Gs, GpReg::GsAr, GpReg::GsLimit),
        ] {
            // Set up CS to point to GDT entry 1, others to point to 2.
            // You should set up the GDT if you need it.
            s.set_gpreg(seg, if seg == GpReg::Cs { 0x0008 } else { 0x0010 });
            s.set_gpreg(ar, u64::from(default_ar));
            s.set_gpreg(limit, u32::MAX as u64);
        }

        s.set_gpreg(GpReg::Cr0, 1);
        s.set_gpreg(GpReg::Ip, 0x1000);
        s.set_flag(Intel386Flag::If, true);
        s.set_gpreg(GpReg::IdtBase, 0xfafa0000);
        s.set_gpreg(GpReg::IdtLimit, 0x47);
        s.set_gpreg(GpReg::GdtBase, 0xfaf90000);
        s.set_gpreg(GpReg::GdtLimit, 0x17);
        s
    };

    for file in std::fs::read_dir(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("benches/full-system-benches")).unwrap() {
        let file = file.unwrap();
        let path = file.path();
        if path.extension().map(|x| x == "asm").unwrap_or(false) {
            let name = path.file_stem().unwrap().to_string_lossy();
            let compiled = from_asm(&path);
            let use_pagejit = !std::fs::read_to_string(&path).unwrap().contains("no-page-jit");
            info!("Compiled {name}: {:02X}", compiled.iter().format(""));
            run_bench(
                c,
                &name,
                0x1000,
                &compiled,
                instr_semantics.clone(),
                initial_state.clone(),
                use_pagejit,
            );
        }
    }
}

criterion_group! {
    name = benches;
    config = Criterion::default().measurement_time(Duration::from_secs(15));
    targets = run_benches,
}
criterion_main!(benches);
