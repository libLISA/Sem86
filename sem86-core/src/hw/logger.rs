use bitcode::{Decode, Encode};
use log::info;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, Encode, Decode)]
pub struct Logger {
    buf: Vec<u8>,
}

impl Default for Logger {
    fn default() -> Self {
        Self::new()
    }
}

impl Logger {
    pub fn write(&mut self, val: u8) {
        if val == b'\n' || self.buf.len() >= 80 {
            info!("Incoming: {:?}", std::str::from_utf8(&self.buf));
            self.buf.clear();
        } else {
            self.buf.push(val)
        }
    }

    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
        }
    }

    pub fn snapshot(&self) -> Logger {
        self.clone()
    }

    pub fn restore(&mut self, logger: Logger) {
        *self = logger;
    }
}
