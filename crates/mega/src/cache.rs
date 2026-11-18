//! The account's decrypted tree kept on disk between runs, the way MEGA's own SDK keeps its
//! local cache: a start reads it and only asks the `sc` channel for what changed since its
//! sequence number, instead of downloading and decrypting the whole tree with `f` again.
//!
//! # Format
//!
//! One file per account, `<hash of the e-mail>.tree`, in the folder the app sets with
//! [`set_dir`] (`%LOCALAPPDATA%\OpenExplorer\mega`; with none set, nothing is kept). The file is
//! a compact binary snapshot of the tree written whole and atomically (a temporary file renamed
//! over the old one), not a database: the tree already lives in memory, and a snapshot needs no
//! C dependency.
//!
//! ```text
//! magic "OEMTREE\0" | format (u32 LE) | key id (16) | nonce (12) | AES-256-GCM(body) | tag (16)
//! ```
//!
//! The body holds the sequence number, the share keys and each node (see [`encode`]).
//!
//! # Security
//!
//! The file holds names and node keys in the clear once opened, so it's sealed with AES-256-GCM
//! under a key derived (HKDF-SHA256) from the master key and the session key together, the
//! user's handle as salt. Both only come back from MEGA by resuming the session kept in the
//! credential store, so the file alone opens nothing, and a new login (a new session key) can't
//! open an old file either: it's thrown away and the tree read again. The magic, the format and
//! the key id are authenticated with the body, so a changed byte anywhere is refused. The key id
//! (derived apart from the key) tells a file of another session from a damaged one without
//! decrypting it.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use aes_gcm::KeyInit;
use aes_gcm::aead::AeadInOut;
use serde_json::Value;
use sha2::Digest;

use crate::nodes::{Handle, Kind, Node, NodeKey, Tree, UserHandle};

const MAGIC: &[u8; 8] = b"OEMTREE\0";
/// Bumped when the body's shape changes: a file of another format is thrown away.
pub const FORMAT: u32 = 1;
const HEADER: usize = 8 + 4 + 16 + 12;
const TAG: usize = 16;

/// Why a kept tree wasn't used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// No file.
    Missing,
    /// It couldn't be read.
    Io(String),
    /// Not one of these files, or of another format.
    Format,
    /// Sealed by another session (or another account).
    OtherSession,
    /// Changed since it was written: the authentication failed.
    Tampered,
    /// Opened, but its contents don't read as a tree.
    Malformed,
}

/// What seals and opens one session's file.
#[derive(Clone)]
pub struct Secret {
    key: [u8; 32],
    id: [u8; 16],
}

impl Secret {
    /// The secret of the session with master key `master` and session key `sek`, of user `user`.
    pub fn of(user: &str, master: &[u8; 16], sek: &[u8; 16]) -> Secret {
        let mut ikm = [0u8; 32];
        ikm[..16].copy_from_slice(master);
        ikm[16..].copy_from_slice(sek);
        let hk = hkdf::Hkdf::<sha2::Sha256>::new(Some(user.as_bytes()), &ikm);
        let mut key = [0u8; 32];
        let mut id = [0u8; 16];
        hk.expand(b"OpenExplorer MEGA tree cache: key", &mut key).expect("32 bytes is a valid length");
        hk.expand(b"OpenExplorer MEGA tree cache: key id", &mut id).expect("16 bytes is a valid length");
        Secret { key, id }
    }
}

// ---------------------------------------------------------------------------------------------
// Where

static DIR: Mutex<Option<PathBuf>> = Mutex::new(None);
/// One file is written at a time, so an older snapshot never lands over a newer one, and a
/// forget waits for a write in flight (then the file it renamed is deleted too).
static WRITING: Mutex<()> = Mutex::new(());

/// The folder the trees are kept in; `None` keeps none (the default, and tests').
pub fn set_dir(dir: Option<PathBuf>) {
    *DIR.lock().unwrap_or_else(|e| e.into_inner()) = dir;
}

