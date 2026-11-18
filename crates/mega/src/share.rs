//! Public links: `https://mega.nz/file/<handle>#<key>`. The key after `#` never reaches MEGA's
//! servers; whoever has the whole link can open the file.
//!
//! A file's link is made (`l`) and removed (`l` with `d`) here. A folder's link needs a share
//! key registered in the account's keys attribute, which only MEGA's own apps write: a folder
//! already shared by link (on the website) gives its link and can lose it here, but a new
//! folder link is made on the website.

use openexplorer_i18n::{t, tr};
use serde_json::{Value, json};

use crate::Result;
use crate::account::Account;
use crate::crypto::b64;
use crate::nodes::{Kind, Node};

/// The link of node `n` with public handle `ph`, given the key whoever opens it needs.
pub fn link_url(kind: Kind, ph: &str, key: &[u8]) -> String {
    let what = if kind == Kind::File { "file" } else { "folder" };
    format!("https://mega.nz/{what}/{ph}#{}", b64(key))
}

impl Account {
    /// The item's public link, when it has one.
    pub fn link_of(&self, n: &Node) -> Option<String> {
        let inner = self.lock();
        let n = inner.tree.get(&n.handle)?;
        let ph = n.link.as_ref()?;
        match n.kind {
            Kind::File => Some(link_url(Kind::File, ph, &n.key)),
            Kind::Folder => inner.keys.shares.get(&n.handle).map(|k| link_url(Kind::Folder, ph, k)),
            _ => None,
        }
    }

    /// Makes the item's public link (a file's), or gives the one it has.
    pub fn create_link(&self, n: &Node) -> Result<String> {
        if let Some(url) = self.link_of(n) {
            return Ok(url);
        }
        self.can_share(n)?;
        if n.kind != Kind::File {
            return Err(t(
                "Links de pasta são criados no site do MEGA (a chave da pasta fica guardada só pelos apps do MEGA). Depois de criado lá, o Open Explorer copia e remove o link.",
                "Folder links are made on the MEGA website (the folder's key is kept only by MEGA's own apps). Once made there, Open Explorer copies and removes the link.",
            )
            .into());
        }
        let ph = self.api.run(json!({"a": "l", "n": n.handle}))?;
        let ph = ph.as_str().ok_or(t("O MEGA não deu o link.", "MEGA didn't give the link."))?.to_string();
        self.lock().tree.set_link(&n.handle, Some(ph.clone()));
        Ok(link_url(Kind::File, &ph, &n.key))
    }

    /// Removes the item's public link: whoever has it can't open the item any more.
    pub fn remove_link(&self, n: &Node) -> Result<()> {
        let has = self.lock().tree.get(&n.handle).is_some_and(|x| x.link.is_some());
        if !has {
            return Err(tr!("“{}” não tem link público.", "“{}” has no public link.", n.name));
        }
        self.can_share(n)?;
        self.api.run(json!({"a": "l", "n": n.handle, "d": 1}))?;
        // A folder's link stands on a share with "EXP", which goes too.
        if n.kind == Kind::Folder {
            let _ = self.api.call(json!({"a": "s2", "n": n.handle, "s": [{"u": "EXP"}]}));
        }
        self.lock().tree.set_link(&n.handle, None);
        Ok(())
    }

    /// Only what the account owns, outside the Rubbish Bin, can have a link.
    fn can_share(&self, n: &Node) -> Result<()> {
        let inner = self.lock();
        if inner.tree.in_rubbish(&n.handle) {
            return Err(t("Restaure o item da Lixeira antes de compartilhar.", "Restore the item from the Rubbish Bin before sharing it.").into());
        }
        if n.owner != inner.keys.user || inner.tree.top_of(&n.handle).is_some_and(|t| t.inshare.is_some()) {
            return Err(t("Só o dono pode criar ou remover links deste item.", "Only the owner can make or remove links for this item.").into());
        }
        if !matches!(n.kind, Kind::File | Kind::Folder) {
            return Err(t("Essa pasta do MEGA não pode ter link.", "That MEGA folder can't have a link.").into());
        }
        Ok(())
    }
}

/// A link change MEGA reports (`ph` action packet): the node, and its public handle or none.
pub(crate) fn link_change(packet: &Value) -> Option<(String, Option<String>)> {
    let h = packet.get("h")?.as_str()?.to_string();
    let removed = packet.get("d").and_then(Value::as_i64) == Some(1);
    let ph = if removed { None } else { packet.get("ph").and_then(Value::as_str).map(str::to_string) };
    Some((h, ph))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nodes::Tree;
    use crate::nodes::tests::{keys, listing};

    #[test]
    fn links_carry_the_key_after_the_hash() {
        let mut k = keys();
        let tree = { let l = listing(&k); Tree::from_listing(&mut k, &l) };
        let a = Account::offline(k, tree);
        let file = a.node("/Drive na nuvem/nota.txt").unwrap();
        let url = a.link_of(&file).unwrap();
        assert_eq!(url, format!("https://mega.nz/file/PUBH0001#{}", b64(&file.key)));
        assert_eq!(a.link_of(&a.node("/Drive na nuvem/Fotos").unwrap()), None);
        // No network here: an existing link comes back without asking.
        assert_eq!(a.create_link(&file).unwrap(), url);
        assert!(a.create_link(&a.node("/Drive na nuvem/Fotos").unwrap()).unwrap_err().contains("site do MEGA"));
        assert!(a.create_link(&a.node("/Lixeira/velho.txt").unwrap()).unwrap_err().contains("Restaure"));
        assert!(a.remove_link(&a.node("/Drive na nuvem/Fotos").unwrap()).unwrap_err().contains("não tem link"));
    }

    #[test]
    fn link_packets_say_made_or_removed() {
        assert_eq!(link_change(&json!({"a": "ph", "h": "N1", "ph": "P1"})), Some(("N1".into(), Some("P1".into()))));
        assert_eq!(link_change(&json!({"a": "ph", "h": "N1", "ph": "P1", "d": 1})), Some(("N1".into(), None)));
    }
}
