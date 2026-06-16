use std::cmp::Reverse;
use std::collections::HashMap;
use std::fmt::{Debug, Display};
use std::marker::PhantomData;
use std::mem::take;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering, compiler_fence};
use std::sync::mpsc::channel;
use std::time::Duration;

pub trait EncodeU64 {
    fn encode(&self) -> u64;
    fn decode(val: u64) -> Self;
}

pub struct Profiler<T> {
    current: Arc<AtomicU64>,
    running: Arc<AtomicBool>,
    _phantom: PhantomData<fn() -> T>,
    need_snapshot: Arc<AtomicBool>,
    incoming_snapshots: std::sync::mpsc::Receiver<HashMap<u64, u64>>,
}

impl<T: EncodeU64> Profiler<T> {
    pub fn new(default: &T) -> Self {
        let need_snapshot = Arc::new(AtomicBool::new(false));
        let running = Arc::new(AtomicBool::new(true));
        let current = Arc::new(AtomicU64::new(default.encode()));
        let (snapshot_sender, incoming_snapshots) = channel();

        let running_copy = running.clone();
        let current_copy = current.clone();
        let need_snapshot_copy = need_snapshot.clone();
        std::thread::Builder::new()
            .name(String::from("profiler-thread"))
            .spawn(move || {
                let mut counts = HashMap::new();
                while running_copy.load(Ordering::Relaxed) {
                    let current = current_copy.load(Ordering::Relaxed);
                    *counts.entry(current).or_insert(0u64) += 1;

                    if need_snapshot_copy
                        .compare_exchange(true, false, Ordering::Relaxed, Ordering::Relaxed)
                        .is_ok()
                    {
                        snapshot_sender.send(take(&mut counts)).unwrap();
                    }

                    std::thread::sleep(Duration::from_micros(10));
                }
            })
            .expect("thread should spawn");

        Self {
            current,
            running,
            need_snapshot,
            incoming_snapshots,
            _phantom: PhantomData,
        }
    }

    pub fn at<const PROFILE_ENABLED: bool>(&mut self, state: &T) {
        if PROFILE_ENABLED {
            let val = state.encode();

            // Make sure the compiler doesn't move this store to a different location in the program.
            compiler_fence(Ordering::SeqCst);

            self.current.store(val, Ordering::Relaxed);

            // Make sure the compiler doesn't move this store to a different location in the program.
            compiler_fence(Ordering::SeqCst);
        }
    }

    pub fn snapshot(&mut self) -> Snapshot<T> {
        while self
            .need_snapshot
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            std::thread::sleep(Duration::from_micros(100));
        }

        let snapshot = self.incoming_snapshots.recv().unwrap();
        Snapshot {
            data: snapshot,
            _phantom: PhantomData,
        }
    }
}

struct Percent(f64);

impl Display for Percent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:>5.2}%", self.0)
    }
}

pub struct Snapshot<T> {
    data: HashMap<u64, u64>,
    _phantom: PhantomData<T>,
}

impl<'a, T: Debug + EncodeU64> Debug for Snapshot<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let total = self.data.values().sum::<u64>();

        let mut values = self.data.iter().map(|(&x, &y)| (x, y)).collect::<Vec<_>>();
        values.sort_by_key(|(_, count)| Reverse(*count));

        for (key, count) in values {
            let key: T = T::decode(key);
            write!(f, "{} - ", Percent(count as f64 / total as f64 * 100.))?;
            Debug::fmt(&key, f)?;
            writeln!(f)?;
        }

        Ok(())
    }
}

impl<T> Drop for Profiler<T> {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
    }
}
