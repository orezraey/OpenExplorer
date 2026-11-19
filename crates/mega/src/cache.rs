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
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use openexplorer_cloud::sealed::{self, OnClear, Store};
use openexplorer_cloud::varint::{Reader, put_bytes, put_len, put_signed};
use serde_json::Value;

pub use openexplorer_cloud::sealed::Refusal;

use crate::nodes::{Handle, Kind, Node, NodeKey, Tree, UserHandle};

/// Bumped when the body's shape changes: a file of another format is thrown away.
pub const FORMAT: u32 = 1;
const HEADER: usize = sealed::HEADER;
const TAG: usize = sealed::TAG;

/// The files, `<hash of the e-mail>.tree`. A clear leaves the trees the open accounts hold:
/// each is the account's live state, which the `sc` channel keeps current, not a copy of its
/// file; those accounts just never keep it on disk again this run.
static STORE: Store = Store::new(b"OEMTREE\0", FORMAT, "tree", OnClear::Keep);

/// What seals and opens one session's file.
#[derive(Clone)]
pub struct Secret(sealed::Secret);

impl Secret {
    /// The secret of the session with master key `master` and session key `sek`, of user `user`.
    pub fn of(user: &str, master: &[u8; 16], sek: &[u8; 16]) -> Secret {
        let mut ikm = [0u8; 32];
        ikm[..16].copy_from_slice(master);
        ikm[16..].copy_from_slice(sek);
        Secret(sealed::Secret::derive("OpenExplorer MEGA tree cache", user.as_bytes(), &ikm))
    }
}

// ---------------------------------------------------------------------------------------------
// Where

/// The folder the trees are kept in; `None` keeps none (the default, and tests').
pub fn set_dir(dir: Option<PathBuf>) {
    STORE.set_dir(dir);
}

pub fn dir() -> Option<PathBuf> {
    STORE.dir()
}

/// The file of account `email` in `dir`: named by a hash, so the folder doesn't list e-mails.
pub fn file_in(dir: &Path, email: &str) -> PathBuf {
    STORE.file_in(dir, &crate::session::normal_email(email))
}

/// The file of account `email` in the folder set, if one is.
pub fn file_of(email: &str) -> Option<PathBuf> {
    dir().map(|d| file_in(&d, email))
}

/// What an account notes as it starts, before reading its kept tree: [`cleared_since`] tells
/// whether its file's folder was cleared after.
pub fn mark() -> u64 {
    STORE.mark()
}

/// Whether the folder `file` is in was cleared after [`mark`] gave `mark`.
pub fn cleared_since(mark: u64, file: &Path) -> bool {
    STORE.cleared_since(mark, file)
}

/// Runs `clear` on the folder set (see [`set_dir`]), with every write held off: a tree an
/// account open now (or opening) holds is never written there again this run, so the folder
/// stays clear; accounts opened later read the whole tree and keep it as usual. `None` with no
/// folder set.
pub fn clear_with<R>(clear: impl FnOnce(&Path) -> R) -> Option<R> {
    STORE.clear_with(clear)
}

/// [`clear_with`] on folder `dir`.
pub fn clear_in<R>(dir: &Path, clear: impl FnOnce(&Path) -> R) -> R {
    STORE.clear_in(dir, clear)
}

/// Deletes account `email`'s file, after any write in flight.
pub fn forget(email: &str) {
    if let Some(file) = file_of(email) {
        forget_file(&file);
    }
}

/// Deletes `file`, and any temporary file a write cut short (a crash) left beside it.
pub fn forget_file(file: &Path) {
    STORE.forget_file(file);
}

// ---------------------------------------------------------------------------------------------
// Body

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

/// A MEGA id of `N` bytes, padded with zeros.
fn id<const N: usize>(r: &mut Reader) -> Result<crate::nodes::Id<N>, Refusal> {
    let raw = r.array::<N>()?;
    let len = raw.iter().position(|&b| b == 0).unwrap_or(N);
    crate::nodes::Id::new(std::str::from_utf8(&raw[..len]).map_err(|_| Refusal::Malformed)?).filter(|id| id.0 == raw).ok_or(Refusal::Malformed)
}

