//! Who can reach a file: its permissions, and the link anyone can open.
//! <https://developers.google.com/workspace/drive/api/reference/rest/v3/permissions>

use serde::Deserialize;

use crate::api::checked;
use crate::{Drive, Result};

/// What a permission lets its holder do, most to least.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Role {
    Owner,
    /// Manager of a shared drive.
    Organizer,
    /// Content manager of a shared drive.
    FileOrganizer,
    Writer,
    Commenter,
    Reader,
    #[serde(other)]
    Unknown,
}

impl Role {
    /// The roles Compartilhar hands out, in its order.
    pub const GRANTED: [Role; 3] = [Role::Reader, Role::Commenter, Role::Writer];

    pub fn api(self) -> &'static str {
        match self {
            Role::Owner => "owner",
            Role::Organizer => "organizer",
            Role::FileOrganizer => "fileOrganizer",
            Role::Writer => "writer",
            Role::Commenter => "commenter",
            Role::Reader | Role::Unknown => "reader",
        }
    }

    /// As the Drive website names it in Portuguese.
    pub fn label(self) -> &'static str {
        match self {
            Role::Owner => "Proprietário",
            Role::Organizer => "Administrador",
            Role::FileOrganizer => "Gerente de conteúdo",
            Role::Writer => "Editor",
            Role::Commenter => "Comentarista",
            Role::Reader => "Leitor",
            Role::Unknown => "Outro",
        }
    }
}

/// Who a permission is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Access {
    User,
    Group,
    /// Everyone in a Workspace domain.
    Domain,
    /// Anyone with the link.
    Anyone,
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Permission {
    pub id: String,
    pub access: Access,
    pub role: Role,
    pub email: Option<String>,
    pub name: Option<String>,
    pub domain: Option<String>,
    /// The account was deleted; the permission stays until removed.
    pub deleted: bool,
    /// Comes from a folder or shared drive above: it can only change there.
    pub inherited: bool,
}

impl Permission {
    /// Who it is, in words: the person's name and e-mail, the domain, or "Qualquer pessoa com o
    /// link".
    pub fn who(&self) -> String {
        match self.access {
            Access::Anyone => "Qualquer pessoa com o link".into(),
            Access::Domain => format!("Qualquer pessoa em {}", self.domain.as_deref().unwrap_or("?")),
            _ => match (&self.name, &self.email) {
                (Some(n), Some(e)) if n != e => format!("{n} <{e}>"),
                (_, Some(e)) => e.clone(),
                (Some(n), None) => n.clone(),
                (None, None) => "(sem nome)".into(),
            },
        }
    }
}

/// A file's sharing as Compartilhar shows it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sharing {
    /// The file's address on the Drive website.
    pub link: Option<String>,
    pub permissions: Vec<Permission>,
    /// Whether this account may change who has access.
    pub can_share: bool,
}

impl Sharing {
    /// The anyone-with-the-link permission, if the link is open.
    pub fn anyone(&self) -> Option<&Permission> {
        self.permissions.iter().find(|p| p.access == Access::Anyone)
    }

    /// A line for Propriedades: "Só você", or who else has access and how, `me` (the account)
    /// left out.
    pub fn summary(&self, me: &str) -> String {
        let others: Vec<String> = self
            .permissions
            .iter()
            .filter(|p| !p.email.as_deref().is_some_and(|e| e.eq_ignore_ascii_case(me)))
            .map(|p| format!("{} ({})", p.who(), p.role.label()))
            .collect();
        if others.is_empty() { "Só você".into() } else { others.join(" · ") }
    }

