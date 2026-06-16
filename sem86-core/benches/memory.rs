use std::pin::Pin;
use std::sync::Arc;
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use generativity::make_guard;
use sem86_arch::mem::{Mem32, Shm};
use sem86_core::codegen::backends::inkwell::{InkwellBackend, InkwellContext};
use sem86_core::codegen::see::SingleEncodingExecution;
use sem86_core::decoder::PackedInstrSem;
use sem86_core::emulator::exec::ExecutionContext;
use sem86_core::hw::Hw;
use sem86_core::hw::intr::Intr;
use sem86_core::icache::InstructionCache;
use sem86_core::time::EmulatorClock;

fn memory_benches(c: &mut Criterion) {
    let shm = Arc::new(Shm::new("bench", 64 << 20)); // 64MiB
    let mem = Arc::new(Mem32::new(shm.clone()));

    let intr = Intr::new();
    let intr = Pin::new(&intr);
    let hw = Hw::new(
        mem.clone(),
        Vec::new(),
        channel().0,
        channel().1,
        Arc::new(Shm::new("vbios", 1 << 16)),
        Intr::handle(intr),
        EmulatorClock::new_asynchronous(),
    );
    make_guard!(guard);
    let cache = InstructionCache::new(
        guard,
        Arc::new(PackedInstrSem::empty()),
        SingleEncodingExecution::new(InkwellBackend::new(InkwellContext::leak_new()), 0),
    );
    let mut ctx = ExecutionContext::new(hw, &mem, None, cache);

    c.bench_function("page_walk_predictable", |b| {
        for n in 0..1024 {
            // Set all PDEs to 0x1000
            mem.write_physical_slice(n * 4, &0x00001007u32.to_le_bytes(), &mut ctx.mmio_ctx)
                .unwrap();

            // Set all PTEs to 0x2000
            mem.write_physical_slice(0x1000 + n * 4, &0x00002007u32.to_le_bytes(), &mut ctx.mmio_ctx)
                .unwrap();
        }

        mem.set_page_directory_base(0);
        b.iter(|| black_box(mem.page_walk(0x123000, false)));
    });

    c.bench_function("page_walk_random", |b| {
        for n in 0..1024 {
            // Set all PDEs to 0x1000
            mem.write_physical_slice(n * 4, &0x00001007u32.to_le_bytes(), &mut ctx.mmio_ctx)
                .unwrap();

            // Set all PTEs to 0x2000
            mem.write_physical_slice(0x1000 + n * 4, &0x00002007u32.to_le_bytes(), &mut ctx.mmio_ctx)
                .unwrap();
        }

        mem.set_page_directory_base(0);
        b.iter(|| black_box(mem.page_walk(rand::random(), false)));
    });

    c.bench_function("mapped_write_u32_hwmmio", |b| {
        for n in 0..1024 {
            // Set all PDEs to 0x1000
            mem.write_physical_slice(n * 4, &0x00001007u32.to_le_bytes(), &mut ctx.mmio_ctx)
                .unwrap();

            // Set all PTEs to 0x2000
            mem.write_physical_slice(0x1000 + n * 4, &0x00002007u32.to_le_bytes(), &mut ctx.mmio_ctx)
                .unwrap();
        }

        mem.set_page_directory_base(0);
        b.iter(|| black_box(mem.write(0x5120, false, 0u32, &mut ctx.mmio_ctx).unwrap()));
    });

    c.bench_function("mapped_unaligned_write_u32_hwmmio", |b| {
        for n in 0..1024 {
            // Set all PDEs to 0x1000
            mem.write_physical_slice(n * 4, &0x00001007u32.to_le_bytes(), &mut ctx.mmio_ctx)
                .unwrap();

            // Set all PTEs to 0x2000
            mem.write_physical_slice(0x1000 + n * 4, &0x00002007u32.to_le_bytes(), &mut ctx.mmio_ctx)
                .unwrap();
        }

        mem.set_page_directory_base(0);
        b.iter(|| black_box(mem.write(0x5121, false, 0u32, &mut ctx.mmio_ctx).unwrap()));
    });

    c.bench_function("mapped_read_u32_hwmmio", |b| {
        for n in 0..1024 {
            // Set all PDEs to 0x1000
            mem.write_physical_slice(n * 4, &0x00001007u32.to_le_bytes(), &mut ctx.mmio_ctx)
                .unwrap();

            // Set all PTEs to 0x2000
            mem.write_physical_slice(0x1000 + n * 4, &0x00002007u32.to_le_bytes(), &mut ctx.mmio_ctx)
                .unwrap();
        }

        mem.set_page_directory_base(0);

        b.iter(|| black_box(mem.read_u32(0x120, false, &mut ctx.mmio_ctx).unwrap()));
    });

    c.bench_function("mapped_unaligned_read_u32_hwmmio", |b| {
        for n in 0..1024 {
            // Set all PDEs to 0x1000
            mem.write_physical_slice(n * 4, &0x00001007u32.to_le_bytes(), &mut ctx.mmio_ctx)
                .unwrap();

            // Set all PTEs to 0x2000
            mem.write_physical_slice(0x1000 + n * 4, &0x00002007u32.to_le_bytes(), &mut ctx.mmio_ctx)
                .unwrap();
        }

        mem.set_page_directory_base(0);

        b.iter(|| black_box(mem.read_u32(0x121, false, &mut ctx.mmio_ctx).unwrap()));
    });

    c.bench_function("mapped_read_u32_executioncontext_mmio", |b| {
        for n in 0..1024 {
            // Set all PDEs to 0x1000
            mem.write_physical_slice(n * 4, &0x00001007u32.to_le_bytes(), &mut ctx.mmio_ctx)
                .unwrap();

            // Set all PTEs to 0x2000
            mem.write_physical_slice(0x1000 + n * 4, &0x00002007u32.to_le_bytes(), &mut ctx.mmio_ctx)
                .unwrap();
        }

        mem.set_page_directory_base(0);

        b.iter(|| black_box(mem.read_u32(0x120, false, &mut ctx.mmio_ctx).unwrap()));
    });

    c.bench_function("unmapped_read_u32", |b| {
        for n in 0..1024 {
            // Set all PDEs to not present
            mem.write_physical_slice(n * 4, &0u32.to_le_bytes(), &mut ctx.mmio_ctx)
                .unwrap();

            // Set all PTEs to not present
            mem.write_physical_slice(0x1000 + n * 4, &0u32.to_le_bytes(), &mut ctx.mmio_ctx)
                .unwrap();
        }

        mem.set_page_directory_base(0);

        b.iter(|| black_box(mem.read_u32(0xffff_0000, false, &mut ctx.mmio_ctx).unwrap()));
    });

    c.bench_function("first_read", |b| {
        for n in 0..1024 {
            // Set all PDEs to 0x1000
            mem.write_physical_slice(n * 4, &0x00001007u32.to_le_bytes(), &mut ctx.mmio_ctx)
                .unwrap();

            // Set all PTEs to 0x2000
            mem.write_physical_slice(0x1000 + n * 4, &0x00002007u32.to_le_bytes(), &mut ctx.mmio_ctx)
                .unwrap();
        }

        mem.set_page_directory_base(0);

        b.iter_custom(|mut num| {
            let mut total = Duration::ZERO;
            let max = 10_000;
            while num > 0 {
                let count = max.min(num);
                let start = Instant::now();
                for n in 0..count {
                    black_box(mem.read_u32(0x120 + 0x1000 * n as u32, false, &mut ctx.mmio_ctx).unwrap());
                }
                mem.invalidate_all_pages();
                total += start.elapsed();

                num -= count;
            }

            total
        })
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default().measurement_time(Duration::from_secs(30));
    targets = memory_benches
}
criterion_main!(benches);
