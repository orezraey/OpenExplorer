//! MEGA's client-side cryptography: everything in an account is encrypted before it leaves the
//! computer, so names, keys and file contents are worked out here.
//!
//! - The password gives the key that opens the account's master key: PBKDF2-HMAC-SHA512 for
//!   accounts made since 2018 (v2), 65 536 rounds of AES for older ones (v1).
//! - Every node (file or folder) has its own key, kept encrypted with the master key (AES-ECB),
//!   or with a share's key for what's in a shared folder. Attributes (the name) are AES-CBC
//!   with a zero IV under the node's key; file contents are AES-CTR, checked by a chained
//!   CBC-MAC over chunks.
//! - The session id comes encrypted with the account's RSA key.

use aes::Aes128;
use aes::cipher::{Array, BlockCipherDecrypt, BlockCipherEncrypt, KeyInit};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use num_bigint::BigUint;
use openexplorer_i18n::t;

use crate::Result;

/// MEGA's base64: URL-safe, without padding.
pub fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Reads MEGA's base64, also taking the standard alphabet and padding.
pub fn unb64(text: &str) -> Result<Vec<u8>> {
    let clean: String = text.trim().trim_end_matches('=').chars().map(|c| match c {
        '+' => '-',
        '/' => '_',
        c => c,
    }).collect();
    URL_SAFE_NO_PAD.decode(clean.as_bytes()).map_err(|_| t("O MEGA mandou dados em base64 inválidos.", "MEGA sent invalid base64 data.").into())
}

/// `n` random bytes from the operating system.
pub fn random<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    getrandom::fill(&mut out).expect("the operating system's random source");
    out
}

/// A 16-byte piece as the cipher takes it.
fn blk(b: &mut [u8]) -> &mut aes::Block {
    b.try_into().expect("a 16-byte block")
}

fn cipher(key: &[u8]) -> Aes128 {
    Aes128::new_from_slice(key).expect("AES-128 keys are 16 bytes")
}

/// AES-ECB over whole 16-byte blocks; a trailing partial block is left alone.
pub fn ecb_encrypt(key: &[u8], data: &mut [u8]) {
    let c = cipher(key);
    for block in data.as_chunks_mut::<16>().0 {
        c.encrypt_block(blk(block));
    }
}

pub fn ecb_decrypt(key: &[u8], data: &mut [u8]) {
    let c = cipher(key);
    for block in data.as_chunks_mut::<16>().0 {
        c.decrypt_block(blk(block));
    }
}

/// AES-CBC with a zero IV, over data padded to whole blocks.
pub fn cbc_encrypt(key: &[u8], data: &mut [u8]) {
    let c = cipher(key);
    let mut prev = [0u8; 16];
    for block in data.as_chunks_mut::<16>().0 {
        for (b, p) in block.iter_mut().zip(prev) {
            *b ^= p;
        }
        c.encrypt_block(blk(block));
        prev.copy_from_slice(block);
    }
}

pub fn cbc_decrypt(key: &[u8], data: &mut [u8]) {
    let c = cipher(key);
    let mut prev = [0u8; 16];
    for block in data.as_chunks_mut::<16>().0 {
        let this = *block;
        c.decrypt_block(blk(block));
        for (b, p) in block.iter_mut().zip(prev) {
            *b ^= p;
        }
        prev = this;
    }
}

/// AES-CTR as MEGA runs it over a file: the counter block is the file's 8-byte nonce followed
/// by the block number, big-endian. Bytes may come in any pieces, from any offset.
pub struct Ctr {
    cipher: Aes128,
    nonce: [u8; 8],
    /// The next byte's place in the file.
    pos: u64,
    /// The keystream of the block `pos` is in, when it's partway through one.
    stream: [u8; 16],
}

impl Ctr {
    /// The stream from byte `offset` of the file.
    pub fn new(key: &[u8; 16], nonce: &[u8; 8], offset: u64) -> Ctr {
        let mut c = Ctr { cipher: cipher(key), nonce: *nonce, pos: offset, stream: [0; 16] };
        if !offset.is_multiple_of(16) {
            c.stream = c.keystream(offset / 16);
        }
        c
    }