    /// Reads `files.get` (`webViewLink`, `capabilities.canShare`) and a `permissions.list` page.
    pub fn parse(file: &serde_json::Value, permissions: &serde_json::Value) -> Result<Sharing> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Detail {
            #[serde(default)]
            inherited: bool,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Raw {
            id: String,
            #[serde(rename = "type")]
            access: Access,
            role: Role,
            email_address: Option<String>,
            display_name: Option<String>,
            domain: Option<String>,
            #[serde(default)]
            deleted: bool,
            #[serde(default)]
            permission_details: Vec<Detail>,
        }
        let raw: Vec<Raw> = serde_json::from_value(permissions.get("permissions").cloned().unwrap_or_default())
            .map_err(|e| format!("Compartilhamento: resposta inesperada: {e}"))?;
        let mut list: Vec<Permission> = raw
            .into_iter()
            .map(|r| Permission {
                // Inherited when every grant behind it comes from above (shared drives only).
                inherited: !r.permission_details.is_empty() && r.permission_details.iter().all(|d| d.inherited),
                id: r.id,
                access: r.access,
                role: r.role,
                email: r.email_address,
                name: r.display_name,
                domain: r.domain,
                deleted: r.deleted,
            })
            .collect();
        // The owner first, then people by name, then domains and the link.
        list.sort_by_key(|p| (p.role != Role::Owner, matches!(p.access, Access::Domain | Access::Anyone), p.who().to_lowercase()));
        Ok(Sharing {
            link: file.get("webViewLink").and_then(|v| v.as_str()).map(str::to_string),
            can_share: file.pointer("/capabilities/canShare").and_then(|v| v.as_bool()).unwrap_or(false),
            permissions: list,
        })
    }
}

impl Drive {
    /// Who can reach file `id`, its link, and whether this account can change that.
    pub fn sharing(&self, id: &str) -> Result<Sharing> {
        let path = format!("/files/{}", checked(id)?);
        let file = self.call("GET", &path, &[("fields", "webViewLink,capabilities(canShare)"), ("supportsAllDrives", "true")], None, "Compartilhamento")?;
        let fields = "permissions(id,type,role,emailAddress,displayName,domain,deleted,permissionDetails(inherited))";
        // One page holds up to 100; a file shared with more than that is rare enough to show
        // the first hundred.
        let perms = self.call("GET", &format!("{path}/permissions"), &[("fields", fields), ("pageSize", "100"), ("supportsAllDrives", "true")], None, "Compartilhamento")?;
        Sharing::parse(&file, &perms)
    }

    /// Gives `email` (a person or a Google group) `role` on file `id`. With `notify`, Google
    /// e-mails them the invitation.
    pub fn share_with(&self, id: &str, email: &str, role: Role, notify: bool) -> Result<()> {
        let email = email.trim();
        if !valid_email(email) {
            return Err(format!("E-mail inválido: {email}"));
        }
        let body = serde_json::json!({ "type": "user", "role": role.api(), "emailAddress": email });
        let notify = if notify { "true" } else { "false" };
        self.call("POST", &format!("/files/{}/permissions", checked(id)?), &[("sendNotificationEmail", notify), ("supportsAllDrives", "true"), ("fields", "id")], Some(&body), "Compartilhar")?;
        Ok(())
    }

    pub fn set_role(&self, id: &str, permission: &str, role: Role) -> Result<()> {
        let path = format!("/files/{}/permissions/{}", checked(id)?, checked(permission)?);
        self.call("PATCH", &path, &[("supportsAllDrives", "true"), ("fields", "id")], Some(&serde_json::json!({ "role": role.api() })), "Mudar o acesso")?;
        Ok(())
    }

    pub fn unshare(&self, id: &str, permission: &str) -> Result<()> {
        let path = format!("/files/{}/permissions/{}", checked(id)?, checked(permission)?);
        self.call("DELETE", &path, &[("supportsAllDrives", "true")], None, "Remover o acesso")?;
        Ok(())
    }

    /// Opens file `id` to anyone with its link, as a reader (not listed in searches), or closes
    /// it again by removing every anyone-with-the-link permission.
    pub fn set_link_access(&self, id: &str, on: bool) -> Result<()> {
        if on {
            let body = serde_json::json!({ "type": "anyone", "role": "reader", "allowFileDiscovery": false });
            self.call("POST", &format!("/files/{}/permissions", checked(id)?), &[("supportsAllDrives", "true"), ("fields", "id")], Some(&body), "Ativar o link")?;
            return Ok(());
        }
        let current = self.sharing(id)?;
        for p in current.permissions.iter().filter(|p| p.access == Access::Anyone) {
            self.unshare(id, &p.id)?;
        }
        Ok(())
    }
}

