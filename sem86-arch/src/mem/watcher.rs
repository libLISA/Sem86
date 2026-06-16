use std::fmt::Debug;

pub trait MemoryWatcher: Debug + Send + Sync {
    fn notify_dirty(&self, offset: u64);
}