    fn keystream(&self, block: u64) -> [u8; 16] {
        let mut s = [0u8; 16];
        s[..8].copy_from_slice(&self.nonce);
        s[8..].copy_from_slice(&block.to_be_bytes());
        self.cipher.encrypt_block(blk(&mut s));
        s
    }

    /// Encrypts or decrypts (the same thing) the next bytes.
    pub fn apply(&mut self, data: &mut [u8]) {
        const AT_ONCE: usize = 8;
        let mut data = data;
        // The rest of a block begun by the last piece.
        let off = (self.pos % 16) as usize;
        if off != 0 {
            let n = (16 - off).min(data.len());
            for (b, k) in data[..n].iter_mut().zip(&self.stream[off..off + n]) {
                *b ^= k;
            }
            self.pos += n as u64;
            data = &mut data[n..];
        }
        let whole = data.len() - data.len() % 16;
        let (blocks, tail) = data.split_at_mut(whole);
        for group in blocks.chunks_mut(16 * AT_ONCE) {
            let count = group.len() / 16;
            let first = self.pos / 16;
            let mut stream = [Array::<u8, aes::cipher::consts::U16>::default(); AT_ONCE];
            for (i, s) in stream.iter_mut().take(count).enumerate() {
                s[..8].copy_from_slice(&self.nonce);
                s[8..].copy_from_slice(&(first + i as u64).to_be_bytes());
            }
            self.cipher.encrypt_blocks(&mut stream[..count]);
            for (chunk, s) in group.chunks_mut(16).zip(&stream) {
                for (b, k) in chunk.iter_mut().zip(s.iter()) {
                    *b ^= k;
                }
            }
            self.pos += group.len() as u64;
        }
        if !tail.is_empty() {
            self.stream = self.keystream(self.pos / 16);
            for (b, k) in tail.iter_mut().zip(&self.stream) {
                *b ^= k;
            }
            self.pos += tail.len() as u64;
        }
    }
}

/// Where each of a file's MAC chunks starts: 128 KB, 256 KB, … up to 1 MB, then every 1 MB.
pub fn chunk_len(index: u64) -> u64 {
    (index + 1).min(8) * 128 * 1024
}

/// The chained CBC-MAC MEGA keeps of a file's plaintext: a MAC per chunk (CBC from the IV
/// nonce‖nonce), each folded into the file's MAC, condensed at the end to 8 bytes. Bytes may come
/// in any pieces.
pub struct Mac {
    cipher: Aes128,
    iv: [u8; 16],
    /// The MAC of the chunk under way, and how far into it we are.
    chunk: [u8; 16],
    index: u64,
    left: u64,
    /// The bytes of a 16-byte block still being filled.
    partial: [u8; 16],
    filled: usize,
    /// The file's MAC so far (CBC with a zero IV over the chunk MACs).
    file: [u8; 16],
    started: bool,
}

impl Mac {
    pub fn new(key: &[u8; 16], nonce: &[u8; 8]) -> Mac {
        let mut iv = [0u8; 16];
        iv[..8].copy_from_slice(nonce);
        iv[8..].copy_from_slice(nonce);
        Mac { cipher: cipher(key), iv, chunk: iv, index: 0, left: chunk_len(0), partial: [0; 16], filled: 0, file: [0; 16], started: false }
    }

    fn block(&mut self, block: &[u8; 16]) {
        for (c, b) in self.chunk.iter_mut().zip(block) {
            *c ^= b;
        }
        self.cipher.encrypt_block(blk(&mut self.chunk));
    }

    fn end_chunk(&mut self) {
        if self.filled > 0 {
            let mut last = [0u8; 16];
            last[..self.filled].copy_from_slice(&self.partial[..self.filled]);
            self.block(&last);
            self.filled = 0;
        }
        for (f, c) in self.file.iter_mut().zip(self.chunk) {
            *f ^= c;
        }
        self.cipher.encrypt_block(blk(&mut self.file));
        self.index += 1;
        self.left = chunk_len(self.index);
        self.chunk = self.iv;
        self.started = false;
    }

