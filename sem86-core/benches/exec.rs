use std::arch::asm;
use std::io::{BufReader, Cursor};
use std::sync::Arc;
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

use arbitrary_int::{u1, u2, u4, u13, u24};
use criterion::{Criterion, black_box, criterion_group, criterion_main};
use generativity::make_guard;
use liblisa::Instruction;
use liblisa::arch::CpuState;
use lz4_flex::frame::FrameDecoder;
use sem86_arch::exceptions::Interrupt;
use sem86_arch::mem::{Mem32, Shm};
use sem86_core::SegmentSizes;
use sem86_core::arch::intel386::{GpReg, State};
use sem86_core::codegen::backends::UninstantiatedBackendFn;
use sem86_core::codegen::backends::cranelift::CraneliftBackend;
use sem86_core::codegen::backends::inkwell::{InkwellBackend, InkwellContext};
use sem86_core::codegen::see::SingleEncodingExecution;
use sem86_core::decoder::{Decoder, EncodingLookup, PackedInstrSem};
use sem86_core::emulator::{EmulatorContext, pack_flags, unpack_flags};
use sem86_core::hw::Hw;
use sem86_core::il::BorrowEncoding;
use sem86_core::jit::test_encodings;
use sem86_core::system::{
    CodeOrData, Db, Descriptor, DescriptorFlags, GateDescriptor, GateType, Granularity, SegmentAccessByte, SegmentSelector,
};
use sem86_core::time::EmulatorClock;

fn bench_empty(c: &mut Criterion) {
    #[inline(never)]
    fn increment_fn(a: &mut u64) -> bool {
        *a += 1;
        true
    }

    #[inline(never)]
    fn empty_fn() -> bool {
        true
    }

    c.bench_function("empty_function_call_vzeroupper", |bencher: &mut criterion::Bencher<'_>| {
        let ptr = black_box(empty_fn as fn() -> bool);
        bencher.iter(|| unsafe {
            asm! {
                "vpcmpeqq ymm3, ymm3, ymm3",
                "vzeroupper",
                "call [r12]",
                in("r12") &ptr,
                clobber_abi("C")
            }
        })
    });

    c.bench_function("empty_function_call_novzeroupper", |bencher: &mut criterion::Bencher<'_>| {
        let ptr = black_box(empty_fn as fn() -> bool);
        bencher.iter(|| unsafe {
            asm! {
                "vpcmpeqq ymm3, ymm3, ymm3",
                "call [r12]",
                in("r12") &ptr,
                clobber_abi("C")
            }
        })
    });

    c.bench_function("empty_function_call_pipelined", |bencher: &mut criterion::Bencher<'_>| {
        let ptr = black_box(empty_fn as fn() -> bool);
        bencher.iter_custom(|num| unsafe {
            let start = Instant::now();
            for _ in 0..num / 8 {
                asm! {
                    "call [r12]",
                    "call [r12]",
                    "call [r12]",
                    "call [r12]",
                    "call [r12]",
                    "call [r12]",
                    "call [r12]",
                    "call [r12]",
                    in("r12") &ptr,
                    clobber_abi("C")
                }
            }

            start.elapsed()
        })
    });

    c.bench_function("increment_function_call", |bencher: &mut criterion::Bencher<'_>| {
        let mut a = 0;
        bencher.iter(|| black_box(increment_fn(&mut a)))
    });
}