pub fn dir() -> Option<PathBuf> {
    DIR.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// The file of account `email` in `dir`: named by a hash, so the folder doesn't list e-mails.
pub fn file_in(dir: &Path, email: &str) -> PathBuf {
    let digest = sha2::Sha256::digest(crate::session::normal_email(email).as_bytes());
    let name: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    dir.join(format!("{name}.tree"))
}

/// The file of account `email` in the folder set, if one is.
pub fn file_of(email: &str) -> Option<PathBuf> {
    dir().map(|d| file_in(&d, email))
}

/// Deletes account `email`'s file, after any write in flight.
pub fn forget(email: &str) {
    if let Some(file) = file_of(email) {
        forget_file(&file);
    }
}

/// Deletes `file`, and any temporary file a write cut short (a crash) left beside it.
pub fn forget_file(file: &Path) {
    let _held = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    let _ = std::fs::remove_file(file);
    let (Some(dir), Some(stem)) = (file.parent(), file.file_stem().and_then(|s| s.to_str())) else { return };
    let prefix = format!("{stem}.tmp");
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if entry.file_name().to_str().is_some_and(|n| n.starts_with(&prefix)) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Body

fn put_len(out: &mut Vec<u8>, mut n: u64) {
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

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_len(out, b.len() as u64);
    out.extend_from_slice(b);
}

/// Signed numbers zigzagged, so small negative ones stay short.
fn put_signed(out: &mut Vec<u8>, n: i64) {
    put_len(out, ((n << 1) ^ (n >> 63)) as u64);
}

const HAS_MODIFIED: u8 = 1;
const HAS_OTHERS: u8 = 2;
const HAS_INSHARE: u8 = 4;
const HAS_LINK: u8 = 8;

/// The tree and the share keys as the file's body: the sequence number, the share keys, the
/// nodes no key opened yet (as MEGA sent them), then each node.
pub fn encode(tree: &Tree, shares: &HashMap<String, [u8; 16]>) -> Vec<u8> {
    // About what a node takes with a fingerprint and a real name, so the body grows rarely.
    let mut out = Vec::with_capacity(HEADER + tree.len() * 150 + TAG);
    out.resize(HEADER, 0);
    put_bytes(&mut out, tree.sn.as_bytes());
    put_len(&mut out, shares.len() as u64);
    for (h, k) in shares {
        put_bytes(&mut out, h.as_bytes());
        out.extend_from_slice(k);
    }
    let locked = tree.locked_nodes();
    put_len(&mut out, locked.len() as u64);
    for raw in locked {
        put_bytes(&mut out, serde_json::to_string(raw).unwrap_or_default().as_bytes());
    }
    put_len(&mut out, tree.len() as u64);
    for n in tree.nodes() {
        out.extend_from_slice(&n.handle.0);
        out.extend_from_slice(&n.parent.0);
        out.push(n.kind.code());
        let flag = |on: bool, f: u8| if on { f } else { 0 };
        let flags = flag(n.modified.is_some(), HAS_MODIFIED) | flag(n.others.is_some(), HAS_OTHERS) | flag(n.inshare.is_some(), HAS_INSHARE) | flag(n.link.is_some(), HAS_LINK);
        out.push(flags);
        put_bytes(&mut out, n.name.as_bytes());
        put_len(&mut out, n.size);
        put_signed(&mut out, n.created);
        if let Some(m) = n.modified {
            put_signed(&mut out, m);
        }
        put_bytes(&mut out, &n.key);
        if let Some(o) = &n.others {
            put_bytes(&mut out, o.as_bytes());
        }
        out.extend_from_slice(&n.owner.0);
        if let Some((who, access)) = n.inshare {
            out.extend_from_slice(&who.0);
            out.push(access);
        }
        if let Some(l) = n.link {
            out.extend_from_slice(&l.0);
        }
    }
    out
}

struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Refusal> {
        let end = self.at.checked_add(n).filter(|&e| e <= self.data.len()).ok_or(Refusal::Malformed)?;
        let out = &self.data[self.at..end];
        self.at = end;
        Ok(out)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], Refusal> {
        Ok(self.take(N)?.try_into().expect("N bytes"))
    }

    fn byte(&mut self) -> Result<u8, Refusal> {
        Ok(self.take(1)?[0])
    }

    fn len(&mut self) -> Result<u64, Refusal> {
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

    fn signed(&mut self) -> Result<i64, Refusal> {
        let n = self.len()?;
        Ok(((n >> 1) as i64) ^ -((n & 1) as i64))
    }

    /// A count of things at least `min` bytes each, checked against what's left.
    fn count(&mut self, min: usize) -> Result<usize, Refusal> {
        let n = usize::try_from(self.len()?).map_err(|_| Refusal::Malformed)?;
        if n.saturating_mul(min) > self.data.len() - self.at {
            return Err(Refusal::Malformed);
        }
        Ok(n)
    }

    fn bytes(&mut self) -> Result<&'a [u8], Refusal> {
        let n = usize::try_from(self.len()?).map_err(|_| Refusal::Malformed)?;
        self.take(n)
    }

    fn text(&mut self) -> Result<&'a str, Refusal> {
        std::str::from_utf8(self.bytes()?).map_err(|_| Refusal::Malformed)
    }

    fn id<const N: usize>(&mut self) -> Result<crate::nodes::Id<N>, Refusal> {
        let raw = self.array::<N>()?;
        let len = raw.iter().position(|&b| b == 0).unwrap_or(N);
        crate::nodes::Id::new(std::str::from_utf8(&raw[..len]).map_err(|_| Refusal::Malformed)?).filter(|id| id.0 == raw).ok_or(Refusal::Malformed)
    }
}

