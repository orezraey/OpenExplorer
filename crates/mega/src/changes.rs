//! Keeping the tree current with what changes elsewhere (the website, the phone app, someone
//! adding to a shared folder): MEGA's `sc` channel gives the "action packets" since the sequence
//! number the listing came with, or a URL to wait at when there's nothing new. The app asks every
//! few seconds (see the app's `watch`), so nothing here holds a connection open.

use std::collections::HashSet;

use openexplorer_i18n::t;
use serde_json::Value;

use crate::Result;
use crate::account::{Account, Inner};
use crate::api::{ESID, Fail};
use crate::crypto::{attr_key, decrypt_attrs};

/// What changed since the last ask.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Changes {
    /// The folders whose listing may differ now, by handle.
    pub folders: HashSet<String>,
    /// The whole tree was read again: any folder may differ.
    pub all: bool,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        !self.all && self.folders.is_empty()
    }

    /// Whether folder `handle`'s listing may differ.
    pub fn touches(&self, handle: &str) -> bool {
        self.all || self.folders.contains(handle)
    }
}

/// How far a read of the change channel got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Caught {
    /// Nothing more to read: the tree is current.
    Up,
    /// There's more than the rounds allowed.
    More,
    /// Only reading everything again tells what changed: MEGA no longer knows the sequence
    /// number, or a change (a share, new keys) needs a fresh listing.
    Rejected,
}

impl Account {
    /// Takes in what changed since the last ask.
    pub fn poll(&self) -> Result<Changes> {
        let mut changes = Changes::default();
        if !self.lock().refetch && self.read_changes(32, &mut changes)? == Caught::Rejected {
            self.lock().refetch = true;
        }
        if self.lock().refetch {
            self.fetch()?;
            changes.all = true;
        } else {
            self.polled(!changes.is_empty());
        }
        Ok(changes)
    }

    /// Reads the change channel from the tree's sequence number on, `rounds` answers at most,
    /// applying what comes.
    pub(crate) fn read_changes(&self, rounds: usize, changes: &mut Changes) -> Result<Caught> {
        for _ in 0..rounds {
            let sn = self.lock().tree.sn.clone();
            let url = format!("{}/sc?sn={sn}&sid={}", self.api.base(), self.api.sid().unwrap_or_default());
            let answer = match self.api.get_json(&url) {
                Ok(v) => v,
                Err(Fail::Code(ESID)) => return Err(t("A sessão do MEGA terminou (saiu pelo site ou mudou a senha?). Remova a conta e conecte de novo.", "The MEGA session ended (signed out on the website or changed the password?). Remove the account and connect it again.").into()),
                Err(Fail::Code(code)) => {
                    crate::api::log(&format!("MEGA sc: erro {code}, lendo a conta de novo"));
                    return Ok(Caught::Rejected);
                }
                Err(f) => return Err(f.text()),
            };
            // No packets: a URL to wait at (`w`), nothing new.
            let Some(packets) = answer.get("a").and_then(Value::as_array) else { return Ok(Caught::Up) };
            let mut inner = self.lock();
            apply(&mut inner, packets, changes);
            if let Some(sn) = answer.get("sn").and_then(Value::as_str) {
                inner.tree.sn = sn.to_string();
            }
            if inner.refetch {
                return Ok(Caught::Rejected);
            }
            if packets.is_empty() {
                return Ok(Caught::Up);
            }
        }
        Ok(Caught::More)
    }

    /// Applies packets as `poll` would, for tests.
    #[cfg(test)]
    pub(crate) fn apply_packets(&self, packets: &[Value]) -> Changes {
        let mut changes = Changes::default();
        apply(&mut self.lock(), packets, &mut changes);
        changes
    }
}

