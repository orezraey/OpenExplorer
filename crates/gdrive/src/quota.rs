//! How much of the account's storage is used, from `about.get`.
//! <https://developers.google.com/workspace/drive/api/reference/rest/v3/about>

use crate::{Drive, Result};

/// The account's storage: Drive, Gmail and Photos together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quota {
    /// `None` when the account has no limit (some Workspace plans).
    pub limit: Option<u64>,
    /// Everything counted against the limit.
    pub usage: u64,
    /// Drive's part of `usage`.
    pub in_drive: u64,
    /// What sits in the Drive trash, part of `in_drive`.
    pub in_trash: u64,
}

impl Quota {
    /// What's left, or `None` without a limit. Usage can pass the limit (a plan downgraded):
    /// then nothing is left.
    pub fn free(&self) -> Option<u64> {
        self.limit.map(|l| l.saturating_sub(self.usage))
    }

    /// Reads `about`'s `storageQuota`, whose numbers come as strings (int64 in JSON).
    pub fn parse(about: &serde_json::Value) -> Result<Quota> {
        let q = about.get("storageQuota").ok_or("Espaço da conta: o Google não informou a cota.")?;
        let n = |k: &str| q.get(k).and_then(|v| v.as_str().and_then(|s| s.parse().ok()).or_else(|| v.as_u64()));
        Ok(Quota { limit: n("limit"), usage: n("usage").unwrap_or(0), in_drive: n("usageInDrive").unwrap_or(0), in_trash: n("usageInDriveTrash").unwrap_or(0) })
    }
}

impl Drive {
    pub fn quota(&self) -> Result<Quota> {
        let v = self.call("GET", "/about", &[("fields", "storageQuota")], None, "Espaço da conta")?;
        Quota::parse(&v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_limited_and_an_unlimited_quota() {
        let v = serde_json::json!({"storageQuota": {"limit": "16106127360", "usage": "2791728742", "usageInDrive": "2147483648", "usageInDriveTrash": "1048576"}});
        let q = Quota::parse(&v).unwrap();
        assert_eq!(q, Quota { limit: Some(16_106_127_360), usage: 2_791_728_742, in_drive: 2_147_483_648, in_trash: 1_048_576 });
        assert_eq!(q.free(), Some(16_106_127_360 - 2_791_728_742));

        let unlimited = Quota::parse(&serde_json::json!({"storageQuota": {"usage": "5"}})).unwrap();
        assert_eq!((unlimited.limit, unlimited.usage, unlimited.free()), (None, 5, None));

        let over = Quota::parse(&serde_json::json!({"storageQuota": {"limit": "10", "usage": "12"}})).unwrap();
        assert_eq!(over.free(), Some(0));

        assert!(Quota::parse(&serde_json::json!({})).is_err());
    }
}
