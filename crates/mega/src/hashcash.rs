//! The proof of work MEGA asks of a login (HTTP 402 with an `X-Hashcash` header): find a
//! 4-byte prefix that, put before the challenge's 48 bytes repeated 262 144 times (12 MB),
//! makes a SHA-256 whose first word is at most a threshold. As MEGA's SDK does
//! (`src/hashcash.cpp`), every core searches its share of the prefixes.

use std::sync::atomic::{AtomicBool, Ordering};

use sha2::{Digest, Sha256};

use crate::crypto::{b64, unb64};

const TOKEN: usize = 48;
const REPEAT: usize = 262_144;

/// The challenge of an `X-Hashcash` header, `1:<easiness>:<time>:<token>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Challenge {
    pub easiness: u8,
    pub token: String,
}

impl Challenge {
    pub fn parse(header: &str) -> Option<Challenge> {
        let mut parts = header.trim().split(':');
        let (version, easiness, _time, token) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
        if version != "1" || token.len() != 64 {
            return None;
        }
        let easiness = easiness.parse::<u8>().ok()?;
        Some(Challenge { easiness, token: token.to_string() })
    }

    /// The highest first word a hash may have.
    pub fn threshold(&self) -> u32 {
        let e = u32::from(self.easiness);
        (((e & 63) << 1) + 1) << ((e >> 6) * 7 + 3)
    }

    /// The header the request goes again with: `1:<token>:<prefix>`. `None` when the token isn't
    /// 48 bytes, or `stop` turned true first.
    pub fn solve(&self, stop: &AtomicBool) -> Option<String> {
        let token = unb64(&self.token).ok().filter(|t| t.len() == TOKEN)?;
        let mut area = vec![0u8; 4 + TOKEN * REPEAT];
        for chunk in area[4..].as_chunks_mut::<TOKEN>().0 {
            chunk.copy_from_slice(&token);
        }
        let threshold = self.threshold();
        let workers = std::thread::available_parallelism().map_or(1, |n| n.get()).clamp(1, 16) as u32;
        let found = AtomicBool::new(false);
        let prefix = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..workers)
                .map(|w| {
                    let (area, found) = (&area, &found);
                    scope.spawn(move || {
                        let mut n = w;
                        while !found.load(Ordering::Relaxed) && !stop.load(Ordering::Relaxed) {
                            if passes(area, n, threshold) {
                                found.store(true, Ordering::Relaxed);
                                return Some(n);
                            }
                            n = n.checked_add(workers)?;
                        }
                        None
                    })
                })
                .collect();
            handles.into_iter().filter_map(|h| h.join().ok().flatten()).next()
        })?;
        Some(format!("1:{}:{}", self.token, b64(&prefix.to_be_bytes())))
    }
}

/// Whether prefix `n` before `area`'s token (bytes 4 on) gives a hash within `threshold`.
fn passes(area: &[u8], n: u32, threshold: u32) -> bool {
    let mut first = [0u8; 64];
    first[..4].copy_from_slice(&n.to_be_bytes());
    first[4..].copy_from_slice(&area[4..64]);
    let mut h = Sha256::new();
    h.update(first);
    h.update(&area[64..]);
    let d = h.finalize();
    u32::from_be_bytes([d[0], d[1], d[2], d[3]]) <= threshold
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_header_and_its_threshold() {
        let c = Challenge::parse("1:192:1795015326:xesJvRRn8iANluTvpSMIfv1Y-s29opJC0CqJ9-f_ioGktv-azgYmR1qeQs7_NWw9").unwrap();
        assert_eq!(c.easiness, 192);
        // ((0 << 1) + 1) << (3 * 7 + 3): one hash in 256 passes.
        assert_eq!(c.threshold(), 1 << 24);
        assert_eq!(Challenge { easiness: 0, token: String::new() }.threshold(), 8);
        assert_eq!(Challenge::parse("2:192:1:abc"), None);
        assert_eq!(Challenge::parse("1:999:1:xesJvRRn8iANluTvpSMIfv1Y-s29opJC0CqJ9-f_ioGktv-azgYmR1qeQs7_NWw9"), None);
    }

    /// An easy challenge (threshold 127 << 24: about one try in two) solves, and the prefix it gives
    /// does pass when checked from scratch.
    #[test]
    fn a_solution_passes_the_check() {
        let token = b64(&[7u8; 48]);
        let c = Challenge { easiness: 255, token: token.clone() };
        let header = c.solve(&AtomicBool::new(false)).unwrap();
        let prefix = unb64(header.rsplit(':').next().unwrap()).unwrap();
        let mut h = Sha256::new();
        h.update(&prefix);
        for _ in 0..REPEAT {
            h.update([7u8; 48]);
        }
        let d = h.finalize();
        assert!(u32::from_be_bytes([d[0], d[1], d[2], d[3]]) <= c.threshold());
        assert!(header.starts_with(&format!("1:{token}:")));
    }

    /// A challenge MEGA sent, solved by a separate Python script (hashlib, prefixes counted up
    /// from 0) whose answer MEGA accepted: 48 is the first prefix that passes.
    #[test]
    fn the_first_passing_prefix_matches_an_accepted_answer() {
        let token = unb64("FWjd4yFLErQN4gIaM22vhqrlnA50IJeq3ZL0SigCGBXEDEnr-6-jMff6gGVn_rey").unwrap();
        let mut area = vec![0u8; 4 + TOKEN * REPEAT];
        for chunk in area[4..].as_chunks_mut::<TOKEN>().0 {
            chunk.copy_from_slice(&token);
        }
        let threshold = Challenge { easiness: 192, token: String::new() }.threshold();
        assert_eq!((0..=48).find(|&n| passes(&area, n, threshold)), Some(48));
    }

    #[test]
    fn a_stopped_search_gives_up() {
        let c = Challenge { easiness: 0, token: b64(&[1u8; 48]) };
        assert_eq!(c.solve(&AtomicBool::new(true)), None);
    }
}