    pub fn update(&mut self, mut data: &[u8]) {
        while !data.is_empty() {
            let take = (self.left.min(data.len() as u64)) as usize;
            let (now, rest) = data.split_at(take);
            data = rest;
            self.started |= take > 0;
            let mut now = now;
            if self.filled > 0 {
                let n = (16 - self.filled).min(now.len());
                self.partial[self.filled..self.filled + n].copy_from_slice(&now[..n]);
                self.filled += n;
                now = &now[n..];
                if self.filled == 16 {
                    let full = self.partial;
                    self.block(&full);
                    self.filled = 0;
                }
            }
            let whole = now.len() - now.len() % 16;
            for b in now[..whole].as_chunks::<16>().0 {
                self.block(b);
            }
            let tail = &now[whole..];
            if !tail.is_empty() {
                self.partial[..tail.len()].copy_from_slice(tail);
                self.filled = tail.len();
            }
            self.left -= take as u64;
            if self.left == 0 {
                self.end_chunk();
            }
        }
    }

    /// The condensed MAC: the file MAC's two halves of two words each, XORed.
    pub fn finish(mut self) -> [u8; 8] {
        if self.started || self.filled > 0 {
            self.end_chunk();
        }
        let m = self.file;
        let mut out = [0u8; 8];
        for i in 0..4 {
            out[i] = m[i] ^ m[i + 4];
            out[i + 4] = m[i + 8] ^ m[i + 12];
        }
        out
    }
}

/// A file node's 32-byte key as MEGA stores it: the AES key XORed with the nonce and condensed
/// MAC, then the nonce and MAC.
pub fn merge_file_key(key: &[u8; 16], nonce: &[u8; 8], mac: &[u8; 8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[16..24].copy_from_slice(nonce);
    out[24..].copy_from_slice(mac);
    for i in 0..16 {
        out[i] = key[i] ^ out[16 + i];
    }
    out
}

/// A file node's AES key, nonce and condensed MAC, from its 32-byte key.
pub fn split_file_key(k: &[u8]) -> ([u8; 16], [u8; 8], [u8; 8]) {
    let mut key = [0u8; 16];
    for i in 0..16 {
        key[i] = k[i] ^ k[16 + i];
    }
    (key, k[16..24].try_into().expect("8 bytes"), k[24..32].try_into().expect("8 bytes"))
}

/// The key a node's attributes are encrypted with: a folder's key as it is, a file's folded.
pub fn attr_key(k: &[u8]) -> [u8; 16] {
    if k.len() >= 32 { split_file_key(k).0 } else { k[..16].try_into().expect("16 bytes") }
}

/// Attributes as MEGA keeps them: `MEGA{"n":"name",…}` padded with zeros, AES-CBC.
pub fn encrypt_attrs(key: &[u8; 16], attrs: &serde_json::Map<String, serde_json::Value>) -> String {
    let mut buf = b"MEGA".to_vec();
    buf.extend(serde_json::to_vec(attrs).expect("a JSON object"));
    buf.resize(buf.len().div_ceil(16) * 16, 0);
    cbc_encrypt(key, &mut buf);
    b64(&buf)
}

/// The attributes `encrypt_attrs` made, or `None` when the key doesn't open them.
pub fn decrypt_attrs(key: &[u8; 16], text: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    let mut buf = unb64(text).ok()?;
    if buf.is_empty() || buf.len() % 16 != 0 {
        return None;
    }
    cbc_decrypt(key, &mut buf);
    let body = buf.strip_prefix(b"MEGA")?;
    let end = body.iter().position(|&b| b == 0).unwrap_or(body.len());
    serde_json::from_slice(&body[..end]).ok()
}

// ---------------------------------------------------------------------------------------------
// Login keys

/// The password key of a v1 account.
pub fn prepare_key_v1(password: &[u8]) -> [u8; 16] {
    let mut data = [0x93u8, 0xC4, 0x67, 0xE3, 0x7D, 0xB0, 0xC7, 0xA4, 0xD1, 0xBE, 0x3F, 0x81, 0x01, 0x52, 0xCB, 0x56];
    let keys: Vec<Aes128> = password
        .chunks(16)
        .map(|chunk| {
            let mut k = [0u8; 16];
            k[..chunk.len()].copy_from_slice(chunk);
            cipher(&k)
        })
        .collect();
    let block = blk(&mut data);
    for _ in 0..65536 {
        for c in &keys {
            c.encrypt_block(block);
        }
    }
    data
}

/// The user hash a v1 account logs in with: the e-mail folded into a block and encrypted 16 384
/// times with the password key; words 0 and 2.
pub fn user_hash_v1(email: &str, key: &[u8; 16]) -> String {
    let mut h = [0u8; 16];
    for (i, b) in email.bytes().enumerate() {
        h[i % 16] ^= b;
    }
    let c = cipher(key);
    let block = blk(&mut h);
    for _ in 0..16384 {
        c.encrypt_block(block);
    }
    let mut out = [0u8; 8];
    out[..4].copy_from_slice(&h[..4]);
    out[4..].copy_from_slice(&h[8..12]);
    b64(&out)
}

/// The password key and user hash of a v2 account: PBKDF2-HMAC-SHA512, 100 000 rounds.
pub fn prepare_key_v2(password: &[u8], salt: &[u8]) -> ([u8; 16], String) {
    let mut dk = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<sha2::Sha512>(password, salt, 100_000, &mut dk);
    (dk[..16].try_into().expect("16 bytes"), b64(&dk[16..]))
}

// ---------------------------------------------------------------------------------------------
// RSA

/// The number at the start of `data` in MEGA's MPI form (a 16-bit bit count, then the bytes),
/// and what follows it.
pub fn mpi(data: &[u8]) -> Result<(&[u8], &[u8])> {
    let bad = || t("O MEGA mandou uma chave RSA malformada.", "MEGA sent a malformed RSA key.").to_string();
    let [hi, lo, rest @ ..] = data else { return Err(bad()) };
    let len = ((usize::from(*hi) << 8) + usize::from(*lo)).div_ceil(8);
    if len > rest.len() {
        return Err(bad());
    }
    Ok(rest.split_at(len))
}

/// The account's RSA private key, from its decrypted MPI form: p, q, d (and u, unused).
#[derive(Clone)]
pub struct RsaKey {
    n: BigUint,
    d: BigUint,
}

impl std::fmt::Debug for RsaKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RsaKey(<redacted>)")
    }
}

