use bitcode::{Decode, Encode};
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize, Encode, Decode)]
pub struct MemorySnapshot {
    pub(super) phys_mem: Vec<u8>,
}