/// The tree and the share keys [`encode`] wrote (`body` without the header).
pub fn decode(body: &[u8]) -> Result<(Tree, HashMap<String, [u8; 16]>), Refusal> {
    let mut r = Reader { data: body, at: 0 };
    let sn = r.text()?.to_string();
    let count = r.count(1 + 16)?;
    let mut shares = HashMap::with_capacity(count);
    for _ in 0..count {
        let h = r.text()?.to_string();
        shares.insert(h, r.array::<16>()?);
    }
    let count = r.count(1)?;
    let mut locked = Vec::with_capacity(count);
    for _ in 0..count {
        locked.push(serde_json::from_str::<Value>(r.text()?).map_err(|_| Refusal::Malformed)?);
    }
    let count = r.count(8 + 8 + 2 + 3 + 1 + 11)?;
    let mut tree = Tree::with_capacity(count);
    for _ in 0..count {
        let handle: Handle = r.id()?;
        let parent: Handle = r.id()?;
        let kind = Kind::of(i64::from(r.byte()?)).ok_or(Refusal::Malformed)?;
        let flags = r.byte()?;
        if handle.is_empty() || flags & !(HAS_MODIFIED | HAS_OTHERS | HAS_INSHARE | HAS_LINK) != 0 {
            return Err(Refusal::Malformed);
        }
        let name = r.text()?.to_string();
        let size = r.len()?;
        let created = r.signed()?;
        let modified = if flags & HAS_MODIFIED != 0 { Some(r.signed()?) } else { None };
        let key = r.bytes()?;
        if key.len() > 32 {
            return Err(Refusal::Malformed);
        }
        let others = if flags & HAS_OTHERS != 0 { Some(r.text()?.into()) } else { None };
        let owner: UserHandle = r.id()?;
        let inshare = if flags & HAS_INSHARE != 0 { Some((r.id::<11>()?, r.byte()?.min(2))) } else { None };
        let link = if flags & HAS_LINK != 0 { Some(r.id::<8>()?) } else { None };
        if tree.get(&handle).is_some() {
            return Err(Refusal::Malformed);
        }
        tree.put(Node { handle, parent, kind, name, size, created, modified, key: NodeKey::new(key), others, owner, inshare, link });
    }
    if r.at != body.len() {
        return Err(Refusal::Malformed);
    }
    tree.sn = sn;
    tree.set_locked(locked);
    Ok((tree, shares))
}

// ---------------------------------------------------------------------------------------------
// Sealing