fn bench_interrupt(c: &mut Criterion) {
    test_log::env_logger::try_init().ok();

    let shm = Arc::new(Shm::new("bench", 64 << 20)); // 64MiB
    let mem = Arc::new(Mem32::new(shm.clone()));

    make_guard!(guard);
    let mut emulator_ctx = EmulatorContext::new(
        &mem,
        Arc::new(PackedInstrSem::empty()),
        State::default(),
        |intr| {
            Hw::new(
                mem.clone(),
                Vec::new(),
                channel().0,
                channel().1,
                Arc::new(Shm::new("vgabios", 16)),
                intr,
                EmulatorClock::new_asynchronous(),
            )
        },
        guard,
    );
    emulator_ctx.emulator().ctx().set_protected_mode(true);

    let mut initial_cpu = State::default();
    initial_cpu.set_gpreg(GpReg::IdtBase, 0x5000);
    initial_cpu.set_gpreg(GpReg::IdtLimit, 0x1000);
    initial_cpu.set_gpreg(GpReg::GdtBase, 0x6000);
    initial_cpu.set_gpreg(GpReg::GdtLimit, 0x1000);
    initial_cpu.set_gpreg(GpReg::Cr0, 1);
    initial_cpu.set_gpreg(GpReg::Sp, 0x8000);

    let selector = SegmentSelector::new(u2::new(0), false, u13::new(1));
    let entry = GateDescriptor::new(0, selector.into(), GateType::InterruptGate32, u2::new(0), true, 0);
    mem.write(0x5000, false, u64::from(entry), &mut ()).unwrap();

    let descriptor = Descriptor::new(
        0xffff,
        u24::new(0),
        SegmentAccessByte::new(CodeOrData::new(false, true, false, true).into(), true, u2::new(0), true),
        u4::new(0xf),
        DescriptorFlags::new(u1::new(0), false, Db::Protected32, Granularity::Page),
        0xff,
    );
    mem.write(0x6008, false, u64::from(descriptor), &mut ()).unwrap();

    // iret IP
    mem.write(0x8000, false, 0xffffffffu32, &mut ()).unwrap();

    // iret CS
    mem.write(0x8004, false, u16::from(selector) as u32, &mut ()).unwrap();

    // iret flags
    mem.write(0x8008, false, 0xeeeeeeeeu32, &mut ()).unwrap();

    // TODO: Equivalent of IRET from 0:0xC0001525 to 0x15F:0x68FCF0 (operand size: Protected32, selector: SegmentSelector { rpl: 3, is_local: true, segment_index: 2B }, vm=false, next vm=false, k=5G648016973)

    c.bench_function("cr3_reload", |bencher: &mut criterion::Bencher<'_>| {
        let shm = Arc::new(Shm::new("bench", 64 << 20)); // 64MiB
        let mem = Arc::new(Mem32::new(shm.clone()));

        bencher.iter(|| {
            mem.set_page_directory_base(0);
        })
    });

    c.bench_function("interrupt_iret_novm", |bencher: &mut criterion::Bencher<'_>| {
        bencher.iter(|| {
            *emulator_ctx.emulator().cpu_mut() = initial_cpu.clone();
            black_box(emulator_ctx.emulator().iret(Db::Protected32)).unwrap();
        })
    });

    c.bench_function("enter_interrupt", |bencher: &mut criterion::Bencher<'_>| {
        bencher.iter(|| {
            *emulator_ctx.emulator().cpu_mut() = initial_cpu.clone();
            black_box(emulator_ctx.emulator().enter_interrupt(Interrupt::HardwareInterrupt(0))).unwrap();
        })
    });

    c.bench_function("restore_cpu_state_overhead", |bencher: &mut criterion::Bencher<'_>| {
        bencher.iter(|| {
            *emulator_ctx.emulator().cpu_mut() = black_box(initial_cpu.clone());
        })
    });

    c.bench_function("pack_flags", |bencher: &mut criterion::Bencher<'_>| {
        bencher.iter(|| {
            black_box(pack_flags(black_box(&initial_cpu)));
        })
    });

    c.bench_function("unpack_flags", |bencher: &mut criterion::Bencher<'_>| {
        let mut cpu = State::default();
        bencher.iter(|| {
            black_box(unpack_flags(&mut cpu, black_box(0), true, false));
        })
    });
}

