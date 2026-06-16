use std::hint::black_box;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use rand::{Rng, SeedableRng};

fn f0(a: &mut u64) {
    *a += 1
}
fn f1(a: &mut u64) {
    *a += 2
}
fn f2(a: &mut u64) {
    *a += 3
}
fn f3(a: &mut u64) {
    *a += 4
}
fn f4(a: &mut u64) {
    *a += 5
}
fn f5(a: &mut u64) {
    *a += 6
}
fn f6(a: &mut u64) {
    *a += 7
}
fn f7(a: &mut u64) {
    *a += 8
}

fn run_bench(c: &mut Criterion) {
    for num_functions in 1..=8 {
        let functions = &[f0, f1, f2, f3, f4, f5, f6, f7][..num_functions];
        let mut rng = rand_xoshiro::Xoshiro256PlusPlus::seed_from_u64(0);
        let mut a = 0;

        c.bench_function(&format!("call{num_functions}_random_unrolled"), |b| {
            b.iter_custom(|num| {
                let start = Instant::now();

                const UNROLL_STEPS: usize = 4;
                for _ in 0..num / UNROLL_STEPS as u64 {
                    let x: [_; UNROLL_STEPS] = std::array::from_fn(|_| rng.random_range(0..num_functions));

                    for x in x {
                        (functions[x])(&mut a);
                    }
                }

                start.elapsed()
            });
        });

        c.bench_function(&format!("call{num_functions}_random"), |b| {
            b.iter(|| (functions[rng.random_range(0..num_functions)])(&mut a))
        });

        c.bench_function(&format!("call{num_functions}_sequential"), |b| {
            let mut n = 0;
            b.iter_custom(|num| {
                let start = Instant::now();

                for _ in 0..num {
                    n += 1;
                    black_box(rng.random_range(0..num_functions));
                    (functions[n % num_functions])(&mut a);
                }

                start.elapsed()
            });
        });
    }
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .measurement_time(Duration::from_secs(15))
        .warm_up_time(Duration::from_secs(1));
    targets = run_bench,
}
criterion_main!(benches);
