//! Drive's change log (`changes.list`): what changed in an account since a point in it, made
//! on the website, a phone, by someone sharing a folder, or by this app. A point is a page
//! token; [`Drive::start_page_token`] gives the current one, and [`Drive::changes`] what
//! changed since a token, with the token to ask from next time.

use openexplorer_i18n::{t, tr};
use serde::Deserialize;

use crate::Result;
use crate::api::{Drive, DriveFile, FILE_FIELDS, RawFile, checked};

/// One file (or shared drive) that changed.
#[derive(Clone, Debug)]
pub struct Change {
    pub file_id: String,
    /// The file as it is now; `None` when it's gone for this account (deleted for good, or no
    /// longer shared with it), and for a change to a shared drive itself.
    pub file: Option<DriveFile>,
    /// The folders holding it now.
    pub parents: Vec<String>,
    pub trashed: bool,
    /// A shared drive was added, renamed or removed: the list of shared drives changed.
    pub drive: bool,
}

/// How many pages of changes one call reads at most (1,000 changes each). Past that the
/// rest waits for the next call, from where this one stopped.
const MOST_PAGES: usize = 20;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawChange {
    change_type: Option<String>,
    #[serde(default)]
    removed: bool,
    file_id: Option<String>,
    drive_id: Option<String>,
    file: Option<RawChangedFile>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawChangedFile {
    #[serde(flatten)]
    file: RawFile,
    #[serde(default)]
    parents: Vec<String>,
    #[serde(default)]
    trashed: bool,
}

impl From<RawChange> for Change {
    fn from(r: RawChange) -> Change {
        let drive = r.change_type.as_deref() == Some("drive");
        let file_id = r.file_id.or(r.drive_id).unwrap_or_default();
        match r.file.filter(|_| !r.removed && !drive) {
            Some(f) => Change { file_id, parents: f.parents, trashed: f.trashed, file: Some(f.file.into()), drive },
            None => Change { file_id, file: None, parents: Vec::new(), trashed: false, drive },
        }
    }
}

impl Drive {
    /// The point the account's change log is at now: [`Drive::changes`] from it gives what
    /// changes after this call.
    pub fn start_page_token(&self) -> Result<String> {
        let v = self.get("/changes/startPageToken", &[("supportsAllDrives", "true"), ("fields", "startPageToken")], t("Início das mudanças", "Start of changes"))?;
        v.get("startPageToken").and_then(|t| t.as_str()).map(str::to_string).ok_or_else(|| t("Início das mudanças: o Google não respondeu o ponto de início.", "Start of changes: Google didn't send the starting point.").into())
    }

    /// What changed since `token`, in My Drive and the shared drives, and the token to ask from
    /// next time. A file changed several times shows once, as it is now.
    pub fn changes(&self, token: &str) -> Result<(Vec<Change>, String)> {
        let fields = format!("nextPageToken,newStartPageToken,changes(changeType,removed,fileId,driveId,file({FILE_FIELDS},parents,trashed))");
        let mut out = Vec::new();
        let mut at = token.to_string();
        for _ in 0..MOST_PAGES {
            let query = [
                ("pageToken", at.as_str()),
                ("pageSize", "1000"),
                ("spaces", "drive"),
                ("includeRemoved", "true"),
                ("includeItemsFromAllDrives", "true"),
                ("supportsAllDrives", "true"),
                ("fields", fields.as_str()),
            ];
            let mut v = self.get("/changes", &query, t("Mudanças no Drive", "Drive changes"))?;
            let page: Vec<RawChange> = serde_json::from_value(v.get_mut("changes").map(serde_json::Value::take).unwrap_or_default())
                .map_err(|e| tr!("Mudanças no Drive: resposta inesperada: {e}", "Drive changes: unexpected answer: {e}"))?;
            out.extend(page.into_iter().map(Change::from).filter(|c| checked(&c.file_id).is_ok()));
            let next = |k: &str| v.get(k).and_then(|t| t.as_str()).map(str::to_string);
            match (next("newStartPageToken"), next("nextPageToken")) {
                (Some(done), _) => return Ok((latest(out), done)),
                (None, Some(more)) => at = more,
                (None, None) => return Err(t("Mudanças no Drive: o Google não respondeu onde continuar.", "Drive changes: Google didn't say where to continue from.").into()),
            }
        }
        // A long backlog: the rest comes on the next call, from here.
        Ok((latest(out), at))
    }
}

/// Each file once, as its last change left it, in the order of those last changes.
fn latest(changes: Vec<Change>) -> Vec<Change> {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<Change> = changes.into_iter().rev().filter(|c| seen.insert((c.file_id.clone(), c.drive))).collect();
    out.reverse();
    out
}

#[cfg(test)]
mod tests {
    use crate::api::tests::stand_in;

    #[test]
    fn changes_page_through_and_keep_each_file_once_as_it_is_now() {
        let page1 = r#"{"nextPageToken":"p2","changes":[
            {"changeType":"file","fileId":"a","file":{"id":"a","name":"velho.txt","mimeType":"text/plain","parents":["root1"]}},
            {"changeType":"file","fileId":"b","removed":true},
            {"changeType":"drive","driveId":"0Adrive"}
        ]}"#;
        let page2 = r#"{"newStartPageToken":"t9","changes":[
            {"changeType":"file","fileId":"a","file":{"id":"a","name":"novo.txt","mimeType":"text/plain","parents":["pasta"],"trashed":true,"starred":true}},
            {"changeType":"file","fileId":"bad/id","file":{"id":"bad/id","name":"x","mimeType":"text/plain"}}
        ]}"#;
        let (d, seen) = stand_in(vec![("200 OK", page1), ("200 OK", page2)]);
        let (changes, next) = d.changes("t1").unwrap();
        assert_eq!(next, "t9");
        let ids: Vec<&str> = changes.iter().map(|c| c.file_id.as_str()).collect();
        assert_eq!(ids, ["b", "0Adrive", "a"]);
        assert!(changes[0].file.is_none() && !changes[0].drive);
        assert!(changes[1].drive && changes[1].file.is_none());
        let a = &changes[2];
        assert_eq!((a.file.as_ref().unwrap().name.as_str(), a.parents.as_slice(), a.trashed), ("novo.txt", ["pasta".to_string()].as_slice(), true));
        assert!(a.file.as_ref().unwrap().starred);
        let seen = seen.join().unwrap();
        assert!(seen[0].starts_with("GET /changes?pageToken=t1&") && seen[0].contains("includeItemsFromAllDrives=true"), "{}", seen[0]);
        assert!(seen[1].starts_with("GET /changes?pageToken=p2&"), "{}", seen[1]);
    }

    #[test]
    fn the_start_token_and_the_root_id_come_from_their_calls() {
        let (d, seen) = stand_in(vec![("200 OK", r#"{"startPageToken":"42"}"#), ("200 OK", r#"{"id":"0AMeuDrive"}"#)]);
        assert_eq!(d.start_page_token().unwrap(), "42");
        assert_eq!(d.root_id().unwrap(), "0AMeuDrive");
        let seen = seen.join().unwrap();
        assert!(seen[0].starts_with("GET /changes/startPageToken?"), "{}", seen[0]);
        assert!(seen[1].starts_with("GET /files/root?"), "{}", seen[1]);
    }
}
