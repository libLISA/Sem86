use std::{fs::File, io::Write, path::PathBuf};
use clap::Parser;
use sem86_core::tracefile::TraceEntryReader;
use lz4_flex::frame::{FrameDecoder, FrameEncoder, FrameInfo};


#[derive(Clone, Debug, Parser)]
enum Cmd {
    Info,

    Optimize {
        output: PathBuf,
    }
}

#[derive(Clone, Debug, Parser)]
struct Args {
    file: PathBuf,

    #[clap(subcommand)]
    cmd: Cmd,
}

fn main() {
    let args = Args::parse();
    match args.cmd {
        Cmd::Info => {
            let f = File::open(args.file).unwrap();
            let f = FrameDecoder::new(f);
            let mut r = TraceEntryReader::new(f);
            println!("Reading tracefile...");

            let mut n = 0usize;
            while r.next(false).is_some() {
                n += 1;
                if n % 381_327 == 0 {
                    print!("\rEntries: {n} | {:.3}GiB", r.gb_read());
                    std::io::stdout().flush().unwrap();
                }
            }

            println!("\rTotal number of entries: {n} | {:.3}GiB", r.gb_read());
        },
        Cmd::Optimize { output } => {
            let input = File::open(args.file).unwrap();
            let mut input = FrameDecoder::new(input);
            let output = File::create(output).unwrap();
            let info = FrameInfo::new()
                .block_mode(lz4_flex::frame::BlockMode::Linked);
            let mut output = FrameEncoder::with_frame_info(info, output);

            std::io::copy(&mut input, &mut output).unwrap();
            output.finish().unwrap();
        }
    }
}
