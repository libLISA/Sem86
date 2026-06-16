use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Debug;
use std::fs::File;
use std::io::Seek;
use std::ops::Range;
use std::os::unix::fs::FileExt;

use serde::{Deserialize, Serialize};

#[typetag::serde(tag = "backend_type")]
pub trait DiskDataBackendSnapshot {
    fn into_disk_data(&self, current: Option<&mut DiskData>) -> Box<dyn DiskDataBackend>;

    fn duplicate(&self) -> Box<dyn DiskDataBackendSnapshot>;
}

pub trait DiskDataBackend: Send {
    fn len(&self) -> u64;
    fn read(&self, range: std::ops::Range<usize>) -> Vec<u8>;
    fn write(&mut self, pos: u64, new_data: &[u8]);

    fn snapshot(&self) -> Box<dyn DiskDataBackendSnapshot>;
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct MemoryDiskData {
    data: Vec<u8>,
}

impl MemoryDiskData {
    pub fn new(data: Vec<u8>) -> Self {
        Self {
            data,
        }
    }
}

impl DiskDataBackend for MemoryDiskData {
    fn len(&self) -> u64 {
        self.data.len() as u64
    }

    fn read(&self, range: std::ops::Range<usize>) -> Vec<u8> {
        self.data[range].to_vec()
    }

    fn write(&mut self, pos: u64, new_data: &[u8]) {
        self.data[pos as usize..pos as usize + new_data.len()].copy_from_slice(new_data)
    }

    fn snapshot(&self) -> Box<dyn DiskDataBackendSnapshot> {
        Box::new(self.clone()) as _
    }
}

#[typetag::serde]
impl DiskDataBackendSnapshot for MemoryDiskData {
    fn into_disk_data(&self, _current: Option<&mut DiskData>) -> Box<dyn DiskDataBackend> {
        Box::new(self.clone()) as _
    }

    fn duplicate(&self) -> Box<dyn DiskDataBackendSnapshot> {
        Box::new(self.clone()) as _
    }
}

pub struct FileDiskData {
    file: RefCell<File>,
    len: u64,
}

impl FileDiskData {
    pub fn new(mut file: File) -> Self {
        file.seek(std::io::SeekFrom::End(0)).unwrap();
        let len = file.stream_position().unwrap();
        FileDiskData {
            file: RefCell::new(file),
            len,
        }
    }
}

impl DiskDataBackend for FileDiskData {
    fn len(&self) -> u64 {
        self.len
    }

    fn read(&self, range: std::ops::Range<usize>) -> Vec<u8> {
        let mut buf = vec![0; range.len()];
        self.file.borrow_mut().read_exact_at(&mut buf, range.start as u64).unwrap();
        buf
    }

    fn write(&mut self, pos: u64, new_data: &[u8]) {
        self.file.borrow_mut().write_all_at(new_data, pos).unwrap()
    }

    fn snapshot(&self) -> Box<dyn DiskDataBackendSnapshot> {
        Box::new(MissingDiskData {
            name: None,
        }) as _
    }
}

/// Uses the currently set disk data as the backend.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MissingDiskData {
    name: Option<String>,
}

#[typetag::serde]
impl DiskDataBackendSnapshot for MissingDiskData {
    fn into_disk_data(&self, current: Option<&mut DiskData>) -> Box<dyn DiskDataBackend> {
        current
            .map(|d| {
                let mut v = Box::new(self.clone()) as Box<_>;
                std::mem::swap(&mut d.backend, &mut v);
                v
            })
            .unwrap_or_else(|| Box::new(self.clone()) as Box<_>)
    }

    fn duplicate(&self) -> Box<dyn DiskDataBackendSnapshot> {
        Box::new(self.clone()) as _
    }
}

impl DiskDataBackend for MissingDiskData {
    fn len(&self) -> u64 {
        unimplemented!("missing disk data: {:?}", self.name)
    }

    fn read(&self, _range: std::ops::Range<usize>) -> Vec<u8> {
        unimplemented!("missing disk data: {:?}", self.name)
    }

    fn write(&mut self, _pos: u64, _new_data: &[u8]) {
        unimplemented!("missing disk data: {:?}", self.name)
    }

    fn snapshot(&self) -> Box<dyn DiskDataBackendSnapshot> {
        unimplemented!("missing disk data: {:?}", self.name)
    }
}

const SECTOR_SIZE: usize = 4096;

