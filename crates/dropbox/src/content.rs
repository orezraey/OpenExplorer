//! Dropbox's `content_hash`: the file split in blocks of 4 MiB, each block's SHA-256, and the
//! SHA-256 of those digests one after the other, in hex. Every file's metadata carries it, so a
//! transfer checks what arrived, and a file already on Dropbox with the same bytes isn't sent
//! again.

use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256};

pub const BLOCK: usize = 4 << 20;

/// Takes the bytes as they go by.
#[derive(Clone, Default)]
pub struct Hasher {
    whole: Sha256,
    block: Sha256,
    in_block: usize,
}

impl Hasher {
    pub fn update(&mut self, mut data: &[u8]) {
        while !data.is_empty() {
            let take = (BLOCK - self.in_block).min(data.len());
            self.block.update(&data[..take]);
            self.in_block += take;
            data = &data[take..];
            if self.in_block == BLOCK {
                let done = std::mem::take(&mut self.block).finalize();
                self.whole.update(done);
                self.in_block = 0;
            }
        }
    }

    pub fn finish(mut self) -> String {
        if self.in_block > 0 {
            let done = std::mem::take(&mut self.block).finalize();
            self.whole.update(done);
        }
        self.whole.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// The block digests of a run of bytes that starts where a block does, as they go by: a part of
/// a file fetched on its own, whose digests [`joined`] with the other parts' give the hash.
#[derive(Default)]
pub struct Blocks {
    digests: Vec<[u8; 32]>,
    block: Sha256,
    in_block: usize,
}

impl Blocks {
    pub fn update(&mut self, mut data: &[u8]) {
        while !data.is_empty() {
            let take = (BLOCK - self.in_block).min(data.len());
            self.block.update(&data[..take]);
            self.in_block += take;
            data = &data[take..];
            if self.in_block == BLOCK {
                self.digests.push(std::mem::take(&mut self.block).finalize().into());
                self.in_block = 0;
            }
        }
    }

    pub fn finish(mut self) -> Vec<[u8; 32]> {
        if self.in_block > 0 {
            self.digests.push(std::mem::take(&mut self.block).finalize().into());
        }
        self.digests
    }
}

/// The hash of a file from its blocks' digests, in order.
pub fn joined(digests: impl IntoIterator<Item = [u8; 32]>) -> String {
    let mut whole = Sha256::new();
    for d in digests {
        whole.update(d);
    }
    whole.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// The hash of what `src` reads, to its end. `stop` is asked between blocks.
pub fn of_reader(src: &mut dyn Read, stop: &dyn Fn() -> bool) -> std::io::Result<String> {
    let mut h = Hasher::default();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        if stop() {
            return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, crate::CANCELLED));
        }
        match src.read(&mut buf) {
            Ok(0) => return Ok(h.finish()),
            Ok(n) => h.update(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}

/// The hash of a file on this computer.
pub fn of_file(path: &Path, stop: &dyn Fn() -> bool) -> std::io::Result<String> {
    of_reader(&mut std::fs::File::open(path)?, stop)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(d: &[u8]) -> String {
        d.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// By the definition: the digest of the blocks' digests.
    fn by_definition(data: &[u8]) -> String {
        let mut all = Vec::new();
        for block in data.chunks(BLOCK) {
            all.extend_from_slice(&Sha256::digest(block));
        }
        hex(&Sha256::digest(&all))
    }

    #[test]
    fn the_hash_follows_its_definition_whatever_the_reads() {
        // An empty file has no block: the hash of nothing.
        assert_eq!(Hasher::default().finish(), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        let data: Vec<u8> = (0..(2 * BLOCK + 12_345)).map(|i| (i * 31 % 251) as u8).collect();
        for cut in [1usize, 1000, BLOCK - 1, BLOCK, BLOCK + 7] {
            let mut h = Hasher::default();
            for piece in data.chunks(cut) {
                h.update(piece);
            }
            assert_eq!(h.finish(), by_definition(&data), "pieces of {cut}");
        }
        // Exactly one block isn't followed by an empty one.
        assert_eq!(of_reader(&mut &data[..BLOCK], &|| false).unwrap(), by_definition(&data[..BLOCK]));
        // It's not the plain SHA-256 of the bytes once there's more than one block.
        assert_ne!(by_definition(&data), hex(&Sha256::digest(&data)));
    }
}
