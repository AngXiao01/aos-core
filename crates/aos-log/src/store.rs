use alloc::vec::Vec;

use aos_types::ContentHash;

use crate::entry::LogEntry;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreError {
    Seq,
    Hash,
    Prev,
    ChunkingUnsupported,
}

pub trait LogStore {
    fn append(&mut self, entry: LogEntry) -> Result<(), StoreError>;
    fn len(&self) -> usize;
    fn get(&self, index: usize) -> Option<LogEntry>;
    fn tip_hash(&self) -> [u8; 32];

    fn entries(&self) -> Vec<LogEntry> {
        let mut out = Vec::with_capacity(self.len());
        for i in 0..self.len() {
            if let Some(e) = self.get(i) {
                out.push(e);
            }
        }
        out
    }
}

#[derive(Clone, Debug)]
pub struct ChunkRef {
    pub hash: ContentHash,
    pub length: u64,
}

pub trait BlobStore {
    fn put(&mut self, bytes: &[u8]) -> ContentHash;
    fn get(&self, hash: &ContentHash) -> Option<Vec<u8>>;
    fn remove(&mut self, hash: &ContentHash) -> bool;
    fn list_hashes(&self) -> Vec<ContentHash>;

    /// FastCDC is intentionally unsupported in this crate.
    fn put_chunked(&mut self, _chunks: &[ChunkRef]) -> Result<ContentHash, StoreError> {
        Err(StoreError::ChunkingUnsupported)
    }
}

#[derive(Clone, Debug, Default)]
pub struct MemoryLog {
    entries: Vec<LogEntry>,
}

impl LogStore for MemoryLog {
    fn append(&mut self, entry: LogEntry) -> Result<(), StoreError> {
        if entry.seq != self.entries.len() as u64 {
            return Err(StoreError::Seq);
        }
        if entry.prev_hash != self.tip_hash() {
            return Err(StoreError::Prev);
        }
        if crate::entry::hash_entry(&entry) != Ok(entry.hash) {
            return Err(StoreError::Hash);
        }
        self.entries.push(entry);
        Ok(())
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn get(&self, index: usize) -> Option<LogEntry> {
        self.entries.get(index).cloned()
    }

    fn tip_hash(&self) -> [u8; 32] {
        self.entries.last().map(|e| e.hash).unwrap_or([0u8; 32])
    }
}

#[derive(Clone, Debug, Default)]
pub struct MemoryBlob {
    items: alloc::collections::BTreeMap<ContentHash, Vec<u8>>,
}

impl BlobStore for MemoryBlob {
    fn put(&mut self, bytes: &[u8]) -> ContentHash {
        let hash = ContentHash::of(bytes);
        self.items.entry(hash).or_insert_with(|| bytes.to_vec());
        hash
    }

    fn get(&self, hash: &ContentHash) -> Option<Vec<u8>> {
        self.items.get(hash).cloned()
    }

    fn remove(&mut self, hash: &ContentHash) -> bool {
        self.items.remove(hash).is_some()
    }

    fn list_hashes(&self) -> Vec<ContentHash> {
        self.items.keys().copied().collect()
    }
}