#[derive(Clone, Serialize, Deserialize)]
struct Sector(#[serde(with = "serde_big_array::BigArray")] [u8; SECTOR_SIZE]);

/// A Copy-on-write backend that doesn't modify the original disk, but instead stores changes in RAM.
pub struct CowDiskData {
    backing: Box<dyn DiskDataBackend>,
    sectors: HashMap<u64, Box<Sector>>,
    len: u64,
}

impl CowDiskData {
    pub fn new(backing: Box<dyn DiskDataBackend>) -> Self {
        let len = backing.len();
        Self {
            backing,
            sectors: HashMap::new(),
            len,
        }
    }

    fn read_sector(&self, sector: u64) -> [u8; SECTOR_SIZE] {
        if let Some(buf) = self.sectors.get(&sector) {
            return buf.0;
        }

        let start = sector as usize * SECTOR_SIZE;
        let end = start + SECTOR_SIZE;

        let mut buf = [0u8; SECTOR_SIZE];
        let data = self.backing.read(start..end);
        buf.copy_from_slice(&data);
        buf
    }

    fn get_sector_mut(&mut self, sector: u64) -> &mut [u8; SECTOR_SIZE] {
        if !self.sectors.contains_key(&sector) {
            let buf = self.read_sector(sector);
            self.sectors.insert(sector, Box::new(Sector(buf)));
        }

        &mut self.sectors.get_mut(&sector).unwrap().0
    }
}

impl DiskDataBackend for CowDiskData {
    fn len(&self) -> u64 {
        self.len
    }

    fn read(&self, range: Range<usize>) -> Vec<u8> {
        let mut out = Vec::with_capacity(range.len());

        let mut pos = range.start;
        while pos < range.end {
            let sector = pos as u64 / SECTOR_SIZE as u64;
            let offset = pos % SECTOR_SIZE;

            let chunk_len = (SECTOR_SIZE - offset).min(range.end - pos);

            let sector_data = if let Some(buf) = self.sectors.get(&sector) {
                &buf.0[..]
            } else {
                // Read directly from backing disk
                let start = sector as usize * SECTOR_SIZE;
                let end = start + SECTOR_SIZE;
                let tmp = self.backing.read(start..end);
                // Avoid allocation reuse complexity — copy
                out.extend_from_slice(&tmp[offset..offset + chunk_len]);
                pos += chunk_len;
                continue;
            };

            out.extend_from_slice(&sector_data[offset..offset + chunk_len]);

            pos += chunk_len;
        }

        out
    }

    fn write(&mut self, pos: u64, new_data: &[u8]) {
        let mut written = 0usize;

        while written < new_data.len() {
            let abs_pos = pos as usize + written;
            let sector = abs_pos as u64 / SECTOR_SIZE as u64;
            let offset = abs_pos % SECTOR_SIZE;

            let chunk_len = (SECTOR_SIZE - offset).min(new_data.len() - written);

            let sector_buf = self.get_sector_mut(sector);
            sector_buf[offset..offset + chunk_len].copy_from_slice(&new_data[written..written + chunk_len]);

            written += chunk_len;
        }
    }

    fn snapshot(&self) -> Box<dyn DiskDataBackendSnapshot> {
        Box::new(CowDiskDataSnapshot {
            backing: self.backing.snapshot(),
            sectors: self.sectors.clone(),
            len: self.len,
        })
    }
}

#[derive(Serialize, Deserialize)]
pub struct CowDiskDataSnapshot {
    backing: Box<dyn DiskDataBackendSnapshot>,
    sectors: HashMap<u64, Box<Sector>>,
    len: u64,
}

#[typetag::serde]
impl DiskDataBackendSnapshot for CowDiskDataSnapshot {
    fn into_disk_data(&self, current: Option<&mut DiskData>) -> Box<dyn DiskDataBackend> {
        Box::new(CowDiskData {
            backing: self.backing.into_disk_data(current),
            sectors: self.sectors.clone(),
            len: self.len,
        })
    }

    fn duplicate(&self) -> Box<dyn DiskDataBackendSnapshot> {
        Box::new(Self {
            backing: self.backing.duplicate(),
            sectors: self.sectors.clone(),
            len: self.len,
        })
    }
}

#[derive(Serialize, Deserialize)]
pub struct DiskDataSnapshot {
    backend: Box<dyn DiskDataBackendSnapshot>,
    is_cd: bool,
}

impl Debug for DiskDataSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskDataSnapshot")
            .field("backend", &"backend")
            .field("is_cd", &self.is_cd)
            .finish()
    }
}

impl Clone for DiskDataSnapshot {
    fn clone(&self) -> Self {
        Self {
            backend: self.backend.duplicate(),
            is_cd: self.is_cd,
        }
    }
}

pub struct DiskData {
    backend: Box<dyn DiskDataBackend>,
    is_cd: bool,
}

impl Default for DiskData {
    fn default() -> Self {
        Self::new(Box::new(MemoryDiskData::default()) as _)
    }
}

impl DiskData {
    pub fn new(backend: Box<dyn DiskDataBackend>) -> Self {
        Self {
            backend,
            is_cd: false,
        }
    }

    pub fn from_snapshot(snapshot: DiskDataSnapshot, current_data: Option<&mut DiskData>) -> Self {
        Self {
            backend: snapshot.backend.into_disk_data(current_data),
            is_cd: snapshot.is_cd,
        }
    }

    pub fn with_is_cd(self, is_cd: bool) -> Self {
        Self {
            is_cd,
            ..self
        }
    }

    pub fn len(&self) -> u64 {
        self.backend.len()
    }

    pub fn read_slice(&self, range: std::ops::Range<usize>) -> Vec<u8> {
        self.backend.read(range)
    }

    pub fn write_slice(&mut self, pos: u64, data: &[u8]) {
        self.backend.write(pos, data);
    }

    pub fn is_cd(&self) -> bool {
        self.is_cd
    }

    pub fn snapshot(&self) -> DiskDataSnapshot {
        DiskDataSnapshot {
            backend: self.backend.snapshot(),
            is_cd: self.is_cd,
        }
    }

    pub fn from_mem(data: Vec<u8>) -> DiskData {
        Self::new(Box::new(MemoryDiskData::new(data)) as _)
    }

    pub fn from_file(file: File) -> DiskData {
        Self::new(Box::new(FileDiskData::new(file)) as _)
    }
}

impl Debug for DiskData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<{} bytes>", self.backend.len())
    }
}
