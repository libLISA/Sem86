use std::hint::black_box;
use std::sync::Arc;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use sem86_arch::mem::{Mem32, MmioId, Shm};

fn memory_benches(c: &mut Criterion) {
    let shm = Arc::new(Shm::new("bench", 64 << 20)); // 64MiB
    let mem = Mem32::new(shm.clone());
    mem.map_physical_memory_to_default(0..64 << 20);

    c.bench_function("metadata_clears", |b| {
        mem.enable_paging(false);
        b.iter(|| {
            black_box(mem.invalidate_all_pages());
        })
    });

    c.bench_function("clean_unused_phys_frame", |b| {
        mem.enable_paging(false);
        b.iter(|| {
            black_box(mem.clean_phys_frame(0));
        })
    });

    c.bench_function("hybridmmu_mapped_read_u8", |b| {
        mem.enable_paging(false);
        b.iter(|| {
            black_box(mem.read::<u8>(black_box(123), black_box(false), &mut ()).unwrap());
        })
    });

    c.bench_function("hybridmmu_mapped_read_u32", |b| {
        mem.enable_paging(false);
        b.iter(|| {
            black_box(mem.read::<u32>(black_box(123), black_box(false), &mut ()).unwrap());
        })
    });

    c.bench_function("hybridmmu_mmio_read_u8", |b| {
        mem.map_physical_memory_to_mmio(0xb8000..0xc0000, MmioId::new(0));
        b.iter(|| {
            black_box(mem.read::<u8>(black_box(0xb8000), black_box(false), &mut ()).unwrap());
        })
    });

    c.bench_function("hybridmmu_unmapped_read_phys_mem", |b| {
        mem.enable_paging(false);
        b.iter_custom(|mut num| {
            let mut total = Duration::ZERO;
            let max = shm.len() / 0x1000;
            while num > 0 {
                let count = max.min(num);
                let start = Instant::now();
                for n in 0..count {
                    black_box(mem.read::<u8>(123 + 0x1000 * n as u32, false, &mut ()).unwrap());
                }
                total += start.elapsed();
                mem.invalidate_all_pages();

                num -= count;
            }

            total
        })
    });

    c.bench_function("hybridmmu_unmapped_read_virt_mem", |b| {
        for n in 0..1024 {
            // Set all PDEs to 0x1000
            mem.write_slice(n * 4, &0x00001007u32.to_le_bytes(), false, &mut ()).unwrap();

            // Set all PTEs to 0x2000
            mem.write_slice(0x1000 + n * 4, &0x00002007u32.to_le_bytes(), false, &mut ())
                .unwrap();
        }

        mem.set_page_directory_base(0);

        b.iter_custom(|mut num| {
            let mut total = Duration::ZERO;
            let max = 0x1_0000_0000 / 0x1000;
            while num > 0 {
                let count = max.min(num);
                let start = Instant::now();
                for n in 0..count {
                    black_box(mem.read::<u8>(123 + 0x1000 * n as u32, false, &mut ()).unwrap());
                }
                total += start.elapsed();
                mem.invalidate_all_pages();

                num -= count;
            }

            total
        })
    });

    c.bench_function("hybridmmu_unmapped_read_virt_mem_with_invalidation", |b| {
        for n in 0..1024 {
            // Set all PDEs to 0x1000
            mem.write_slice(n * 4, &0x00001007u32.to_le_bytes(), false, &mut ()).unwrap();

            // Set all PTEs to 0x2000
            mem.write_slice(0x1000 + n * 4, &0x00002007u32.to_le_bytes(), false, &mut ())
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
                    black_box(mem.read::<u8>(123 + 0x1000 * n as u32, false, &mut ()).unwrap());
                }
                mem.invalidate_all_pages();
                total += start.elapsed();

                num -= count;
            }

            total
        })
    });

    c.bench_function("hybridmmu_unmapped_write_virt_mem_with_invalidation", |b| {
        for n in 0..1024 {
            // Set all PDEs to 0x1000
            mem.write_slice(n * 4, &0x00001007u32.to_le_bytes(), false, &mut ()).unwrap();

            // Set all PTEs to 0x2000
            mem.write_slice(0x1000 + n * 4, &(0x1000 * n + 0x00002007u32).to_le_bytes(), false, &mut ())
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
                    mem.write::<u8>(123 + 0x1000 * n as u32, false, 0, &mut ()).unwrap();
                }

                for n in 2..1026 {
                    mem.clean_phys_frame(n * 0x1000);
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
