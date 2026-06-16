#![allow(incomplete_features)]
#![feature(generic_const_exprs)]

use std::io::{BufReader, Cursor};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::mpsc::channel;
use std::time::Duration;

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use generativity::make_guard;
use lz4_flex::frame::FrameDecoder;
use rand::{RngCore, SeedableRng};
use rand_xoshiro::Xoshiro256Plus;
use sem86_arch::addr::LinAddr;
use sem86_arch::mem::{Mem32, Shm};
use sem86_core::SegmentSizes;
use sem86_core::codegen::backends::inkwell::{InkwellBackend, InkwellContext};
use sem86_core::codegen::see::SingleEncodingExecution;
use sem86_core::decoder::{EncodingLookup, PackedInstrSem};
use sem86_core::emulator::exec::ExecutionContext;
use sem86_core::hw::Hw;
use sem86_core::hw::intr::Intr;
use sem86_core::icache::tlb::Tlb;
use sem86_core::icache::{ContextFlags, InstructionCache};
use sem86_core::time::EmulatorClock;
use sem86_core::util::version::Versioner;

fn icache_benches(c: &mut Criterion) {
    let f = BufReader::new(Cursor::new(include_bytes!("../../x86.semantics")));
    let f = FrameDecoder::new(f);
    let instr_semantics: PackedInstrSem = pot::from_reader(f).unwrap();
    let instr_semantics = Arc::new(instr_semantics);
    make_guard!(guard);
    let cache = InstructionCache::new(
        guard,
        instr_semantics.clone(),
        SingleEncodingExecution::new(InkwellBackend::new(InkwellContext::leak_new()), instr_semantics.len()),
    );
    let shm = Arc::new(Shm::new("test", 1 << 20));
    let mem = Arc::new(Mem32::new(shm.clone()));
    mem.enable_paging(true);
    let intr = Intr::new();
    let intr = Pin::new(&intr);
    let hw = Hw::new(
        mem.clone(),
        Vec::new(),
        channel().0,
        channel().1,
        Arc::new(Shm::new("vgabios", 16)),
        Intr::handle(intr),
        EmulatorClock::new_asynchronous(),
    );
    let mut ctx = ExecutionContext::new(hw, &mem, None, cache);
    ctx.protected_mode = true;
    // Clear page tables
    mem.write_physical_slice(0, &[0; 4096 * 2], &mut ctx.mmio_ctx).unwrap();

    // Write PDE for 0x7c814000
    const PDE_OFFSET: u32 = (0x7c814000u32 >> 22) * 4;
    mem.write_physical_slice(PDE_OFFSET, &0x00001007u32.to_le_bytes(), &mut ctx.mmio_ctx)
        .unwrap();

    const PTE_OFFSET1: u32 = 0x1000 + ((0x7c814000u32 >> 12) & 0x3ff) * 4;
    const PTE_OFFSET2: u32 = 0x1000 + ((0x7c815000u32 >> 12) & 0x3ff) * 4;

    mem.write_physical_slice(PTE_OFFSET1, &0x00003007u32.to_le_bytes(), &mut ctx.mmio_ctx)
        .unwrap();
    mem.write_physical_slice(PTE_OFFSET2, &0x00004007u32.to_le_bytes(), &mut ctx.mmio_ctx)
        .unwrap();

    // Instructions
    mem.write_slice(
        0x7c814f00,
        &[
            // jmp rax (indirect jump, will not generate Certain links)
            0xff, 0xe0,
            0xff, 0xe0,
            0xff, 0xe0,
            0xff, 0xe0,
            0xff, 0xe0,
            0xff, 0xe0,
            0xff, 0xe0,
            0xff, 0xe0,
            0xff, 0xe0,
            0xff, 0xe0,
        ],
        false,
        &mut ctx.mmio_ctx
    ).unwrap();

    // Certain next
    mem.write_slice(
        0x7c814f80,
        &[
            // nop (guaranteed to always move to the next instruction, will generate Certain links)
            0x90, 0x90,
        ],
        false,
        &mut ctx.mmio_ctx,
    )
    .unwrap();

    mem.write_slice(
        0x7c815000,
        &[
            // jne
            0x0f, 0x85, 0x00, 0x00, 0x00, 0x00,
        ],
        false,
        &mut ctx.mmio_ctx,
    )
    .unwrap();

    // Load entries into cache
    let flags = ContextFlags::build(true, false, SegmentSizes::Cs32Ss32);

    const ENTRY_INSTR_ADDR: u32 = 0x7c814f00;
    const ENTRY_INSTR_ADDR2: u32 = 0x7c814f80;
    const ENTRY_INSTR_ADDR3: u32 = 0x7c815000;
    const NEXT_INSTR_ADDRS: &[u32] = &[
        0x7c814f02, 0x7c814f04, 0x7c814f06, 0x7c814f08, 0x7c814f0A, 0x7c814f0C, 0x7c814f0E, 0x7c814f10,
    ];

    let mut rng = Xoshiro256Plus::from_os_rng();
    c.bench_function("lookup_first", |b| {
        b.iter(|| {
            black_box(
                ctx.mmio_ctx
                    .icache
                    .lookup_first(
                        0,
                        NEXT_INSTR_ADDRS[rng.next_u32() as usize % NEXT_INSTR_ADDRS.len()],
                        flags,
                        &mem,
                        |_| (),
                        false,
                    )
                    .unwrap(),
            )
        });
    });

    for n in 1..8 {
        c.bench_function(&format!("lookup_{n}_speculative_next"), |b| {
            let first = ctx
                .mmio_ctx
                .icache
                .lookup_first(0, ENTRY_INSTR_ADDR, flags, &mem, |_| (), false)
                .unwrap();
            b.iter(|| {
                black_box(
                    ctx.mmio_ctx
                        .icache
                        .lookup_next_from_entry(
                            first,
                            (0, NEXT_INSTR_ADDRS[rng.next_u32() as usize % n], &*mem),
                            flags,
                            |_| (),
                            false,
                            false,
                        )
                        .unwrap(),
                )
            });
        });
    }

    c.bench_function(&"lookup_certain_next", |b| {
        let first = ctx
            .mmio_ctx
            .icache
            .lookup_first(0, ENTRY_INSTR_ADDR2, flags, &mem, |_| (), false)
            .unwrap();
        b.iter(|| {
            black_box(
                ctx.mmio_ctx
                    .icache
                    .lookup_next_from_entry(first, (0, ENTRY_INSTR_ADDR2 + 1, &*mem), flags, |_| (), false, false)
                    .unwrap(),
            )
        });
    });

    c.bench_function(&"lookup_pagejump_next", |b| {
        let first = ctx
            .mmio_ctx
            .icache
            .lookup_first(0, ENTRY_INSTR_ADDR3, flags, &mem, |_| (), false)
            .unwrap();
        b.iter(|| {
            black_box(
                ctx.mmio_ctx
                    .icache
                    .lookup_next_from_entry(first, (0, ENTRY_INSTR_ADDR2, &*mem), flags, |_| (), false, true)
                    .unwrap(),
            )
        });
    });

    c.bench_function("tlb_lookup", |b| {
        let first = ctx
            .mmio_ctx
            .icache
            .lookup_first(0, ENTRY_INSTR_ADDR3, flags, &mem, |_| (), false)
            .unwrap();
        let mut tlb = Tlb::<12>::new();
        let versioner = Versioner::new();
        let pc = LinAddr::new(0x1234);
        tlb.insert(pc, versioner.current_version(), first);

        b.iter(|| black_box(tlb.lookup(pc, versioner.current_version())).unwrap());
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default().measurement_time(Duration::from_secs(15));
    targets = icache_benches
}
criterion_main!(benches);
