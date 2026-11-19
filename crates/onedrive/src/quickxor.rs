//! QuickXorHash, the one checksum OneDrive gives for files of every kind of account (personal
//! and work or school): each byte XORed into a 160-bit ring, 11 bits further on than the one
//! before, then the length (64 bits, little-endian) XORed into the last 8 of its 20 bytes.
//! A port of Microsoft's C# reference.
//! <https://learn.microsoft.com/onedrive/developer/code-snippets/quickxorhash>

use base64::Engine;

const WIDTH: usize = 160;
const SHIFT: usize = 11;
const LAST_CELL_BITS: usize = 32;

#[derive(Clone, Default)]
pub struct QuickXor {
    data: [u64; 3],
    length: u64,
    shift: usize,
}

impl QuickXor {
    pub fn new() -> QuickXor {
        QuickXor::default()
    }

    pub fn update(&mut self, bytes: &[u8]) {
        let mut cell = self.shift / 64;
        let mut offset = self.shift % 64;
        for i in 0..bytes.len().min(WIDTH) {
            let last = cell == self.data.len() - 1;
            let bits = if last { LAST_CELL_BITS } else { 64 };
            // Every WIDTH-th byte from i lands on the same bits: XORed together first.
            let mut x = 0u8;
            let mut j = i;
            while j < bytes.len() {
                x ^= bytes[j];
                j += WIDTH;
            }
            if offset <= bits - 8 {
                self.data[cell] ^= (x as u64) << offset;
            } else {
                let next = if last { 0 } else { cell + 1 };
                self.data[cell] ^= (x as u64) << offset;
                self.data[next] ^= (x as u64) >> (bits - offset);
            }
            offset += SHIFT;
            while offset >= bits {
                let was_last = cell == self.data.len() - 1;
                offset -= if was_last { LAST_CELL_BITS } else { 64 };
                cell = if was_last { 0 } else { cell + 1 };
            }
        }
        self.shift = (self.shift + SHIFT * (bytes.len() % WIDTH)) % WIDTH;
        self.length += bytes.len() as u64;
    }

    pub fn finish(&self) -> [u8; 20] {
        let mut out = [0u8; 20];
        out[..8].copy_from_slice(&self.data[0].to_le_bytes());
        out[8..16].copy_from_slice(&self.data[1].to_le_bytes());
        out[16..].copy_from_slice(&self.data[2].to_le_bytes()[..4]);
        for (i, b) in self.length.to_le_bytes().iter().enumerate() {
            out[12 + i] ^= b;
        }
        out
    }

    /// As OneDrive writes it: base64.
    pub fn base64(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(self.finish())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hash by its definition, bit by bit: bit `b` of byte `i` toggles bit
    /// `(11 i + b) mod 160` of the ring.
    fn by_definition(data: &[u8]) -> [u8; 20] {
        let mut ring = [false; WIDTH];
        for (i, byte) in data.iter().enumerate() {
            for b in 0..8 {
                if byte >> b & 1 == 1 {
                    let at = (SHIFT * i + b) % WIDTH;
                    ring[at] = !ring[at];
                }
            }
        }
        let mut out = [0u8; 20];
        for (at, on) in ring.iter().enumerate() {
            if *on {
                out[at / 8] |= 1 << (at % 8);
            }
        }
        for (i, b) in (data.len() as u64).to_le_bytes().iter().enumerate() {
            out[12 + i] ^= b;
        }
        out
    }

    fn noise(n: usize) -> Vec<u8> {
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    #[test]
    fn nothing_hashes_to_zeros() {
        assert_eq!(QuickXor::new().base64(), "AAAAAAAAAAAAAAAAAAAAAAAAAAA=");
    }

    #[test]
    fn it_matches_its_definition_whole_or_in_pieces() {
        let data = noise(5000);
        for len in [1, 7, 19, 20, 159, 160, 161, 333, 5000] {
            let mut h = QuickXor::new();
            h.update(&data[..len]);
            assert_eq!(h.finish(), by_definition(&data[..len]), "{len} bytes at once");
        }
        // Fed in uneven pieces, as a transfer reads.
        for step in [1, 3, 64, 159, 160, 1000] {
            let mut h = QuickXor::new();
            for piece in data.chunks(step) {
                h.update(piece);
            }
            assert_eq!(h.finish(), by_definition(&data), "pieces of {step}");
        }
    }
}