/// Seals a body [`encode`] made (its first bytes are room for the header) in place: the file's
/// whole contents.
pub fn seal(secret: &Secret, mut file: Vec<u8>) -> Vec<u8> {
    let nonce = crate::crypto::random::<12>();
    file[..8].copy_from_slice(MAGIC);
    file[8..12].copy_from_slice(&FORMAT.to_le_bytes());
    file[12..28].copy_from_slice(&secret.id);
    file[28..HEADER].copy_from_slice(&nonce);
    let gcm = aes_gcm::Aes256Gcm::new_from_slice(&secret.key).expect("a 32-byte key");
    let (head, body) = file.split_at_mut(HEADER);
    let tag = gcm.encrypt_inout_detached((&nonce[..]).try_into().expect("12 bytes"), &head[..28], body.into()).expect("a body GCM takes");
    file.extend_from_slice(&tag);
    file
}

/// Whether a file's first bytes are of this format and this session's.
fn check_header(secret: &Secret, head: &[u8]) -> Result<(), Refusal> {
    if head.len() < HEADER || &head[..8] != MAGIC || head[8..12] != FORMAT.to_le_bytes() {
        return Err(Refusal::Format);
    }
    if head[12..28] != secret.id {
        return Err(Refusal::OtherSession);
    }
    Ok(())
}

/// Opens a file [`seal`] made, in place: the body, after the header.
pub fn open(secret: &Secret, file: &mut [u8]) -> Result<std::ops::Range<usize>, Refusal> {
    if file.len() < HEADER + TAG {
        return Err(Refusal::Format);
    }
    check_header(secret, &file[..HEADER])?;
    let end = file.len() - TAG;
    let tag: [u8; TAG] = file[end..].try_into().expect("16 bytes");
    let (head, rest) = file.split_at_mut(HEADER);
    let gcm = aes_gcm::Aes256Gcm::new_from_slice(&secret.key).expect("a 32-byte key");
    gcm.decrypt_inout_detached((&head[28..]).try_into().expect("12 bytes"), &head[..28], (&mut rest[..end - HEADER]).into(), (&tag[..]).try_into().expect("16 bytes")).map_err(|_| Refusal::Tampered)?;
    Ok(HEADER..end)
}

// ---------------------------------------------------------------------------------------------
// Files

/// Writes the tree to `file` whole: a temporary file renamed over the old one, so a crash
/// midway leaves the old file or the new, never half of one.
pub fn write(file: &Path, secret: &Secret, body: Vec<u8>) -> std::io::Result<()> {
    write_unless(file, secret, body, &AtomicBool::new(false))
}

/// [`write`], unless `forgotten` is on by the time it's its turn to write (the account was
/// removed meanwhile).
pub fn write_unless(file: &Path, secret: &Secret, body: Vec<u8>, forgotten: &AtomicBool) -> std::io::Result<()> {
    let sealed = seal(secret, body);
    let _held = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    if forgotten.load(Ordering::SeqCst) {
        return Ok(());
    }
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = file.with_extension(format!("tmp{}", u32::from_le_bytes(crate::crypto::random::<4>())));
    let result = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&sealed)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, file)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// The tree and the share keys kept in `file`.