impl RsaKey {
    pub fn parse(data: &[u8]) -> Result<RsaKey> {
        let (p, rest) = mpi(data)?;
        let (q, rest) = mpi(rest)?;
        let (d, _) = mpi(rest)?;
        let (p, q) = (BigUint::from_bytes_be(p), BigUint::from_bytes_be(q));
        Ok(RsaKey { n: p * q, d: BigUint::from_bytes_be(d) })
    }

    /// The modulus's length in bytes.
    pub fn size(&self) -> usize {
        self.n.bits().div_ceil(8) as usize
    }

    /// Raw RSA: the plaintext's bytes, big-endian, without leading zeros.
    pub fn decrypt(&self, data: &[u8]) -> Vec<u8> {
        BigUint::from_bytes_be(data).modpow(&self.d, &self.n).to_bytes_be()
    }

    /// Decrypts an MPI-encoded ciphertext.
    pub fn decrypt_mpi(&self, data: &[u8]) -> Result<Vec<u8>> {
        Ok(self.decrypt(mpi(data)?.0))
    }
}

// ---------------------------------------------------------------------------------------------
// The keys attribute

/// The share keys in the account's `^!keys` attribute (where accounts since 2023 keep them),
/// by the shared folder's handle. The attribute is AES-GCM under a key derived from the master
/// key; its plaintext is records of a tag byte, a 3-byte length and a payload.
pub fn share_keys(master: &[u8; 16], value: &[u8]) -> Result<Vec<(String, [u8; 16])>> {
    use aes_gcm::aead::AeadInOut;
    let bad = || t("O MEGA mandou as chaves da conta num formato desconhecido.", "MEGA sent the account's keys in an unknown format.").to_string();
    let mut derived = [0u8; 16];
    hkdf::Hkdf::<sha2::Sha256>::new(None, master).expand(&[1], &mut derived).map_err(|_| bad())?;
    if value.len() < 14 + 16 || value[0] != 20 {
        return Err(bad());
    }
    let (iv, sealed) = value[2..].split_at(12);
    let gcm = aes_gcm::Aes128Gcm::new_from_slice(&derived).map_err(|_| bad())?;
    let mut data = sealed.to_vec();
    gcm.decrypt_in_place(iv.try_into().map_err(|_| bad())?, &[], &mut data).map_err(|_| bad())?;
    let mut out = Vec::new();
    let mut at = 0;
    while at + 4 <= data.len() {
        let tag = data[at];
        let len = (usize::from(data[at + 1]) << 16) | (usize::from(data[at + 2]) << 8) | usize::from(data[at + 3]);
        at += 4;
        let Some(payload) = data.get(at..at + len) else { return Err(bad()) };
        // Share keys: 6-byte handle, 16-byte key, a trust byte.
        if tag == 48 {
            for rec in payload.as_chunks::<23>().0 {
                out.push((b64(&rec[..6]), rec[6..22].try_into().expect("16 bytes")));
            }
        }
        at += len;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Fingerprints

/// The fingerprint attribute (`c`): the CRC, then the modification time (seconds) in as few
/// little-endian bytes as it takes, after their count.
pub fn fingerprint(crc: &[u8; 16], mtime: i64) -> String {
    let mut out = crc.to_vec();
    let mut bytes = Vec::new();
    let mut v = mtime.max(0) as u64;
    while v > 0 {
        bytes.push((v & 0xff) as u8);
        v >>= 8;
    }
    out.push(bytes.len() as u8);
    out.extend(bytes);
    b64(&out)
}

/// The modification time a fingerprint holds.
pub fn fingerprint_mtime(text: &str) -> Option<i64> {
    let raw = unb64(text).ok()?;
    let (&count, rest) = raw.get(16..)?.split_first()?;
    let count = usize::from(count);
    if count > 8 || count > rest.len() {
        return None;
    }
    Some(rest[..count].iter().rev().fold(0i64, |acc, &b| (acc << 8) | i64::from(b)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// Known answers from the `mega` crate's tests (bytes 0, 1, 2, … of each length).
    #[test]
    fn v1_password_keys_match_known_answers() {
        let input = |n: usize| (0..n).map(|i| i as u8).collect::<Vec<u8>>();
        assert_eq!(hex(&prepare_key_v1(&input(8))), "c4589a459956887caf0b408635c3c03b");
        assert_eq!(hex(&prepare_key_v1(&input(10))), "59930b1c55d783ac77df4c4ff261b0f1");
        assert_eq!(hex(&prepare_key_v1(&input(64))), "83bd84689f057f9ed9834b3ecb81d80e");
    }

    /// FIPS-197's AES-128 example, through the ECB helper.
    #[test]
    fn ecb_is_plain_aes() {
        let key: Vec<u8> = (0..16).collect();
        let mut block: Vec<u8> = (0..16).map(|i| (i * 0x11) as u8).collect();
        ecb_encrypt(&key, &mut block);
        assert_eq!(hex(&block), "69c4e0d86a7b0430d8cdb78070b4c55a");
        ecb_decrypt(&key, &mut block);
        assert_eq!(hex(&block), "00112233445566778899aabbccddeeff");
    }

    /// NIST SP 800-38A F.2.1 (CBC-AES128) with a zero IV is the first block's ECB; the
    /// round trip covers the chaining.
    #[test]
    fn cbc_round_trips_and_chains() {
        let key = random::<16>();
        let mut data: Vec<u8> = (0..64).collect();
        let plain = data.clone();
        cbc_encrypt(&key, &mut data);
        let mut first = plain[..16].to_vec();
        ecb_encrypt(&key, &mut first);
        assert_eq!(&data[..16], &first[..]);
        // The same plaintext block encrypts differently further on.
        assert_ne!(data[16..32], data[..16]);
        cbc_decrypt(&key, &mut data);
        assert_eq!(data, plain);
    }

    /// CTR is the file's bytes XORed with AES of nonce‖block number (AES itself checked above).
    #[test]
    fn ctr_is_aes_of_the_counter_blocks() {
        let (key, nonce) = (random::<16>(), random::<8>());
        let mut data = [0x5au8; 40];
        Ctr::new(&key, &nonce, 0).apply(&mut data);
        for (i, piece) in data.chunks(16).enumerate() {
            let mut counter = [0u8; 16];
            counter[..8].copy_from_slice(&nonce);
            counter[8..].copy_from_slice(&(i as u64).to_be_bytes());
            ecb_encrypt(&key, &mut counter);
            for (b, k) in piece.iter().zip(counter) {
                assert_eq!(*b, 0x5a ^ k);
            }
        }
    }

    #[test]
    fn ctr_resumes_anywhere_and_takes_any_pieces() {
        let (key, nonce) = (random::<16>(), random::<8>());
        let plain: Vec<u8> = (0..1000u32).map(|i| (i * 7) as u8).collect();
        let mut whole = plain.clone();
        Ctr::new(&key, &nonce, 0).apply(&mut whole);
        for from in [160usize, 161, 999] {
            let mut tail = plain[from..].to_vec();
            Ctr::new(&key, &nonce, from as u64).apply(&mut tail);
            assert_eq!(tail, whole[from..], "from {from}");
        }
        let mut pieces = plain.clone();
        let mut c = Ctr::new(&key, &nonce, 0);
        let mut at = 0;
        for n in [1usize, 15, 17, 3, 200, 5, 300] {
            c.apply(&mut pieces[at..at + n]);
            at += n;
        }
        c.apply(&mut pieces[at..]);
        assert_eq!(pieces, whole);
        Ctr::new(&key, &nonce, 0).apply(&mut whole);
        assert_eq!(whole, plain);
    }

    /// The MAC straight from its definition, to check the streaming one against.
    fn mac_by_definition(key: &[u8; 16], nonce: &[u8; 8], data: &[u8]) -> [u8; 8] {
        let mut file = [0u8; 16];
        let mut at = 0usize;
        let mut index = 0;
        while at < data.len() {
            let end = (at + chunk_len(index) as usize).min(data.len());
            let mut chunk = data[at..end].to_vec();
            chunk.resize(chunk.len().div_ceil(16) * 16, 0);
            let mut m = [0u8; 16];
            m[..8].copy_from_slice(nonce);
            m[8..].copy_from_slice(nonce);
            for b in chunk.chunks(16) {
                for i in 0..16 {
                    m[i] ^= b[i];
                }
                ecb_encrypt(key, &mut m);
            }
            for i in 0..16 {
                file[i] ^= m[i];
            }
            ecb_encrypt(key, &mut file);
            at = end;
            index += 1;
        }
        let mut out = [0u8; 8];
        for i in 0..4 {
            out[i] = file[i] ^ file[i + 4];
            out[i + 4] = file[i + 8] ^ file[i + 12];
        }
        out
    }

    #[test]
    fn the_streaming_mac_matches_its_definition_whatever_the_pieces() {
        let (key, nonce) = (random::<16>(), random::<8>());
        for size in [0usize, 1, 15, 16, 17, 131_072, 131_073, 400_000, 4_718_592 + 1_048_576 + 33] {
            let data: Vec<u8> = (0..size).map(|i| (i * 31 % 251) as u8).collect();
            let want = mac_by_definition(&key, &nonce, &data);
            let mut whole = Mac::new(&key, &nonce);
            whole.update(&data);
            assert_eq!(whole.finish(), want, "size {size}");
            let mut pieces = Mac::new(&key, &nonce);
            let mut rest = &data[..];
            let mut n = 1;
            while !rest.is_empty() {
                let k = n.min(rest.len());
                pieces.update(&rest[..k]);
                rest = &rest[k..];
                n = n * 3 + 5;
            }
            assert_eq!(pieces.finish(), want, "size {size} in pieces");
        }
    }

    #[test]
    fn chunks_grow_to_a_megabyte() {
        assert_eq!((0..10).map(chunk_len).collect::<Vec<_>>(), [131072, 262144, 393216, 524288, 655360, 786432, 917504, 1048576, 1048576, 1048576]);
    }

    #[test]
    fn file_keys_merge_and_split() {
        let (k, n, m) = (random::<16>(), random::<8>(), random::<8>());
        let merged = merge_file_key(&k, &n, &m);
        assert_eq!(split_file_key(&merged), (k, n, m));
        assert_eq!(attr_key(&merged), k);
        assert_eq!(attr_key(&k), k);
    }

    #[test]
    fn attributes_round_trip_and_a_wrong_key_reads_nothing() {
        let key = random::<16>();
        let mut attrs = serde_json::Map::new();
        attrs.insert("n".into(), "Relatório 2024.pdf".into());
        attrs.insert("c".into(), "abc".into());
        let text = encrypt_attrs(&key, &attrs);
        assert_eq!(decrypt_attrs(&key, &text), Some(attrs));
        assert_eq!(decrypt_attrs(&random::<16>(), &text), None);
        assert_eq!(decrypt_attrs(&key, "!!"), None);
    }

    #[test]
    fn base64_takes_either_alphabet() {
        assert_eq!(unb64("-_8").unwrap(), unb64("+/8=").unwrap());
        assert_eq!(b64(&[0xfb, 0xff]), "-_8");
    }

    #[test]
    fn fingerprints_keep_the_modification_time() {
        let crc = random::<16>();
        for t in [0i64, 1, 255, 256, 1_700_000_000, 4_102_444_800] {
            assert_eq!(fingerprint_mtime(&fingerprint(&crc, t)), Some(t));
        }
        assert_eq!(fingerprint_mtime("AAAA"), None);
    }

    #[test]
    fn rsa_decrypts_what_its_public_half_encrypted() {
        // p = 61, q = 53, n = 3233, e = 17, d = 2753 (the textbook example), as MPIs.
        let mpi = |v: u32| {
            let bytes = BigUint::from(v).to_bytes_be();
            let bits = v.ilog2() + 1;
            let mut out = vec![(bits >> 8) as u8, bits as u8];
            out.extend(bytes);
            out
        };
        let key = [mpi(61), mpi(53), mpi(2753), mpi(38)].concat();
        let rsa = RsaKey::parse(&key).unwrap();
        let c = BigUint::from(65u32).modpow(&BigUint::from(17u32), &BigUint::from(3233u32)).to_bytes_be();
        assert_eq!(rsa.decrypt(&c), vec![65]);
        assert!(RsaKey::parse(&[0, 64, 1]).is_err());
    }

    #[test]
    fn share_keys_come_out_of_the_keys_attribute() {
        use aes_gcm::aead::AeadInOut;
        let master = random::<16>();
        let mut derived = [0u8; 16];
        hkdf::Hkdf::<sha2::Sha256>::new(None, &master).expand(&[1], &mut derived).unwrap();
        let handle = [1u8, 2, 3, 4, 5, 6];
        let key = random::<16>();
        // A version record, then the share keys record.
        let mut plain = vec![1, 0, 0, 1, 1];
        plain.extend([48, 0, 0, 23]);
        plain.extend(handle);
        plain.extend(key);
        plain.push(0);
        let iv = random::<12>();
        let gcm = aes_gcm::Aes128Gcm::new_from_slice(&derived).unwrap();
        let mut sealed = plain.clone();
        gcm.encrypt_in_place((&iv).into(), &[], &mut sealed).unwrap();
        let value = [vec![20, 0], iv.to_vec(), sealed].concat();
        assert_eq!(share_keys(&master, &value).unwrap(), vec![(b64(&handle), key)]);
        assert!(share_keys(&random::<16>(), &value).is_err());
        assert!(share_keys(&master, &[20, 0, 1]).is_err());
    }
}
