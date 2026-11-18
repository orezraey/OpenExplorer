//! How much of the account's storage is used, from `uq`.

use openexplorer_i18n::t;
use serde_json::{Value, json};

use crate::Result;
use crate::account::Account;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quota {
    /// The plan's storage, in bytes.
    pub total: u64,
    /// Everything counted against it (file versions too).
    pub used: u64,
    /// What sits in the Rubbish Bin, part of `used`.
    pub in_rubbish: u64,
}

impl Quota {
    /// Reads `uq`'s answer: `mstrg` the plan, `cstrg` the use, `cstrgn` the use under each top
    /// folder (its first number the bytes).
    pub fn parse(v: &Value, rubbish: Option<&str>) -> Result<Quota> {
        let total = v.get("mstrg").and_then(Value::as_u64).ok_or(t("Espaço da conta: o MEGA não informou a cota.", "Account storage: MEGA didn't send the quota."))?;
        let used = v.get("cstrg").and_then(Value::as_u64).unwrap_or(0);
        let in_rubbish = rubbish.and_then(|h| v.pointer(&format!("/cstrgn/{h}/0"))).and_then(Value::as_u64).unwrap_or(0);
        Ok(Quota { total, used, in_rubbish })
    }
}

impl Account {
    pub fn quota(&self) -> Result<Quota> {
        let v = self.api.run(json!({"a": "uq", "strg": 1, "xfer": 1}))?;
        let rubbish = self.lock().tree.rubbish.clone();
        Quota::parse(&v, rubbish.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_plan_the_use_and_the_bin() {
        let v = json!({"mstrg": 21_474_836_480u64, "cstrg": 1_073_741_824u64, "cstrgn": {"ROOT0000": [1_000_000_000u64, 10, 2, 0, 0], "BIN00000": [73_741_824u64, 1, 0, 0, 0]}});
        assert_eq!(Quota::parse(&v, Some("BIN00000")).unwrap(), Quota { total: 21_474_836_480, used: 1_073_741_824, in_rubbish: 73_741_824 });
        assert_eq!(Quota::parse(&v, None).unwrap().in_rubbish, 0);
        assert!(Quota::parse(&json!({}), None).is_err());
    }
}
