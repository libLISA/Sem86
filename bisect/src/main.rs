use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{BufRead, BufReader, Cursor, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::thread::spawn;
use std::time::Duration;

use clap::Parser;
use image::{EncodableLayout, ImageFormat, Rgba};
use rand::Rng;

#[derive(Clone, Debug, Parser)]
struct Args {
    #[clap(long)]
    min: usize,

    #[clap(long)]
    max: usize,

    binary: PathBuf,

    params: Vec<String>,

    #[clap(long)]
    nonzero_exit_is_bad: bool,

    #[clap(long)]
    invert: bool,

    #[clap(long)]
    classify_from_screenshot: Option<PathBuf>,

    #[clap(long)]
    skip_initial_classification: bool,

    #[clap(long)]
    screenshot_mask_taskbar_clock: bool,

    #[clap(long)]
    keep_history: bool,
}

impl Args {
    pub fn spawn_child(&self, num: usize) -> Child {
        let mut child = Command::new(&self.binary)
            .args(self.params.iter().map(|p| if p == "#" { num.to_string() } else { p.clone() }))
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();

        // Thread to read stdout
        if let Some(stdout) = child.stdout.take() {
            spawn(move || {
                let reader = BufReader::new(stdout);
                for line in reader.lines().map_while(Result::ok) {
                    println!("{}", line);
                }
            });
        }

        // Thread to read stderr
        if let Some(stderr) = child.stdout.take() {
            spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines().map_while(Result::ok) {
                    eprintln!("{}", line);
                }
            });
        }

        child
    }
}

#[derive(Copy, Clone, Debug)]
enum Judgement {
    Good,
    Bad,
}

impl Judgement {
    fn invert(&self, invert: bool) -> Judgement {
        match (invert, self) {
            (false, Judgement::Good) | (true, Judgement::Bad) => Judgement::Good,
            (false, Judgement::Bad) | (true, Judgement::Good) => Judgement::Bad,
        }
    }
}

pub fn clear_screen() {
    println!();
    println!("\x1B[H\x1B[2J\x1B[3J");
}

fn main() {
    let args = Args::parse();
    let (mut min, mut max) = (args.min, args.max);
    let (stdin_sender, stdin_recv) = channel();

    spawn(move || {
        let stdin = std::io::stdin();
        let mut line = String::new();
        while let Ok(n) = stdin.read_line(&mut line)
            && n > 0
        {
            stdin_sender.send(line.clone()).unwrap();
            line.clear();
        }
    });

    println!("Command: {:?} with parameters {:?}", args.binary, args.params);

    let mut screenshot_classification = args.classify_from_screenshot.as_ref().map(|screenshot_path| {
        let mut judgements = HashMap::new();
        if !args.skip_initial_classification {
            args.spawn_child(min).wait().unwrap();
            let (bad, _) = compute_screenshot_hashcode(screenshot_path, args.screenshot_mask_taskbar_clock);

            args.spawn_child(max).wait().unwrap();
            let (good, _) = compute_screenshot_hashcode(screenshot_path, args.screenshot_mask_taskbar_clock);

            assert_ne!(bad, good, "screenshots between bad and good state should differ");

            judgements.insert(bad, Judgement::Bad);
            judgements.insert(good, Judgement::Good);
        }

        (judgements, screenshot_path)
    });

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })
    .expect("Error setting Ctrl-C handler");

    let mut pick_random = false;

    'outer: while max > min + 1 {
        let mid = if pick_random {
            pick_random = false;
            rand::rng().random_range(min..max)
        } else {
            min + (max - min) / 2
        };
        let steps_left = ((max - min) as f64).log2();

        println!("Trying {mid} (between {min} and {max}, estimated steps left: {steps_left:.1})...");
        let mut last_capture = None;
        let mut child = args.spawn_child(mid);
        let mut child_exited = false;
        let mut last_code = 0;
        let judgement = loop {
            match stdin_recv.recv_timeout(Duration::from_millis(100)) {
                Ok(line) => match line.trim() {
                    "good" => break Judgement::Good,
                    "bad" => break Judgement::Bad,
                    "unknown" => {
                        pick_random = true;
                        continue 'outer
                    },
                    "exit" => {
                        child.kill().unwrap();
                        break 'outer
                    },
                    other => println!("unknown command: {other:?}"),
                },
                Err(RecvTimeoutError::Disconnected) => {
                    child.kill().unwrap();
                    break 'outer
                },
                Err(RecvTimeoutError::Timeout) => (),
            }

            if !running.load(Ordering::SeqCst) {
                println!("\rType 'good' or 'bad' to classify, or 'unknown' to re-try with a randomized k");
                child.kill().unwrap();
                running.store(true, Ordering::SeqCst);
            }

            if !child_exited && let Ok(Some(status)) = child.try_wait() {
                child_exited = true;
                println!("Exit status: success={}, code={:?}", status.success(), status.code());

                if args.nonzero_exit_is_bad {
                    break if status.success() { Judgement::Good } else { Judgement::Bad }
                }

                if let Some((map, screenshot_path)) = screenshot_classification.as_mut() {
                    let (code, sixel) = compute_screenshot_hashcode(screenshot_path, args.screenshot_mask_taskbar_clock);
                    last_code = code;
                    // assert!(!sixel.is_empty());

                    last_capture = Some(sixel);
                    if let Some(judgement) = map.get(&code) {
                        break judgement.invert(args.invert)
                    } else {
                        println!("Screenshot does not correspond to captured good and bad states. Please classify manually");
                    }
                }
            }
        };

        child.kill().unwrap();

        if !args.keep_history {
            clear_screen();

            if let Some(sixel) = last_capture.as_ref() {
                std::io::stdout().lock().write_all(sixel).unwrap();
                std::io::stdout().lock().flush().unwrap();
            }
        }

        if let Some((map, _)) = screenshot_classification.as_mut() {
            map.entry(last_code).or_insert(judgement);
        }

        println!("Judgement: {judgement:?}");

        if let Some((_, screenshot_path)) = screenshot_classification.as_ref() {
            std::fs::remove_file(screenshot_path).ok();
        }

        match judgement.invert(args.invert) {
            Judgement::Good => max = mid,
            Judgement::Bad => min = mid,
        }
    }

    println!("Final range: {min}..{max}");
}

fn compute_screenshot_hashcode(screenshot_path: &PathBuf, mask_taskbar_clock: bool) -> (u64, Vec<u8>) {
    let screenshot = image::open(screenshot_path).expect("screenshot should be present");
    let mut buf = screenshot.to_rgba8();

    if mask_taskbar_clock {
        for x in buf.width() - 96..buf.width() {
            for y in buf.height() - 64..buf.height() {
                buf.put_pixel(x, y, Rgba([0, 0, 0, 0]));
            }
        }
    }

    let mut hasher = DefaultHasher::new();
    buf.as_bytes().hash(&mut hasher);
    let code = hasher.finish();

    println!("Screenshot hashcode: {code} ({}x{})", buf.width(), buf.height());
    let mut sixel = Command::new("img2sixel")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    let mut png = Cursor::new(Vec::new());
    buf.write_to(&mut png, ImageFormat::Png).unwrap();
    png.flush().unwrap();
    sixel.stdin.take().unwrap().write_all(&png.into_inner()).unwrap();

    let sixel = sixel.wait_with_output().unwrap().stdout;
    std::io::stdout().lock().write_all(&sixel).unwrap();
    std::io::stdout().lock().flush().unwrap();

    (code, sixel)
}