/// The tree and the share keys [`encode`] wrote (`body` without the header).
pub fn decode(body: &[u8]) -> Result<(Tree, HashMap<String, [u8; 16]>), Refusal> {
    let mut r = Reader::new(body);
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
        let handle: Handle = id(&mut r)?;
        let parent: Handle = id(&mut r)?;
        let kind = Kind::of(i64::from(r.byte()?)).ok_or(Refusal::Malformed)?;
        let flags = r.byte()?;
        if handle.is_empty() || flags & !(HAS_MODIFIED | HAS_OTHERS | HAS_INSHARE | HAS_LINK) != 0 {
            return Err(Refusal::Malformed);
        }
        let name = r.text()?.to_string();
        let size = r.unsigned()?;
        let created = r.signed()?;
        let modified = if flags & HAS_MODIFIED != 0 { Some(r.signed()?) } else { None };
        let key = r.bytes()?;
        if key.len() > 32 {
            return Err(Refusal::Malformed);
        }
        let others = if flags & HAS_OTHERS != 0 { Some(r.text()?.into()) } else { None };
        let owner: UserHandle = id(&mut r)?;
        let inshare = if flags & HAS_INSHARE != 0 { Some((id::<11>(&mut r)?, r.byte()?.min(2))) } else { None };
        let link = if flags & HAS_LINK != 0 { Some(id::<8>(&mut r)?) } else { None };
        if tree.get(&handle).is_some() {
            return Err(Refusal::Malformed);
        }
        tree.put(Node { handle, parent, kind, name, size, created, modified, key: NodeKey::new(key), others, owner, inshare, link });
    }
    if !r.done() {
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
pub fn seal(secret: &Secret, file: Vec<u8>) -> Vec<u8> {
    STORE.seal(&secret.0, file)
}

/// Opens a file [`seal`] made, in place: the body, after the header.
pub fn open(secret: &Secret, file: &mut [u8]) -> Result<std::ops::Range<usize>, Refusal> {
    STORE.open(&secret.0, file)
}

// ---------------------------------------------------------------------------------------------
// Files

/// Writes the tree to `file` whole: a temporary file renamed over the old one, so a crash
/// midway leaves the old file or the new, never half of one.
pub fn write(file: &Path, secret: &Secret, body: Vec<u8>) -> std::io::Result<()> {
    write_unless(file, secret, body, &AtomicBool::new(false), mark()).map(|_| ())
}

/// [`write`], unless by the time it's its turn to write `forgotten` is on (the account was
/// removed meanwhile) or the folder was cleared after `mark` (see [`clear_with`]). Whether it
/// wrote.
pub fn write_unless(file: &Path, secret: &Secret, body: Vec<u8>, forgotten: &AtomicBool, mark: u64) -> std::io::Result<bool> {
    let sealed = seal(secret, body);
    STORE.write_unless(file, &sealed, forgotten, mark)
}

/// The tree and the share keys kept in `file`. A file of another session or format is refused
/// from its header, without reading the rest (over 100 MB on a big account).
pub fn read(file: &Path, secret: &Secret) -> Result<(Tree, HashMap<String, [u8; 16]>), Refusal> {
    let (data, body) = STORE.read(file, &secret.0)?;
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

    /// A file sealed before the shared crate (`openexplorer-cloud`) took over the sealing, and
    /// its name: both must still be what they were.
    #[test]
    fn a_file_kept_before_still_opens() {
        let hex = "4f454d5452454500010000006a3c5f089e0f15662618c2eccf5f5a0192c78e184b36a63f918beb9a17c9226c4cb485ceb6f6f30422153a8c99a8c3ac25cdec8c390f68b28b80b40fd9eb63284667eec064cafb34";
        let mut old: Vec<u8> = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect();
        let body = open(&secret(), &mut old).unwrap();
        assert_eq!(&old[body], b"kept before the shared crate");
        assert_eq!(file_in(Path::new("d"), " Ana@Exemplo.com"), Path::new("d").join("840d9902bc6853e59cd9585cd0ecb3ee.tree"));
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
                12..28 => Refusal::OtherKey,
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
            assert_eq!(open(other, &mut sealed.clone()).err(), Some(Refusal::OtherKey));
            let forged = Secret(sealed::Secret { key: other.0.key, id: secret().0.id });
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
        let mut r = Reader::new(&out);
        for n in nums {
            assert_eq!(r.signed().unwrap(), n);
        }
        assert_eq!(r.unsigned().unwrap(), u64::MAX);
    }
}