/// Applies action packets to the tree, noting the folders they touch. Each applies the same
/// whether or not this app made the change: the tree already has its own changes.
fn apply(inner: &mut Inner, packets: &[Value], changes: &mut Changes) {
    let Inner { keys, tree, refetch } = inner;
    for p in packets {
        match p.get("a").and_then(Value::as_str).unwrap_or_default() {
            // New nodes, or moved ones arriving at their new folder.
            "t" => {
                let nodes = p.pointer("/t/f").and_then(Value::as_array).cloned().unwrap_or_default();
                let before = keys.shares.len();
                for raw in &nodes {
                    keys.learn_node(raw);
                }
                for raw in nodes {
                    if let Some(parent) = raw.get("p").and_then(Value::as_str) {
                        changes.folders.insert(parent.to_string());
                    }
                    tree.put_raw(keys, raw);
                }
                if keys.shares.len() > before {
                    tree.unlock(keys);
                }
            }
            // New attributes: a rename.
            "u" => {
                let (Some(h), Some(at)) = (p.get("n").and_then(Value::as_str), p.get("at").and_then(Value::as_str)) else { continue };
                let Some(mut n) = tree.get(h).cloned() else { continue };
                let Some(attrs) = decrypt_attrs(&attr_key(&n.key), at) else { continue };
                n.set_attrs(attrs);
                changes.folders.insert(n.parent.to_string());
                tree.put(n);
            }
            // Deleted, or moved away (`m`: a `t` brings it to its new folder).
            "d" => {
                let Some(h) = p.get("n").and_then(Value::as_str) else { continue };
                let moved = p.get("m").and_then(Value::as_i64) == Some(1);
                if let Some(n) = tree.get(h) {
                    changes.folders.insert(n.parent.to_string());
                    if n.kind.is_dir() {
                        changes.folders.insert(h.to_string());
                    }
                }
                tree.remove(h, !moved);
            }
            "ph" => {
                if let Some((h, ph)) = crate::share::link_change(p) {
                    tree.set_link(&h, ph);
                }
            }
            // Shares made, changed or ended, and the keys attribute: only a fresh listing has
            // all their keys.
            "s" | "s2" | "k" => *refetch = true,
            "ua" if p.get("ua").and_then(Value::as_array).is_some_and(|a| a.iter().any(|v| v.as_str() == Some("^!keys"))) => *refetch = true,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::encrypt_attrs;
    use crate::nodes::Tree;
    use crate::nodes::tests::{keys, listing, raw};
    use serde_json::json;

    fn account() -> Account {
        let mut k = keys();
        let tree = { let l = listing(&k); Tree::from_listing(&mut k, &l) };
        Account::offline(k, tree)
    }

    /// A rename made elsewhere keeps the attributes it doesn't touch: the fingerprint and
    /// whatever else MEGA's apps put there.
    #[test]
    fn a_rename_keeps_the_other_attributes() {
        let a = account();
        let k = keys();
        let mut attrs = serde_json::Map::new();
        attrs.insert("n".into(), "foto.jpg".into());
        attrs.insert("c".into(), crate::crypto::fingerprint(&[2; 16], 1_600_000_000).into());
        attrs.insert("fav".into(), 1.into());
        let new = crate::nodes::tests::raw_with(&k, "NEWFILE1", "ROOT0000", 0, attrs, (k.user.as_str(), k.master));
        a.apply_packets(&[json!({"a": "t", "t": {"f": [new]}})]);
        let node = a.node("/Drive na nuvem/foto.jpg").unwrap();
        let mut renamed = node.attrs();
        renamed.insert("n".into(), "praia.jpg".into());
        renamed.insert("rr".into(), "FOLDER01".into());
        a.apply_packets(&[json!({"a": "u", "n": "NEWFILE1", "at": encrypt_attrs(&attr_key(&node.key), &renamed)})]);
        let after = a.node("/Drive na nuvem/praia.jpg").unwrap();
        assert_eq!(after.attrs(), renamed);
        assert_eq!(after.modified, Some(1_600_000_000));
        assert!(a.node("/Drive na nuvem/foto.jpg").is_err());
    }

    #[test]
    fn new_renamed_moved_and_deleted_items_touch_their_folders() {
        let a = account();
        let k = keys();
        let new = raw(&k, "NEWFILE1", "FOLDER01", 0, "nova.jpg", (k.user.as_str(), k.master));
        let c = a.apply_packets(&[json!({"a": "t", "t": {"f": [new]}})]);
        assert!(c.touches("FOLDER01") && !c.touches("ROOT0000"));
        assert!(a.resolve("/Drive na nuvem/Fotos/nova.jpg").is_some());

        let node = a.node("/Drive na nuvem/nota.txt").unwrap();
        let mut attrs = node.attrs();
        attrs.insert("n".into(), "renomeada.txt".into());
        let at = encrypt_attrs(&attr_key(&node.key), &attrs);
        let c = a.apply_packets(&[json!({"a": "u", "n": "FILE0002", "at": at})]);
        assert!(c.touches("ROOT0000"));
        assert!(a.resolve("/Drive na nuvem/renomeada.txt").is_some());

        // A move: out of Cloud Drive's top, into Fotos, children and all.
        let mut moved = a.node("/Drive na nuvem/Fotos").unwrap();
        moved.parent = crate::nodes::tests::h("BIN00000");
        let c = a.apply_packets(&[json!({"a": "d", "n": "FOLDER01", "m": 1})]);
        assert!(c.touches("ROOT0000") && c.touches("FOLDER01"));
        a.lock().tree.put(moved);
        assert!(a.resolve("/Lixeira/Fotos/praia.jpg").is_some(), "a moved folder keeps what it holds");

        let c = a.apply_packets(&[json!({"a": "d", "n": "FOLDER01"})]);
        assert!(c.touches("BIN00000"));
        assert!(a.resolve("/Lixeira/Fotos").is_none() && a.lock().tree.get("FILE0001").is_none());
    }

    #[test]
    fn shares_and_keys_ask_for_a_fresh_listing() {
        let a = account();
        a.apply_packets(&[json!({"a": "c", "u": []})]);
        assert!(!a.lock().refetch);
        a.apply_packets(&[json!({"a": "ua", "ua": ["^!keys"], "u": "x"})]);
        assert!(a.lock().refetch);
        let b = account();
        b.apply_packets(&[json!({"a": "s2", "n": "FOLDER01"})]);
        assert!(b.lock().refetch);
    }

    #[test]
    fn link_packets_update_the_node() {
        let a = account();
        a.apply_packets(&[json!({"a": "ph", "h": "FILE0001", "ph": "NEWPH001"})]);
        assert_eq!(a.lock().tree.get("FILE0001").unwrap().link.as_deref(), Some("NEWPH001"));
        a.apply_packets(&[json!({"a": "ph", "h": "FILE0001", "ph": "NEWPH001", "d": 1})]);
        assert_eq!(a.lock().tree.get("FILE0001").unwrap().link, None);
    }
}