fn bench_exec(c: &mut Criterion) {
    test_log::env_logger::try_init().ok();

    let empty_encoding = test_encodings::empty();

    let shm = Arc::new(Shm::new("bench", 64 << 20)); // 64MiB
    let mem = Arc::new(Mem32::new(shm.clone()));
    mem.map_physical_memory_to_default(0..64 << 20);

    let f = BufReader::new(Cursor::new(include_bytes!("../../x86.semantics")));
    let f = FrameDecoder::new(f);
    let instr_semantics: PackedInstrSem = pot::from_reader(f).unwrap();
    let instr_semantics = Arc::new(instr_semantics);
    let mut decoder = Decoder::new(instr_semantics.clone());

    make_guard!(guard);
    let mut emulator_ctx = EmulatorContext::new(
        &mem,
        instr_semantics.clone(),
        State::default(),
        |intr| {
            Hw::new(
                mem.clone(),
                Vec::new(),
                channel().0,
                channel().1,
                Arc::new(Shm::new("vgabios", 16)),
                intr,
                EmulatorClock::new_asynchronous(),
            )
        },
        guard,
    );

    let extra = [
        (Instruction::new(&[0x8B, 0x03]), SegmentSizes::Cs32Ss32), // MOV EAX, DWORD PTR [EBX]
        (Instruction::new(&[0x89, 0xD8]), SegmentSizes::Cs32Ss32), // MOV EAX, EBX
        (Instruction::new(&[0x03, 0x03]), SegmentSizes::Cs32Ss32), // ADD EAX, DWORD PTR [EBX]
        (Instruction::new(&[0x01, 0xD8]), SegmentSizes::Cs32Ss32), // ADD EAX, EBX
        (Instruction::new(&[0xF3, 0xA5]), SegmentSizes::Cs16Ss16),
        (Instruction::new(&[0xF3, 0x66, 0x67, 0xA5]), SegmentSizes::Cs16Ss16),
        (Instruction::new(&[0x74, 0x38]), SegmentSizes::Cs16Ss16),
        (Instruction::new(&[0x89, 0x46, 0x58]), SegmentSizes::Cs16Ss16),
        (Instruction::new(&[0xb8, 0x12, 0x00]), SegmentSizes::Cs16Ss16),
        (Instruction::new(&[0x40]), SegmentSizes::Cs16Ss16),
        (Instruction::new(&[0xC3]), SegmentSizes::Cs16Ss16),
        (Instruction::new(&[0x54]), SegmentSizes::Cs16Ss16),
        (Instruction::new(&[0x59]), SegmentSizes::Cs16Ss16),
        (
            Instruction::new(&[0x0f, 0x84, 0x22, 0x33, 0x11, 0x44]),
            SegmentSizes::Cs32Ss32,
        ),
        (Instruction::new(&[0xe8, 0x12, 0x34, 0x56, 0x78]), SegmentSizes::Cs32Ss32),
    ]
    .map(|(instr, segment_sizes)| {
        (
            instr_semantics
                .get(
                    decoder
                        .lookup(instr, segment_sizes)
                        .unwrap_or_else(|e| panic!("unable to decode {instr:X}: {e:?}")),
                )
                .clone(),
            instr,
            segment_sizes,
        )
    });

    for (encoding, instr, segment_sizes) in [(
        empty_encoding.borrow_encoding(),
        Instruction::new(&[0x00]),
        SegmentSizes::Cs32Ss32,
    )]
    .into_iter()
    .chain(extra)
    {
        fn reset_state(cpu: &mut State) {
            cpu.set_gpreg(GpReg::Bp, 0x100);
            cpu.set_gpreg(GpReg::Sp, 0x100);
            cpu.set_gpreg(GpReg::Ip, 0x400);
            cpu.set_gpreg(GpReg::Cs, 0x1234);
            cpu.set_gpreg(GpReg::Ds, 0x1234);
            cpu.set_gpreg(GpReg::Ss, 0x1234);
            cpu.set_gpreg(GpReg::CsLimit, 0xffff_ffff);
            cpu.set_gpreg(GpReg::DsLimit, 0xffff_ffff);
            cpu.set_gpreg(GpReg::SsLimit, 0xffff_ffff);
            cpu.set_gpreg(GpReg::DsBase, 0);
        }

        // let mut jit = SingleEncodingJit::new(1);
        // let hw = Hw::new(
        //     shm.clone(),
        //     mem.clone(),
        //     Vec::new(),
        //     channel().0,
        //     channel().1,
        //     Arc::new(Shm::new("vgabios", 16))
        // );
        // let mut ctx = ExecutionContext::new(hw, &mem, None, true, 0);
        // let mut cpu = State::default();
        // c.bench_function(
        //     &format!("jit_old:{}_{instr:X}", encoding.semantics.name),
        //     |bencher| {
        //         let base_instr = encoding.try_extract_base_instr(instr).unwrap();
        //         let part_values = encoding.extract_parts(&base_instr);
        //         bencher.iter(|| {
        //             reset_state(&mut cpu);
        //             let result = black_box(jit.execute_or_build(
        //                 0,
        //                 || &encoding,
        //                 instr.byte_len(),
        //                 &part_values,
        //                 &mut cpu,
        //                 &mut ctx,
        //             ));

        //             assert!(result, "{cpu}");
        //         })
        //     },
        // );

        let cranelift = CraneliftBackend::new(true);
        let mut jit = SingleEncodingExecution::new(cranelift, 1);
        c.bench_function(&format!("jit_cranelift:{}_{instr:X}", encoding.semantics.name), |bencher| {
            let base_instr = encoding.try_extract_base_instr(instr).unwrap();
            let part_values = encoding.semantics.part_packing.pack(&encoding.extract_parts(&base_instr));
            let f = jit.get_or_build(0, encoding, false, segment_sizes, || ());
            bencher.iter(|| {
                reset_state(emulator_ctx.emulator().cpu_mut());
                let result = f.execute_uninstantiated(emulator_ctx.emulator(), instr.byte_len() as u8, part_values, |_| ());

                assert!(result.can_continue_execution(), "{}", emulator_ctx.emulator().cpu());
            })
        });

        let inkwell_ctx = InkwellContext::new();
        let inkwell = InkwellBackend::new(&inkwell_ctx);
        let mut jit = SingleEncodingExecution::new(inkwell, 1);
        c.bench_function(&format!("jit_inkwell:{}_{instr:X}", encoding.semantics.name), |bencher| {
            let base_instr = encoding.try_extract_base_instr(instr).unwrap();
            let part_values = encoding.semantics.part_packing.pack(&encoding.extract_parts(&base_instr));
            let f = jit.get_or_build(0, encoding, false, segment_sizes, || ());
            bencher.iter(|| {
                reset_state(emulator_ctx.emulator().cpu_mut());
                let result = f.execute_uninstantiated(&mut emulator_ctx.emulator(), instr.byte_len() as u8, part_values, |_| ());

                assert!(result.can_continue_execution(), "{}", emulator_ctx.emulator().cpu());
            })
        });
    }
}

criterion_group! {
    name = benches;
    config = Criterion::default().measurement_time(Duration::from_secs(10));
    targets = bench_empty, bench_exec, bench_interrupt, // bench_decode
}
criterion_main!(benches);
