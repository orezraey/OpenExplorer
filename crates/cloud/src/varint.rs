//! The compact binary body MEGA's tree and B2's listings are kept in: lengths and numbers as
//! LEB128 varints (signed ones zigzagged, so small negative ones stay short), byte strings as a
//! length and the bytes. A reader that runs past the end or finds a count larger than what's
//! left refuses the body ([`Refusal::Malformed`]), never panics.

use crate::sealed::Refusal;

pub fn put_len(out: &mut Vec<u8>, mut n: u64) {
    loop {
        let b = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

pub fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_len(out, b.len() as u64);
    out.extend_from_slice(b);
}

/// Signed numbers zigzagged, so small negative ones stay short.
pub fn put_signed(out: &mut Vec<u8>, n: i64) {
    put_len(out, ((n << 1) ^ (n >> 63)) as u64);
}

/// Reads a body [`put_len`] and the others wrote, from `at` on.
pub struct Reader<'a> {
    pub data: &'a [u8],
    pub at: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Reader<'a> {
        Reader { data, at: 0 }
    }

    /// Whether everything was read.
    pub fn done(&self) -> bool {
        self.at == self.data.len()
    }

    pub fn take(&mut self, n: usize) -> Result<&'a [u8], Refusal> {
        let end = self.at.checked_add(n).filter(|&e| e <= self.data.len()).ok_or(Refusal::Malformed)?;
        let out = &self.data[self.at..end];
        self.at = end;
        Ok(out)
    }

    pub fn array<const N: usize>(&mut self) -> Result<[u8; N], Refusal> {
        Ok(self.take(N)?.try_into().expect("N bytes"))
    }

    pub fn byte(&mut self) -> Result<u8, Refusal> {
        Ok(self.take(1)?[0])
    }

    pub fn unsigned(&mut self) -> Result<u64, Refusal> {
        let mut n = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.byte()?;
            n |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Ok(n);
            }
        }
        Err(Refusal::Malformed)
    }

    pub fn signed(&mut self) -> Result<i64, Refusal> {
        let n = self.unsigned()?;
        Ok(((n >> 1) as i64) ^ -((n & 1) as i64))
    }

    /// A count of things at least `min` bytes each, checked against what's left.
    pub fn count(&mut self, min: usize) -> Result<usize, Refusal> {
        let n = usize::try_from(self.unsigned()?).map_err(|_| Refusal::Malformed)?;
        if n.saturating_mul(min) > self.data.len() - self.at {
            return Err(Refusal::Malformed);
        }
        Ok(n)
    }

    pub fn bytes(&mut self) -> Result<&'a [u8], Refusal> {
        let n = usize::try_from(self.unsigned()?).map_err(|_| Refusal::Malformed)?;
        self.take(n)
    }

    pub fn text(&mut self) -> Result<&'a str, Refusal> {
        std::str::from_utf8(self.bytes()?).map_err(|_| Refusal::Malformed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bytes the MEGA crate's own encoder wrote before it moved here: the format can't change.
    #[test]
    fn numbers_are_written_as_before_and_read_back() {
        let nums = [0i64, 1, -1, 63, -64, 300, -300, i64::MAX, i64::MIN];
        let mut out = Vec::new();
        for n in nums {
            put_signed(&mut out, n);
        }
        put_len(&mut out, u64::MAX);
        put_bytes(&mut out, "ação".as_bytes());
        let hex: String = out.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "0002017e7fd804d704feffffffffffffffff01ffffffffffffffffff01ffffffffffffffffff010661c3a7c3a36f");
        let mut r = Reader::new(&out);
        for n in nums {
            assert_eq!(r.signed().unwrap(), n);
        }
        assert_eq!(r.unsigned().unwrap(), u64::MAX);
        assert_eq!(r.text().unwrap(), "ação");
        assert!(r.done());
        assert_eq!(r.byte(), Err(Refusal::Malformed));
    }

    #[test]
    fn a_count_beyond_whats_left_is_refused() {
        let mut out = Vec::new();
        put_len(&mut out, 3);
        out.extend_from_slice(b"ab");
        assert_eq!(Reader::new(&out).count(1), Err(Refusal::Malformed));
        assert_eq!(Reader::new(&out).bytes(), Err(Refusal::Malformed));
        // Eleven bytes of continuation are no number.
        assert_eq!(Reader::new(&[0xff; 11]).unsigned(), Err(Refusal::Malformed));
    }
}