pub fn read(file: &Path, secret: &Secret) -> Result<(Tree, HashMap<String, [u8; 16]>), Refusal> {
    let io = |e: std::io::Error| Refusal::Io(e.to_string());
    let mut f = match std::fs::File::open(file) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(Refusal::Missing),
        Err(e) => return Err(io(e)),
    };
    // The header first: a file of another session or format is refused without reading the
    // rest (over 100 MB on a big account).
    let mut data = vec![0; HEADER];
    match f.read_exact(&mut data) {
        Ok(()) => check_header(secret, &data)?,
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(Refusal::Format),
        Err(e) => return Err(io(e)),
    }
    let len = f.metadata().map_err(io)?.len();
    data.reserve(usize::try_from(len).unwrap_or(0).saturating_sub(HEADER));
    f.read_to_end(&mut data).map_err(io)?;
    let body = open(secret, &mut data)?;
    decode(&data[body])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nodes::tests::{keys, listing, raw};

    fn secret() -> Secret {
        Secret::of("USER0000000", &[7; 16], &[9; 16])
    }

    /// A tree with a bit of everything: a link, an incoming share, attributes besides the name,
    /// a node no key opens yet.
    fn sample() -> (Tree, HashMap<String, [u8; 16]>) {
        let mut k = keys();
        let mut l = listing(&k);
        let sk = [4u8; 16];
        k.shares.insert("INSHARE1".into(), sk);
        let mut top = raw(&k, "INSHARE1", "", 1, "Deles", ("INSHARE1", sk));
        top["su"] = "OTHERUSER01".into();
        top["r"] = 1.into();
        l["f"].as_array_mut().unwrap().push(top);
        l["f"].as_array_mut().unwrap().push(raw(&k, "LOCKED01", "FOLDER01", 0, "segredo.txt", ("SOMESHARE", [8u8; 16])));
        let mut attrs = serde_json::Map::new();
        attrs.insert("n".into(), "foto.jpg".into());
        attrs.insert("c".into(), crate::crypto::fingerprint(&[2; 16], 1_600_000_000).into());
        attrs.insert("lbl".into(), 3.into());
        l["f"].as_array_mut().unwrap().push(crate::nodes::tests::raw_with(&k, "FILE0003", "FOLDER01", 0, attrs, (k.user.as_str(), k.master)));
        let tree = Tree::from_listing(&mut k, &l);
        (tree, k.shares)
    }

    fn same(a: &Tree, b: &Tree) {
        let mut x: Vec<&Node> = a.nodes().collect();
        let mut y: Vec<&Node> = b.nodes().collect();
        x.sort_by_key(|n| n.handle);
        y.sort_by_key(|n| n.handle);
        assert_eq!(x, y);
        assert_eq!((&a.sn, &a.root, &a.rubbish, &a.vault), (&b.sn, &b.root, &b.rubbish, &b.vault));
        assert_eq!(a.locked_nodes(), b.locked_nodes());
        for h in ["ROOT0000", "FOLDER01", "BIN00000"] {
            let names = |t: &Tree| t.children(h).into_iter().map(|(n, x)| (n, x.handle)).collect::<Vec<_>>();
            assert_eq!(names(a), names(b));
        }
        assert_eq!(a.inshares().len(), b.inshares().len());
    }

    fn temp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("oe-mega-cache-{}-{name}", std::process::id()))
    }

    #[test]
    fn a_tree_goes_to_disk_and_comes_back_whole() {
        let (tree, shares) = sample();
        let dir = temp("roundtrip");
        let file = file_in(&dir, "Ana@Exemplo.com ");
        assert_eq!(file, file_in(&dir, "ana@exemplo.com"), "one file per account, whatever the e-mail's case");
        write(&file, &secret(), encode(&tree, &shares)).unwrap();
        let (back, back_shares) = read(&file, &secret()).unwrap();
        same(&tree, &back);
        assert_eq!(back_shares, shares);
        assert_eq!(back.get("FILE0002").unwrap().link.as_deref(), Some("PUBH0001"));
        assert_eq!(back.get("INSHARE1").unwrap().inshare.map(|(u, a)| (u.to_string(), a)), Some(("OTHERUSER01".into(), 1)));
        assert_eq!(back.get("FILE0003").unwrap().attrs(), tree.get("FILE0003").unwrap().attrs());
        assert_eq!(back.get("FILE0003").unwrap().modified, Some(1_600_000_000));
        // The names aren't readable in the file.
        let raw = std::fs::read(&file).unwrap();
        assert!(!raw.windows(5).any(|w| w == b"praia") && !raw.windows(5).any(|w| w == b"Fotos"));
        // Nodes still lock out: the share key arrives, and the waiting node opens.
        let mut k = keys();
        k.shares.insert("SOMESHARE".into(), [8u8; 16]);
        let mut back = back;
        assert!(back.unlock(&k));
        assert_eq!(back.get("LOCKED01").unwrap().name, "segredo.txt");
        // A write cut short left its temporary file: it goes with the account's.
        let stray = file.with_extension("tmp123");
        std::fs::write(&stray, b"half").unwrap();
        forget_file(&file);
        assert_eq!(read(&file, &secret()).err(), Some(Refusal::Missing));
        assert!(!stray.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_changed_byte_anywhere_is_refused() {
        let (tree, shares) = sample();
        let sealed = seal(&secret(), encode(&tree, &shares));
        // Every byte of the header, and a sample of the body and the tag.
        let picks: Vec<usize> = (0..HEADER).chain((HEADER..sealed.len()).step_by(37)).chain([sealed.len() - 1]).collect();
        for i in picks {
            let mut bad = sealed.clone();
            bad[i] ^= 0x01;
            let got = open(&secret(), &mut bad).err();
            let want = match i {
                0..12 => Refusal::Format,
                12..28 => Refusal::OtherSession,
                _ => Refusal::Tampered,
            };
            assert_eq!(got, Some(want), "byte {i}");
        }
        let mut cut = sealed.clone();
        cut.truncate(sealed.len() - 1);
        assert_eq!(open(&secret(), &mut cut).err(), Some(Refusal::Tampered));
        assert_eq!(open(&secret(), &mut sealed[..10].to_vec()).err(), Some(Refusal::Format));
        let mut good = sealed;
        assert!(open(&secret(), &mut good).is_ok());
    }

    /// Another session (a new login: a new session key), another account, or the key id forged
    /// to match: none opens the file.
    #[test]
    fn another_key_opens_nothing() {
        let (tree, shares) = sample();
        let sealed = seal(&secret(), encode(&tree, &shares));
        let others = [Secret::of("USER0000000", &[7; 16], &[10; 16]), Secret::of("USER0000000", &[8; 16], &[9; 16]), Secret::of("USER0000001", &[7; 16], &[9; 16])];
        for other in &others {
            assert_eq!(open(other, &mut sealed.clone()).err(), Some(Refusal::OtherSession));
            let forged = Secret { key: other.key, id: secret().id };
            assert_eq!(open(&forged, &mut sealed.clone()).err(), Some(Refusal::Tampered));
        }
    }

    #[test]
    fn another_format_is_refused() {
        let (tree, shares) = sample();
        let mut sealed = seal(&secret(), encode(&tree, &shares));
        sealed[8..12].copy_from_slice(&(FORMAT + 1).to_le_bytes());
        assert_eq!(open(&secret(), &mut sealed).err(), Some(Refusal::Format));
        let mut other = b"something else entirely, longer than a header and a tag".to_vec();
        assert_eq!(open(&secret(), &mut other).err(), Some(Refusal::Format));
    }

    /// A body that opens but doesn't read as a tree (a bug, or a format change without a bump)
    /// is refused, never a panic.
    #[test]
    fn a_body_that_doesnt_read_is_refused() {
        let (tree, shares) = sample();
        let body = encode(&tree, &shares)[HEADER..].to_vec();
        assert!(decode(&body).is_ok());
        for cut in [0, 1, 5, body.len() / 2, body.len() - 1] {
            assert_eq!(decode(&body[..cut]).err(), Some(Refusal::Malformed), "cut at {cut}");
        }
        let mut longer = body.clone();
        longer.push(0);
        assert_eq!(decode(&longer).err(), Some(Refusal::Malformed));
        // A count far beyond what's there.
        let mut huge = Vec::new();
        put_bytes(&mut huge, b"SN");
        put_len(&mut huge, u64::MAX >> 1);
        assert_eq!(decode(&huge).err(), Some(Refusal::Malformed));
        // Every single-byte change either reads or is refused.
        for i in 0..body.len() {
            let mut b = body.clone();
            b[i] = b[i].wrapping_add(0x41);
            let _ = decode(&b);
        }
    }

    #[test]
    fn signed_numbers_and_lengths_round_trip() {
        let mut out = Vec::new();
        let nums = [0i64, 1, -1, 63, -64, i64::MAX, i64::MIN, 1_700_000_000];
        for n in nums {
            put_signed(&mut out, n);
        }
        put_len(&mut out, u64::MAX);
        let mut r = Reader { data: &out, at: 0 };
        for n in nums {
            assert_eq!(r.signed().unwrap(), n);
        }
        assert_eq!(r.len().unwrap(), u64::MAX);
    }
}
