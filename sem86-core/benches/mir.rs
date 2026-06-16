use std::io::{BufReader, Cursor};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use liblisa::Instruction;
use lz4_flex::frame::FrameDecoder;
use sem86_core::SegmentSizes;
use sem86_core::codegen::mir::{EncodingEntry, MirBuilder};
use sem86_core::decoder::{Decoder, EncodingLookup, PackedInstrSem};

fn bench_chain(
    c: &mut Criterion, name: &str, instrs: &str, semantics: &PackedInstrSem, decoder: &mut Decoder<PackedInstrSem>,
    segment_sizes: SegmentSizes,
) {
    let mut entries = Vec::new();
    for instr in instrs.split(' ') {
        let instr = Instruction::from_str(instr).unwrap();
        let encoding_index = decoder.lookup(instr, segment_sizes).unwrap();
        let encoding_ref = semantics.get(encoding_index);
        let part_values = encoding_ref.semantics.part_packing.pack(&encoding_ref.extract_parts(&instr));

        entries.push(EncodingEntry {
            instr: Some(instr),
            instr_len: instr.byte_len(),
            encoding: encoding_ref,
            part_values,
            is_cs32: segment_sizes.is_cs32(),
            metadata: None,
        });
    }

    c.bench_function(name, |b| {
        b.iter(|| black_box(MirBuilder::build_from_sequence(true, &entries)));
    });
}

fn bench_chains(c: &mut Criterion) {
    test_log::env_logger::try_init().ok();

    let f = BufReader::new(Cursor::new(include_bytes!("../../x86.semantics")));
    let f = FrameDecoder::new(f);
    let instr_semantics: PackedInstrSem = pot::from_reader(f).unwrap();
    let instr_semantics = Arc::new(instr_semantics);
    let mut decoder = Decoder::new(instr_semantics.clone());

    bench_chain(
        c,
        "x87-chain",
        "55 8bec 51 d94508 d9c0 d9e0 d95dfc d9450c d9c0 d9e0 d95d08 d94510 d9e0 d95d0c d94110 d94508 d9c0 deca d901 d945fc d9c0 deca d9cb dec1 d94120 d9450c d9c0 deca d9ca dec1 d84130 d95930 d94114 d9c2 dec9 d9c3 d84904 dec1 d94124 d9c2 dec9 dec1 d84134 d95934 d94108 d9c3 dec9 d9c2 d84918 dec1 d94128 d9c2 dec9 dec1 d84138 d95938 d9411c deca d9410c decb d9c9 dec2 d8492c dec1 d8413c d95d08 d94508 d9513c d9410c d9c3 dec9 d801 d919 d9411c d9c3 dec9 d84110 d95910 d9412c d9c3 dec9 d84120 d95920 d9c0 decb 8bc1 d94130 dec3 d9ca d95930 d9410c d9c1 dec9 d84104 d95904 d9411c d9c1 dec9 d84114 d95914 d9412c d9c1 dec9 d84124 d95924 dec9 d84134 d95934 d9410c d94510 d9c0 deca d94108 dec2 d9c9 d95908 d9411c d9c1 dec9 d84118 d95918 d9412c d9c1 dec9 d84128 d95928 d8493c d84138 d95938 8be5 5d c20c00",
        &instr_semantics,
        &mut decoder,
        SegmentSizes::Cs32Ss32,
    );
}

criterion_group! {
    name = benches;
    config = Criterion::default().measurement_time(Duration::from_secs(15));
    targets = bench_chains,
}
criterion_main!(benches);