/// Loosely, one address: something@domain.tld, no spaces. Google checks the rest.
pub fn valid_email(email: &str) -> bool {
    let Some((user, domain)) = email.split_once('@') else { return false };
    !user.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.') && !email.chars().any(|c| c.is_whitespace() || c == ',' || c == ';') && !domain.contains('@')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Sharing {
        let file = serde_json::json!({"webViewLink": "https://drive.google.com/file/d/abc/view?usp=drivesdk", "capabilities": {"canShare": true}});
        let perms = serde_json::json!({"permissions": [
            {"id": "anyoneWithLink", "type": "anyone", "role": "reader"},
            {"id": "111", "type": "user", "role": "writer", "emailAddress": "ana@exemplo.com", "displayName": "Ana"},
            {"id": "222", "type": "user", "role": "owner", "emailAddress": "voce@gmail.com", "displayName": "Você"},
            {"id": "333", "type": "group", "role": "commenter", "emailAddress": "time@exemplo.com"},
            {"id": "444", "type": "domain", "role": "reader", "domain": "exemplo.com"},
            {"id": "555", "type": "user", "role": "organizer", "emailAddress": "chefe@exemplo.com", "permissionDetails": [{"permissionType": "member", "role": "organizer", "inherited": true, "inheritedFrom": "0AA"}]},
            {"id": "666", "type": "user", "role": "pendingOwner", "emailAddress": "novo@exemplo.com"}
        ]});
        Sharing::parse(&file, &perms).unwrap()
    }

    #[test]
    fn reads_permissions_owner_first_and_the_link_last() {
        let s = sample();
        assert!(s.can_share);
        assert_eq!(s.link.as_deref(), Some("https://drive.google.com/file/d/abc/view?usp=drivesdk"));
        let order: Vec<&str> = s.permissions.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(order, ["222", "111", "555", "666", "333", "anyoneWithLink", "444"]);
        let ana = &s.permissions[1];
        assert_eq!((ana.access, ana.role, ana.who().as_str(), ana.inherited), (Access::User, Role::Writer, "Ana <ana@exemplo.com>", false));
        assert!(s.permissions.iter().find(|p| p.id == "555").unwrap().inherited);
        assert_eq!(s.permissions.iter().find(|p| p.id == "666").unwrap().role, Role::Unknown);
        assert_eq!(s.anyone().map(|p| p.id.as_str()), Some("anyoneWithLink"));
        assert_eq!(s.permissions.last().unwrap().who(), "Qualquer pessoa em exemplo.com");
    }

    #[test]
    fn summary_leaves_the_account_out() {
        let only_me = Sharing::parse(&serde_json::json!({}), &serde_json::json!({"permissions": [{"id": "1", "type": "user", "role": "owner", "emailAddress": "Voce@gmail.com"}]})).unwrap();
        assert!(!only_me.can_share && only_me.link.is_none());
        assert_eq!(only_me.summary("voce@gmail.com"), "Só você");
        let s = sample();
        let line = s.summary("voce@gmail.com");
        assert!(line.starts_with("Ana <ana@exemplo.com> (Editor) · "), "{line}");
        assert!(line.contains("Qualquer pessoa com o link (Leitor)") && !line.contains("Você"), "{line}");
    }

    #[test]
    fn emails_are_checked_loosely() {
        assert!(valid_email("ana@exemplo.com"));
        assert!(valid_email("a.b+c@sub.exemplo.com.br"));
        for bad in ["", "ana", "ana@", "@x.com", "ana@x", "ana @x.com", "a@b.com,c@d.com", "a@.com", "a@b@c.com"] {
            assert!(!valid_email(bad), "{bad}");
        }
    }

    #[test]
    fn roles_round_trip_to_the_api() {
        for r in Role::GRANTED {
            let back: Role = serde_json::from_value(serde_json::Value::String(r.api().into())).unwrap();
            assert_eq!(back, r);
        }
        assert_eq!(Role::GRANTED.map(Role::label), ["Leitor", "Comentarista", "Editor"]);
    }
}
